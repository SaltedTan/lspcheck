// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! One run of the server under test: its pipes, a transcript of every byte,
//! and the messages and stream problems seen so far.
//!
//! Stdout and stderr are read by their own tasks for the whole session, so a
//! server that writes a lot can never block on a full pipe while lspcheck is
//! writing to it. The stdout task also parses frames and records framing
//! evidence itself, so that evidence is complete even if nobody consumes the
//! messages. Every write and every wait has an absolute deadline. Stdout is
//! inspected up to an output budget; past it, lspcheck keeps reading and
//! discarding so the server isn't blocked, and records that its evidence is
//! incomplete.

use crate::message::{self, Id, Kind, Message, codes};
use crate::process::{self, Coverage, Found, Group, ProcInfo, ServerCommand, Unconfirmed};
use crate::transport::{self, Chunk, FrameReader, Header};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep, sleep_until, timeout_at};

/// Session settings.
#[derive(Debug, Clone)]
pub struct Options {
    /// The executable to run as the process-group sentinel (normally lspcheck itself).
    pub sentinel: Option<PathBuf>,
    /// The longest any single write may take.
    pub write_timeout: Duration,
    /// The longest header section inspected.
    pub max_header: usize,
    /// The largest frame body inspected.
    pub max_body: usize,
    /// Stdout bytes inspected and kept per session.
    pub max_output: usize,
    /// Stderr bytes kept in the transcript; the rest are counted.
    pub stderr_limit: usize,
    /// Answer the server's own requests (configuration, registration, progress).
    pub auto_respond: bool,
    /// The answer to `workspace/workspaceFolders`.
    pub workspace_folders: Value,
    /// The settings `workspace/configuration` is answered from (a profile's
    /// `[client].configuration`); without them, each item gets `null`.
    pub configuration: Option<Value>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            sentinel: None,
            write_timeout: Duration::from_secs(10),
            max_header: transport::DEFAULT_MAX_HEADER_BYTES,
            max_body: transport::DEFAULT_MAX_BODY_BYTES,
            max_output: 256 * 1024 * 1024,
            stderr_limit: 1024 * 1024,
            auto_respond: true,
            workspace_folders: Value::Null,
            configuration: None,
        }
    }
}

/// One transcript entry.
#[derive(Debug, Clone)]
pub struct Event {
    /// Time since the session started.
    pub at: Duration,
    /// When the event began relative to every other, so that a transcript
    /// can be put back in the order things happened: a write is appended only
    /// once it has flushed, by which time the reader may have appended the
    /// reply, possibly with the same timestamp. Taken from [`next_seq`].
    pub seq: u64,
    pub kind: EventKind,
}

/// The next event sequence number. Process-wide, so it only ever grows.
pub fn next_seq() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

#[derive(Debug, Clone)]
pub enum EventKind {
    /// Bytes that reached the server's stdin.
    Sent(Vec<u8>),
    /// Bytes exactly as read from the server's stdout.
    Read(Vec<u8>),
    /// How the frame reader split the bytes read so far.
    Parsed {
        kind: &'static str,
        detail: String,
    },
    Stderr(Vec<u8>),
    StdoutClosed,
    StdinClosed,
    Exited(String),
    Note(String),
    /// The workspace the server runs in and the files in it, so a transcript
    /// can be replayed in a recreated workspace.
    Workspace {
        path: PathBuf,
        files: Vec<(String, Vec<u8>)>,
        directories: Vec<String>,
        file_modes: std::collections::BTreeMap<String, u32>,
    },
    /// The settings `workspace/configuration` is answered from, so a replay
    /// answers the same way.
    Configuration(Value),
    /// The plan of a mutation case, recorded before it is sent. Replay
    /// ignores it.
    Mutation(Value),
}

/// Something wrong with the server's output stream.
#[derive(Debug, Clone, PartialEq)]
pub enum Problem {
    /// Bytes outside any frame.
    Garbage(Vec<u8>),
    BadHeader {
        block: Vec<u8>,
        reason: String,
    },
    /// A frame whose body isn't JSON.
    NotJson {
        body: Vec<u8>,
        error: String,
    },
    /// The stream ended inside a frame.
    Truncated {
        content_length: usize,
        received: usize,
    },
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Problem::Garbage(b) => write!(f, "{} bytes outside any frame: {}", b.len(), preview(b)),
            Problem::BadHeader { block, reason } => write!(f, "bad header ({reason}): {}", preview(block)),
            Problem::NotJson { body, error } => write!(f, "frame body is not JSON ({error}): {}", preview(body)),
            Problem::Truncated { content_length, received } => {
                write!(f, "stream ended {received} bytes into a {content_length}-byte body")
            }
        }
    }
}

/// The answer to one `workspace/configuration` item: its `section` looked up
/// as a dot-separated path into `settings`, `null` where the path leads
/// nowhere. An item without a section (or with an empty one) gets all of it.
pub fn configuration_item(settings: &Value, item: &Value) -> Value {
    let section = match item.get("section") {
        None | Some(Value::Null) => return settings.clone(),
        Some(Value::String(s)) if s.is_empty() => return settings.clone(),
        Some(Value::String(s)) => s,
        Some(_) => return Value::Null,
    };
    section.split('.').try_fold(settings, |v, key| v.get(key)).cloned().unwrap_or(Value::Null)
}

/// A short, escaped rendering of some bytes.
pub fn preview(bytes: &[u8]) -> String {
    const MAX: usize = 80;
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX)]);
    let more = if bytes.len() > MAX { "…" } else { "" };
    format!("{text:?}{more}")
}

/// What one receive produced.
#[derive(Debug)]
pub enum Recv {
    Message(Message),
    Problem(Problem),
    /// Stdout has closed, or lspcheck stopped inspecting it.
    Closed,
    Timeout,
}

/// Why a wait for a response ended without one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitError {
    Timeout,
    Closed,
}

/// How L6 and L7's wait for the server to exit after `exit` ended (see
/// [`Session::serve_after_exit`]). Times are since the session started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitWait {
    /// lspcheck saw it exit by the deadline.
    Exited(ExitStatus),
    /// lspcheck saw it still running a whole timeout after it went quiet.
    Running(Quiet),
    /// lspcheck was still busy with the server's output at `at`, past the
    /// deadline, doing `what`, so it can't see the server quiet for a whole
    /// timeout by twice the timeout. It stopped waiting then.
    Busy { at: Duration, what: String },
    /// lspcheck saw that it had exited only after the deadline, and last saw
    /// it running at `running`, if it did.
    Unseen { running: Option<Duration>, exited: Duration, status: ExitStatus },
    /// lspcheck couldn't look at the process.
    Unknown,
}

/// What L6 and L7's wait saw of a server still running after `exit`. Times
/// are since the session started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quiet {
    /// When lspcheck began waiting, having written `exit`.
    pub sent: Duration,
    /// The deadline: `sent` and the exit timeout.
    pub due: Duration,
    /// Q: when lspcheck last went back to waiting with nothing of the
    /// server's left to handle, and what it had just done then, if it had
    /// done anything since `exit`.
    pub since: Duration,
    pub after: Option<String>,
    /// When lspcheck looked and saw it running: a whole timeout or more after
    /// `since`, and after its last read of the server's stdout or stderr.
    pub seen: Duration,
}

/// Why a write failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteError {
    /// Stdin was closed, by lspcheck or by the server exiting.
    Closed,
    Timeout,
    Io(String),
    /// A request with this id is still waiting for its response.
    DuplicateId(String),
    /// An earlier write stopped partway through a frame, so anything sent
    /// now would land inside it; lspcheck sends nothing more.
    Abandoned,
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::Closed => f.write_str("the server's stdin is closed"),
            WriteError::Timeout => f.write_str("the server stopped reading its stdin"),
            WriteError::Io(e) => write!(f, "writing to the server failed: {e}"),
            WriteError::DuplicateId(id) => write!(f, "a request with id {id} is still outstanding"),
            WriteError::Abandoned => f.write_str("an earlier write to the server stopped partway through a frame"),
        }
    }
}

/// Why lspcheck may have missed something the server sent between `from`
/// and `to`: bytes it skipped or couldn't read as a frame (see
/// [`Evidence::lost`]) that arrived over a span overlapping that time, or,
/// from any time up to `to`, `ongoing`: reading that stopped, or a server
/// request lspcheck couldn't answer, after which the server may have held
/// back what it would have sent.
fn incomplete_between(
    lost: &[(Duration, Duration, String)],
    ongoing: [Option<&(Duration, String)>; 2],
    from: Duration,
    to: Duration,
) -> Option<String> {
    let lost = lost.iter().find(|(began, ended, _)| *began <= to && *ended >= from).map(|(_, _, why)| why.clone());
    let since =
        ongoing.into_iter().flatten().filter(|(at, _)| *at <= to).min_by_key(|(at, _)| *at).map(|(_, why)| why.clone());
    lost.or(since)
}

/// What [`Evidence::lost`] says about bytes that couldn't be read as a frame.
const UNREAD: &str = "some of the server's frames couldn't be read (see F1)";

/// What the stdout task observed, independently of what was consumed.
#[derive(Debug, Default)]
struct Evidence {
    problems: Vec<(Duration, Problem)>,
    content_types: Vec<(Duration, String)>,
    limit: Option<(Duration, String)>,
    /// Where content was lost: every frame skipped for its size, and every
    /// stretch of bytes that couldn't be read as a frame (a log line loses
    /// nothing, so it isn't one). Each spans from when its first byte arrived
    /// (or earlier) until it was complete, since a frame that turns out to be
    /// too long or malformed may have swallowed anything sent meanwhile.
    lost: Vec<(Duration, Duration, String)>,
    /// When the bytes the frame reader holds unfinished began arriving, if it
    /// holds any.
    partial_since: Option<Duration>,
    /// When and why lspcheck stopped reading stdout, if it did.
    stopped: Option<(Duration, String)>,
    /// When lspcheck last read bytes from stdout, inspected or not.
    last_read: Option<Duration>,
    /// When the first readable frame with non-ASCII text in its body arrived.
    non_ascii: Option<Duration>,
    /// When lspcheck began signalling processes that may hold stdout: an
    /// end of the stream seen from then on may be lspcheck's doing.
    signalled: Option<Duration>,
    /// A frame the stream was seen to end inside after lspcheck began
    /// signalling: when, and how much of it came. Too long a Content-Length
    /// looks the same, so it is an inspection limit, not a problem.
    cut_short: Option<(Duration, String)>,
    /// Frames whose body isn't JSON: when, the Content-Length, and where the body starts.
    unparsed: Vec<(Duration, usize, usize)>,
    /// lspcheck stopped reading stdout into the transcript before it ended.
    cut: bool,
    /// The stdout task has seen the end of the stream.
    done: bool,
}

/// A request the server sent lspcheck, and how lspcheck answered it.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerRequest {
    /// When it was read.
    pub at: Duration,
    pub id: Id,
    pub method: String,
    /// When lspcheck began writing its answer, if it did. Until then the
    /// server can't have had the answer.
    pub answer_began: Option<Duration>,
    /// When lspcheck finished writing the whole answer, if it did.
    pub answered_at: Option<Duration>,
}

/// A response that matched no outstanding request, with what explains it.
#[derive(Debug, Clone)]
pub struct Unmatched {
    /// When it was read.
    pub at: Duration,
    pub id: Id,
    /// A request with this id had already been answered.
    pub duplicate: bool,
    /// lspcheck never sent a request with this id in this session.
    pub unsent: bool,
    /// The request with this id was registered only after this response was read.
    pub early: bool,
    /// The request's write failed, so the server may have seen part of it.
    pub aborted: bool,
    /// An id lspcheck sent that is only loosely equal to this one (a number
    /// spelled as a string, or a number equal only after rounding).
    pub loose: Option<Id>,
}

/// The note recorded when lspcheck starts stopping a server that is still
/// running; what the server sends after it is a reaction to being stopped.
pub const STOPPING_NOTE: &str = "lspcheck is stopping the server";

/// What remains when a session ends.
#[derive(Debug)]
pub struct Finish {
    /// The server's exit status, when it exited before lspcheck had to stop it.
    pub exit: Option<ExitStatus>,
    /// Whether lspcheck had to stop the server.
    pub killed: bool,
    /// Descendants still alive after the grace period, before lspcheck stopped them.
    pub leftovers: Vec<ProcInfo>,
    /// Processes found then only through parent links lspcheck couldn't
    /// confirm: they may not have been the server's.
    pub unconfirmed: Option<Unconfirmed>,
    /// What the leftover search could see.
    pub coverage: Coverage,
    /// Why the leftover search failed, if it did.
    pub scan_error: Option<String>,
    /// Processes lspcheck couldn't stop.
    pub not_stopped: Vec<ProcInfo>,
    /// When and why lspcheck's inspection of stdout became incomplete, if it did.
    pub limit: Option<(Duration, String)>,
    /// Why lspcheck's own side of the conversation was incomplete (it
    /// couldn't answer one of the server's requests), if it was. This can
    /// explain a reply that never came, but not what stdout contained.
    pub own_limit: Option<(Duration, String)>,
    /// Where content was lost: frames skipped for their size and bytes that
    /// couldn't be read as a frame, each from when its first byte arrived
    /// until it was complete (or the session ended).
    pub lost: Vec<(Duration, Duration, String)>,
    /// When and why lspcheck stopped reading stdout, if it did.
    pub stopped: Option<(Duration, String)>,
    /// When lspcheck last read bytes from stdout, inspected or not, up to the
    /// end of the session.
    pub last_read: Option<Duration>,
    pub transcript: Vec<Event>,
    /// Every message the session consumed.
    pub messages: Vec<(Duration, Message)>,
    /// When lspcheck sent `initialized`, if it did.
    pub initialized_at: Option<Duration>,
    /// Why evidence since `initialized` is incomplete, if it is.
    /// A response without an id read in that window could have been skipped.
    pub j3_incomplete: Option<String>,
    /// When a write of lspcheck's that stopped partway through a frame began.
    pub abandoned_at: Option<Duration>,
    /// Every stream problem in the whole of stdout.
    pub problems: Vec<(Duration, Problem)>,
    /// A frame still unfinished when the stream was seen to end after
    /// lspcheck began stopping the server's processes, with when and how much
    /// of it came. It isn't among `problems`: lspcheck may have cut it short
    /// itself.
    pub cut_short: Option<(Duration, String)>,
    /// Responses that matched no outstanding request, with what explains them.
    pub unmatched: Vec<Unmatched>,
    /// Requests that were answered, with when their response was read.
    pub answered: Vec<(Duration, Id)>,
    answers: HashMap<Id, usize>,
    /// The method of the request each matched response answered, by the
    /// response's index in `messages`.
    pub answered_methods: HashMap<usize, String>,
    /// Requests lspcheck sent whole, with when each was registered (just
    /// before it was written).
    pub sent: Vec<(Duration, Id)>,
    /// The server's requests, in the order they were read.
    pub server_requests: Vec<ServerRequest>,
    /// `Content-Type` values the server sent.
    pub content_types: Vec<(Duration, String)>,
    /// When the first readable frame with non-ASCII text in its body arrived.
    pub non_ascii: Option<Duration>,
    /// Frames whose Content-Length wasn't the length in bytes of the JSON
    /// body that followed, which had non-ASCII text: when, and how.
    pub miscounted: Vec<(Duration, String)>,
    /// How many unreadable frames weren't checked for that, because the
    /// work allowed for it ran out.
    pub unchecked: usize,
}

impl Finish {
    pub fn answer_to(&self, id: &Id) -> Option<&Message> {
        self.answers.get(id).map(|&i| &self.messages[i].1)
    }

    /// Why lspcheck may have missed something the server sent between `from`
    /// and `to`, judged over the whole session (see [`Session::incomplete_within`]).
    pub fn incomplete_within(&self, from: Duration, to: Duration) -> Option<String> {
        incomplete_between(&self.lost, [self.stopped.as_ref(), self.own_limit.as_ref()], from, to)
    }

    pub fn missed_output_within(&self, from: Duration, to: Duration) -> Option<String> {
        incomplete_between(&self.lost, [self.stopped.as_ref(), None], from, to)
    }
}

type Transcript = Arc<Mutex<Vec<Event>>>;

fn record(t: &Transcript, at: Duration, kind: EventKind) {
    record_began(t, at, next_seq(), kind);
}

/// Records an event that began at `at` and was given `seq` then.
fn record_began(t: &Transcript, at: Duration, seq: u64, kind: EventKind) {
    t.lock().expect("transcript lock").push(Event { at, seq, kind });
}

enum Inbound {
    Message(Message),
    Problem(Problem),
    /// The output budget ran out or the reader stopped; nothing more will come.
    Stopped,
    Closed,
}

/// Unregisters a session's processes even if it is dropped without `finish`.
struct Registration(Vec<i32>);

impl Drop for Registration {
    fn drop(&mut self) {
        process::session_ended(&self.0);
    }
}

/// A running server.
pub struct Session {
    child: Child,
    pid: i32,
    stdin: Option<ChildStdin>,
    rx: mpsc::UnboundedReceiver<(Duration, Inbound)>,
    stdout_closed: bool,
    start: Instant,
    transcript: Transcript,
    evidence: Arc<Mutex<Evidence>>,
    /// When lspcheck last read bytes from stderr, if it has.
    stderr_read: Arc<Mutex<Option<Duration>>>,
    /// Every message consumed, in order.
    pub messages: Vec<(Duration, Message)>,
    /// When lspcheck sent `initialized`, if it did.
    initialized_at: Option<Duration>,
    /// When and why this session's own actions left evidence incomplete.
    own_limit: Option<(Duration, String)>,
    /// When a write that stopped partway through a frame began: from then on
    /// the server may lawfully answer malformed input with errors whose id
    /// is null.
    abandoned: Option<Duration>,
    /// Requests sent and not yet answered, with when they were sent.
    pending: HashMap<Id, Duration>,
    /// Requests answered: the index of the response in `messages`.
    answers: HashMap<Id, usize>,
    methods: HashMap<Id, String>,
    /// The method of the request each matched response answered, by the
    /// response's index in `messages`.
    answered_methods: HashMap<usize, String>,
    /// Every id a request was sent with, and when it was registered.
    used_ids: HashMap<Id, Duration>,
    /// Ids handed out by [`Session::fresh_id`], sent or not.
    reserved: HashSet<Id>,
    /// Ids whose write failed before the whole request reached the server.
    aborted: HashSet<Id>,
    /// Requests that were answered, with when the response was read.
    answered: Vec<(Duration, Id)>,
    unmatched: Vec<usize>,
    /// The server's requests, in the order they were read.
    server_requests: Vec<ServerRequest>,
    group: Group,
    exit: Option<ExitStatus>,
    /// When lspcheck first saw that the server had exited. The exit came no
    /// later than this.
    exit_seen: Option<Instant>,
    /// The last time lspcheck looked and found the server still running.
    running_seen: Option<Instant>,
    opts: Options,
    /// Bytes that reached the server's stdin.
    written: u64,
    /// The cap automatic answers are held to, with the part of it kept for
    /// writes lspcheck must still make (see [`Session::cap_answers`]).
    answer_cap: Option<(u64, u64)>,
    /// When the first server request lspcheck left unanswered for that cap
    /// was read, and why.
    withheld: Option<(Duration, String)>,
    _registration: Registration,
}

impl Session {
    /// Starts the server with piped stdio in a process group of its own.
    pub async fn start(cmd: &ServerCommand, opts: Options) -> std::io::Result<Session> {
        let group = Group::start(opts.sentinel.as_deref())?;
        let mut c = Command::new(&cmd.program);
        c.args(&cmd.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(dir) = &cmd.cwd {
            c.current_dir(dir);
        }
        for (k, v) in &cmd.env {
            c.env(k, v);
        }
        #[cfg(unix)]
        c.process_group(group.as_ref().map_or(0, |g| g.pgid));
        let mut child = c.spawn()?;
        let pid = child.id().map_or(-1, |p| p as i32);
        let group = group.unwrap_or_else(|| Group::led_by(pid));
        let mut own = group.own.clone();
        own.push(pid);
        process::session_started(&own);
        let registration = Registration(own);
        let start = Instant::now();
        let transcript: Transcript = Arc::default();
        let evidence: Arc<Mutex<Evidence>> = Arc::default();

        let (tx, rx) = mpsc::unbounded_channel();
        let stdout = child.stdout.take().expect("piped stdout");
        tokio::spawn(read_stdout(stdout, tx, transcript.clone(), evidence.clone(), start, opts.clone()));

        let mut stderr = child.stderr.take().expect("piped stderr");
        let t = transcript.clone();
        let limit = opts.stderr_limit;
        let stderr_read: Arc<Mutex<Option<Duration>>> = Arc::default();
        let read = stderr_read.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 16 * 1024];
            let (mut kept, mut dropped) = (0usize, 0usize);
            loop {
                match stderr.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let at = start.elapsed();
                        *read.lock().expect("stderr lock") = Some(at);
                        let keep = n.min(limit.saturating_sub(kept));
                        if keep > 0 {
                            record(&t, at, EventKind::Stderr(buf[..keep].to_vec()));
                        }
                        kept += keep;
                        dropped += n - keep;
                    }
                }
            }
            if dropped > 0 {
                record(&t, start.elapsed(), EventKind::Note(format!("{dropped} more stderr bytes not kept")));
            }
        });

        Ok(Session {
            stdin: child.stdin.take(),
            child,
            pid,
            rx,
            stdout_closed: false,
            start,
            transcript,
            evidence,
            stderr_read,
            messages: Vec::new(),
            initialized_at: None,
            own_limit: None,
            abandoned: None,
            pending: HashMap::new(),
            answers: HashMap::new(),
            methods: HashMap::new(),
            answered_methods: HashMap::new(),
            used_ids: HashMap::new(),
            reserved: HashSet::new(),
            aborted: HashSet::new(),
            answered: Vec::new(),
            unmatched: Vec::new(),
            server_requests: Vec::new(),
            group,
            exit: None,
            exit_seen: None,
            running_seen: None,
            opts,
            written: 0,
            answer_cap: None,
            withheld: None,
            _registration: registration,
        })
    }

    pub fn pid(&self) -> i32 {
        self.pid
    }

    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    pub fn note(&self, text: impl Into<String>) {
        record(&self.transcript, self.start.elapsed(), EventKind::Note(text.into()));
    }

    /// Records the workspace and the files in it, for [`crate::replay`].
    pub fn record_workspace(&self, path: PathBuf, files: Vec<(String, Vec<u8>)>, directories: Vec<String>) {
        #[cfg(unix)]
        let file_modes = {
            use std::os::unix::fs::PermissionsExt;
            files
                .iter()
                .filter_map(|(name, _)| {
                    std::fs::metadata(path.join(name))
                        .ok()
                        .map(|metadata| (name.clone(), metadata.permissions().mode() & 0o777))
                })
                .collect()
        };
        #[cfg(not(unix))]
        let file_modes = std::collections::BTreeMap::new();
        record(&self.transcript, self.start.elapsed(), EventKind::Workspace { path, files, directories, file_modes });
    }

    /// Records the settings `workspace/configuration` is answered from, for [`crate::replay`].
    pub fn record_configuration(&self, settings: Value) {
        record(&self.transcript, self.start.elapsed(), EventKind::Configuration(settings));
    }

    /// Records the plan of a mutation case.
    pub fn record_mutation(&self, plan: Value) {
        record(&self.transcript, self.start.elapsed(), EventKind::Mutation(plan));
    }

    /// How many bytes have reached the server's stdin, answers to its
    /// requests included.
    pub fn bytes_written(&self) -> u64 {
        self.written
    }

    /// Holds automatic answers to the server's requests to `cap` bytes
    /// written in all, keeping `reserve` of it for writes lspcheck must still
    /// make. An answer that doesn't fit isn't written (see
    /// [`Session::answer_withheld`]). The caller sets `reserve` again as what
    /// it must still write changes.
    pub fn cap_answers(&mut self, cap: u64, reserve: u64) {
        self.answer_cap = Some((cap, reserve));
    }

    /// Whether `bytes` more fit under the answer cap beside its reserve;
    /// anything fits without one.
    pub fn fits(&self, bytes: u64) -> bool {
        self.answer_cap.is_none_or(|(cap, reserve)| self.written + bytes + reserve <= cap)
    }

    /// When the first server request left unanswered for the answer cap was
    /// read, and why, if one was.
    pub fn answer_withheld(&self) -> Option<&(Duration, String)> {
        self.withheld.as_ref()
    }

    /// When and why lspcheck stopped reading the server's stdout (its output
    /// budget ran out), if it did. Stdout reported closed without this was
    /// closed by the server.
    pub fn stopped_reading(&self) -> Option<(Duration, String)> {
        self.evidence.lock().expect("evidence lock").stopped.clone()
    }

    /// Takes in what has already been read, answering the server's requests,
    /// and says whether its stdout has closed (or lspcheck stopped reading it).
    pub async fn output_closed(&mut self) -> bool {
        self.absorb_all().await;
        self.stdout_closed
    }

    /// Drains already-read output within the write timeout. False means
    /// queued output remains, so a missing response is not established.
    pub async fn absorb_all(&mut self) -> bool {
        self.absorb(Instant::now() + self.opts.write_timeout).await
    }

    pub fn stdout_closed(&self) -> bool {
        self.stdout_closed
    }

    pub fn answered_at(&self, id: &Id) -> Option<Duration> {
        self.answers.get(id).map(|&i| self.messages[i].0)
    }

    pub fn answer_to(&self, id: &Id) -> Option<&Message> {
        self.answers.get(id).map(|&i| &self.messages[i].1)
    }

    /// Why evidence is incomplete so far, if it is: an inspection limit,
    /// frames that couldn't be read, or lspcheck's own side of the
    /// conversation. Any of these could explain a response that didn't come.
    pub fn incomplete(&self) -> Option<String> {
        let ev = self.evidence.lock().expect("evidence lock");
        let unread = ev.problems.iter().any(|(_, p)| !matches!(p, Problem::Garbage(_)));
        ev.limit
            .as_ref()
            .or(self.own_limit.as_ref())
            .map(|(_, why)| why.clone())
            .or_else(|| unread.then(|| "some of the server's frames couldn't be read (see F1)".to_string()))
    }

    /// Why lspcheck may have missed something the server sent between `from`
    /// and `to`, if it may have: a frame in that time it skipped or couldn't
    /// read, or, from any time before `to`, reading that stopped or a server
    /// request lspcheck couldn't answer (see [`Session::incomplete`]).
    pub fn incomplete_within(&self, from: Duration, to: Duration) -> Option<String> {
        let ev = self.evidence.lock().expect("evidence lock");
        incomplete_between(&ev.lost, [ev.stopped.as_ref(), self.own_limit.as_ref()], from, to)
    }

    /// Why lspcheck may have missed some of what the server wrote to stdout
    /// between `from` and `to`, if it may have.
    pub fn missed_output_within(&self, from: Duration, to: Duration) -> Option<String> {
        let ev = self.evidence.lock().expect("evidence lock");
        incomplete_between(&ev.lost, [ev.stopped.as_ref(), None], from, to)
    }

    /// Records everything the server has sent that was already read, without
    /// waiting for more and without answering its requests (for when
    /// lspcheck is done talking: an answer that can't be written mustn't keep
    /// the rest from being recorded). An unanswered request makes the
    /// evidence incomplete from when it was read.
    pub fn take_in(&mut self) {
        self.take_queued(true);
    }

    /// Records everything already in the channel without waiting. With
    /// `mark_requests`, a server request lspcheck never answered also makes the
    /// evidence incomplete from when it was read (as `take_in` does).
    fn take_queued(&mut self, mark_requests: bool) {
        while let Ok((at, inbound)) = self.rx.try_recv() {
            match inbound {
                Inbound::Message(m) => {
                    // A server waiting for this answer may be holding back
                    // what it would have sent next.
                    if mark_requests
                        && let Kind::Request { method, .. } = &m.kind
                        && self.own_limit.as_ref().is_none_or(|(began, _)| at < *began)
                    {
                        self.own_limit = Some((at, format!("lspcheck didn't answer the server's {method} request")));
                    }
                    self.accept(at, &m)
                }
                Inbound::Closed | Inbound::Stopped => self.stdout_closed = true,
                Inbound::Problem(_) => {}
            }
        }
    }

    /// Writes bytes exactly as given, within the write timeout.
    pub async fn write_raw(&mut self, bytes: &[u8]) -> Result<(), WriteError> {
        self.write_until(bytes, Instant::now() + self.opts.write_timeout).await
    }

    /// Writes bytes by `deadline` (or the write timeout, if sooner), recording
    /// exactly how many reached the pipe.
    async fn write_until(&mut self, bytes: &[u8], deadline: Instant) -> Result<(), WriteError> {
        self.write_split(bytes, &[], Duration::ZERO, deadline).await
    }

    /// Writes bytes as separate writes cut at `cuts` (ascending offsets into
    /// `bytes`), flushing each piece and pausing `pause` before the next, so a
    /// server reading promptly sees them one at a time. Everything happens by
    /// `deadline` (or the write timeout, if sooner), and each piece is recorded
    /// as it reached the pipe. `bytes` should hold whole frames: if the write
    /// stops partway through, lspcheck sends nothing more.
    pub async fn write_split(
        &mut self,
        bytes: &[u8],
        cuts: &[usize],
        pause: Duration,
        deadline: Instant,
    ) -> Result<(), WriteError> {
        self.write_pieces(bytes, cuts, pause, deadline).await.map(drop)
    }

    /// [`Session::write_split`], returning when each piece began: nothing the
    /// server sent before then can be about that piece.
    pub async fn write_pieces(
        &mut self,
        bytes: &[u8],
        cuts: &[usize],
        pause: Duration,
        deadline: Instant,
    ) -> Result<Vec<Duration>, WriteError> {
        let deadline = deadline.min(Instant::now() + self.opts.write_timeout);
        let began = self.start.elapsed();
        if self.abandoned.is_some() {
            return Err(WriteError::Abandoned);
        }
        let Some(stdin) = self.stdin.as_mut() else { return Err(WriteError::Closed) };
        let (t, start, child) = (&self.transcript, self.start, &mut self.child);
        let ends: Vec<usize> =
            cuts.iter().copied().filter(|&c| c > 0 && c < bytes.len()).chain([bytes.len()]).collect();
        // The piece being written: where it starts and when it began.
        let (mut written, mut piece, mut piece_at, mut piece_seq) = (0usize, 0usize, start.elapsed(), next_seq());
        let mut pieces = Vec::with_capacity(ends.len());
        // A write can last until its deadline, so watch for the server's exit
        // meanwhile: one seen only afterwards can't be placed before it.
        let watching = self.exit.is_none();
        let mut exited = None;
        let result = {
            let mut write = std::pin::pin!(timeout_at(deadline, async {
                for (i, &end) in ends.iter().enumerate() {
                    if end <= written {
                        continue;
                    }
                    if i > 0 && !pause.is_zero() {
                        sleep(pause).await;
                    }
                    (piece_at, piece_seq) = (start.elapsed(), next_seq());
                    pieces.push(piece_at);
                    while written < end {
                        let n = stdin.write(&bytes[written..end]).await?;
                        if n == 0 {
                            return Err(std::io::Error::from(std::io::ErrorKind::WriteZero));
                        }
                        written += n;
                    }
                    stdin.flush().await?;
                    record_began(t, piece_at, piece_seq, EventKind::Sent(bytes[piece..written].to_vec()));
                    piece = written;
                }
                Ok(())
            }));
            // Waiting for the exit is cancel safe, and the write is never
            // dropped here: it goes on after an exit, since a process the
            // server left behind may still read it.
            tokio::select! {
                biased;
                status = child.wait(), if watching => {
                    if let Ok(status) = status {
                        record(t, start.elapsed(), EventKind::Exited(describe_status(status)));
                        exited = Some((status, Instant::now()));
                    }
                    write.await
                }
                result = &mut write => result,
            }
        };
        if let Some((status, seen)) = exited {
            self.exit = Some(status);
            self.exit_seen = Some(seen);
        }
        if written > piece {
            record_began(&self.transcript, piece_at, piece_seq, EventKind::Sent(bytes[piece..written].to_vec()));
        }
        self.written += written as u64;
        let outcome = match result {
            Ok(Ok(())) => Ok(pieces),
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                self.stdin = None;
                Err(WriteError::Closed)
            }
            Ok(Err(e)) => Err(WriteError::Io(e.to_string())),
            Err(_) => Err(WriteError::Timeout),
        };
        if let Err(e) = &outcome {
            self.note(format!("write stopped after {written} of {} bytes: {e}", bytes.len()));
            if written > 0 && written < bytes.len() && self.stdin.is_some() {
                self.abandoned = Some(began);
                self.limit_self(
                    began,
                    format!("lspcheck's write stopped partway through a frame ({e}), so it sent nothing more"),
                );
            } else if *e != WriteError::Closed {
                self.limit_self(began, format!("lspcheck couldn't finish its write ({e})"));
            }
        }
        outcome
    }

    /// Records that lspcheck's own side of the conversation is incomplete.
    fn limit_self(&mut self, began: Duration, why: String) {
        self.note(why.clone());
        if self.own_limit.as_ref().is_none_or(|(at, _)| began < *at) {
            self.own_limit = Some((began, why));
        }
    }

    /// Frames and writes one message.
    pub async fn send(&mut self, value: &Value) -> Result<(), WriteError> {
        self.send_until(value, Instant::now() + self.opts.write_timeout).await
    }

    async fn send_until(&mut self, value: &Value, deadline: Instant) -> Result<(), WriteError> {
        // From `initialized` on, a response without an id answers a
        // notification. Recorded before the write so that a response read
        // while it is in progress is still counted.
        if value.get("id").is_none()
            && value.get("method").and_then(Value::as_str) == Some("initialized")
            && self.initialized_at.is_none()
        {
            self.initialized_at = Some(self.start.elapsed());
        }
        let body = serde_json::to_vec(value).expect("JSON values serialize");
        self.write_until(&transport::encode(&body), deadline).await
    }

    /// A fresh integer id in LSP's 32-bit range. Its unpredictability is what
    /// keeps a server from preparing a response before the request exists.
    pub fn fresh_id(&mut self) -> Id {
        loop {
            let n = (random_u64() % (i32::MAX as u64)) as i64 + 1;
            let id = Id::int(n);
            // Handed-out ids are reserved but count as sent only once
            // registered, so a reply to one never sent stays unasked (J2).
            if !self.used_ids.contains_key(&id) && self.reserved.insert(id.clone()) {
                return id;
            }
        }
    }

    /// Keeps [`Session::fresh_id`] from choosing this id. It remains available
    /// to a caller that deliberately sends a request with that id.
    pub fn reserve_id(&mut self, id: Id) {
        self.reserved.insert(id);
    }

    /// Whether lspcheck has sent a request with this id in this session.
    pub fn sent(&self, id: &Id) -> bool {
        self.used_ids.contains_key(id)
    }

    /// When the session registered the request with this id, if it did: no
    /// response read before then can answer it.
    pub fn registered_at(&self, id: &Id) -> Option<Duration> {
        self.used_ids.get(id).copied()
    }

    /// The indices in `messages` of unmatched responses read at or after `since`.
    pub fn unmatched_indices(&self, since: Duration) -> impl Iterator<Item = usize> + '_ {
        self.unmatched.iter().copied().filter(move |&i| self.messages[i].0 >= since)
    }

    /// The ids of unmatched responses read at or after `since`.
    pub fn unmatched_ids(&self, since: Duration) -> Vec<Id> {
        self.unmatched_responses(since, Duration::MAX).filter_map(|m| m.response_id().cloned()).collect()
    }

    pub fn unmatched_responses(&self, from: Duration, to: Duration) -> impl Iterator<Item = &Message> {
        self.unmatched.iter().filter_map(move |&i| {
            let (at, m) = &self.messages[i];
            (*at >= from && *at <= to).then_some(m)
        })
    }

    /// Sends a request, registering it so only a later response can answer it.
    pub async fn request(&mut self, method: &str, params: Option<Value>) -> Result<Id, WriteError> {
        let id = self.fresh_id();
        self.request_with_id(&id, method, params).await?;
        Ok(id)
    }

    /// Sends a request with a chosen id. Only output lspcheck has already read
    /// is excluded from answering it, so with a predictable id a response the
    /// server wrote just before may still count; [`Session::request`] avoids that.
    pub async fn request_with_id(&mut self, id: &Id, method: &str, params: Option<Value>) -> Result<(), WriteError> {
        self.request_until(id, method, params, Instant::now() + self.opts.write_timeout).await
    }

    async fn request_until(
        &mut self,
        id: &Id,
        method: &str,
        params: Option<Value>,
        deadline: Instant,
    ) -> Result<(), WriteError> {
        self.begin_request(id, method, deadline).await?;
        let result = self.send_until(&message::request(id, method, params), deadline).await;
        if result.is_err() {
            self.abort_request(id);
        }
        result
    }

    /// Registers a request for `method` about to be written some other way
    /// (split, with other frames, or with extra headers), so only a later
    /// response can answer it. If writing it fails, call [`Session::abort_request`].
    pub async fn begin_request(&mut self, id: &Id, method: &str, deadline: Instant) -> Result<(), WriteError> {
        if self.pending.contains_key(id) {
            return Err(WriteError::DuplicateId(id.to_string()));
        }
        // Responses lspcheck has already read can't answer this request.
        self.absorb(deadline).await;
        if Instant::now() >= deadline {
            return Err(WriteError::Timeout);
        }
        let sent = self.start.elapsed();
        self.used_ids.insert(id.clone(), sent);
        self.answers.remove(id);
        self.methods.insert(id.clone(), method.to_string());
        self.pending.insert(id.clone(), sent);
        Ok(())
    }

    /// Forgets a request registered with [`Session::begin_request`] that couldn't be written.
    pub fn abort_request(&mut self, id: &Id) {
        self.pending.remove(id);
        self.methods.remove(id);
        self.aborted.insert(id.clone());
    }

    pub async fn notify(&mut self, method: &str, params: Option<Value>) -> Result<(), WriteError> {
        self.send(&message::notification(method, params)).await
    }

    /// Closes the server's stdin.
    pub fn close_stdin(&mut self) {
        if self.stdin.take().is_some() {
            record(&self.transcript, self.start.elapsed(), EventKind::StdinClosed);
        }
    }

    /// Takes in what has already been read, without waiting for more and
    /// without going past `deadline`. What it doesn't reach is handled later.
    pub(crate) async fn absorb(&mut self, deadline: Instant) -> bool {
        tokio::task::yield_now().await;
        let mut n = 0u32;
        while !self.stdout_closed && Instant::now() < deadline {
            match self.rx.try_recv() {
                Ok((at, inbound)) => {
                    let _ = self.handle(at, inbound, deadline).await;
                }
                Err(_) => return true,
            }
            n += 1;
            if n.is_multiple_of(256) {
                tokio::task::yield_now().await;
            }
        }
        self.stdout_closed || self.rx.is_empty()
    }

    /// Receives the next message or stream problem, answering server requests
    /// on the way when `auto_respond` is set. Nothing is processed after `deadline`.
    pub async fn recv(&mut self, deadline: Instant) -> Recv {
        if self.stdout_closed {
            return Recv::Closed;
        }
        if Instant::now() >= deadline {
            return Recv::Timeout;
        }
        match timeout_at(deadline, self.rx.recv()).await {
            Err(_) => Recv::Timeout,
            Ok(None) => {
                self.stdout_closed = true;
                Recv::Closed
            }
            Ok(Some((at, inbound))) => self.handle(at, inbound, deadline).await,
        }
    }

    async fn handle(&mut self, at: Duration, inbound: Inbound, deadline: Instant) -> Recv {
        match inbound {
            Inbound::Closed | Inbound::Stopped => {
                self.stdout_closed = true;
                Recv::Closed
            }
            Inbound::Problem(p) => Recv::Problem(p),
            Inbound::Message(m) => {
                self.accept(at, &m);
                if self.opts.auto_respond
                    && let Kind::Request { id, method } = &m.kind
                {
                    self.answer(id, method, &m.value, deadline).await;
                }
                Recv::Message(m)
            }
        }
    }

    /// Records a message and matches a response to its outstanding request.
    fn accept(&mut self, at: Duration, m: &Message) {
        self.messages.push((at, m.clone()));
        if let Kind::Request { id, method } = &m.kind {
            self.server_requests.push(ServerRequest {
                at,
                id: id.clone(),
                method: method.clone(),
                answer_began: None,
                answered_at: None,
            });
        }
        let Some(id) = m.response_id() else { return };
        if self.used_ids.get(id).is_some_and(|sent| at >= *sent)
            && let Some(method) = self.methods.get(id)
        {
            self.answered_methods.insert(self.messages.len() - 1, method.clone());
        }
        match self.pending.get(id) {
            Some(&sent) if at >= sent => {
                self.pending.remove(id);
                self.answers.insert(id.clone(), self.messages.len() - 1);
                self.answered.push((at, id.clone()));
            }
            _ => self.unmatched.push(self.messages.len() - 1),
        }
    }

    async fn answer(&mut self, id: &Id, method: &str, request: &Value, deadline: Instant) {
        let reply = match method {
            "workspace/configuration" => {
                let items = request.pointer("/params/items").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
                let answers = match &self.opts.configuration {
                    Some(settings) => items.iter().map(|item| configuration_item(settings, item)).collect(),
                    None => vec![Value::Null; items.len()],
                };
                message::response(id, Value::Array(answers))
            }
            "workspace/workspaceFolders" => message::response(id, self.opts.workspace_folders.clone()),
            "workspace/applyEdit" => message::response(id, json!({"applied": false})),
            "window/showDocument" => message::response(id, json!({"success": false})),
            "client/registerCapability"
            | "client/unregisterCapability"
            | "window/workDoneProgress/create"
            | "window/showMessageRequest"
            | "workspace/codeLens/refresh"
            | "workspace/diagnostic/refresh"
            | "workspace/foldingRange/refresh"
            | "workspace/inlayHint/refresh"
            | "workspace/inlineValue/refresh"
            | "workspace/semanticTokens/refresh" => message::response(id, Value::Null),
            _ => message::error_response(id, codes::METHOD_NOT_FOUND, "lspcheck does not handle this request"),
        };
        // `accept` has just recorded this request.
        let i = self.server_requests.len() - 1;
        let frame = transport::encode(&serde_json::to_vec(&reply).expect("JSON values serialize"));
        if let Some((cap, reserve)) = self.answer_cap
            && !self.fits(frame.len() as u64)
        {
            // The server may be waiting for this answer from the moment it
            // asked, so nothing it held back since can be blamed on it.
            let at = self.server_requests[i].at;
            let why = format!(
                "lspcheck left the server's {method} request unanswered: its {}-byte answer didn't fit the {} MiB \
                 written-bytes cap beside the {} bytes written and the {reserve} kept for lspcheck's own remaining writes",
                frame.len(),
                cap / (1024 * 1024),
                self.written
            );
            self.limit_self(at, why.clone());
            self.withheld.get_or_insert((at, why));
            return;
        }
        let began = self.start.elapsed();
        self.server_requests[i].answer_began = Some(began);
        let written = self.write_until(&frame, deadline).await;
        if written.is_ok() {
            self.server_requests[i].answered_at = Some(self.start.elapsed());
        }
        if let Err(e) = written
            && e != WriteError::Closed
        {
            // The server may be waiting for this answer, so what follows can't
            // be blamed on it. (A closed stdin means the server is exiting or
            // closed it itself; nothing more is expected of it.)
            self.limit_self(began, format!("lspcheck couldn't answer the server's {method} request ({e})"));
        }
    }

    /// Waits for the response to `id`, which may already have arrived.
    pub async fn wait_response(&mut self, id: &Id, deadline: Instant) -> Result<Message, WaitError> {
        loop {
            if let Some(&i) = self.answers.get(id) {
                return Ok(self.messages[i].1.clone());
            }
            match self.recv(deadline).await {
                Recv::Message(_) | Recv::Problem(_) => {}
                Recv::Closed => return Err(WaitError::Closed),
                Recv::Timeout => return Err(WaitError::Timeout),
            }
        }
    }

    /// Sends a request and waits for its response, all by one deadline.
    pub async fn call(&mut self, method: &str, params: Option<Value>, timeout: Duration) -> Result<Message, CallError> {
        let deadline = Instant::now() + timeout;
        let id = self.fresh_id();
        self.request_until(&id, method, params, deadline).await.map_err(CallError::Write)?;
        self.wait_response(&id, deadline).await.map_err(CallError::Wait)
    }

    /// Sends a request with a caller-chosen id and waits for its response by
    /// one absolute deadline.
    pub async fn call_with_id(
        &mut self,
        id: &Id,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Message, CallError> {
        self.call_with_id_until(id, method, params, Instant::now() + timeout).await
    }

    /// [`Session::call_with_id`] by a deadline the caller may also use after it.
    pub async fn call_with_id_until(
        &mut self,
        id: &Id,
        method: &str,
        params: Option<Value>,
        deadline: Instant,
    ) -> Result<Message, CallError> {
        self.request_until(id, method, params, deadline).await.map_err(CallError::Write)?;
        self.wait_response(id, deadline).await.map_err(CallError::Wait)
    }

    /// Receives everything until `deadline` or until stdout closes.
    pub async fn drain(&mut self, deadline: Instant) {
        loop {
            match self.recv(deadline).await {
                Recv::Closed | Recv::Timeout => return,
                _ => {}
            }
        }
    }

    /// Whether the server process is still running.
    pub fn is_alive(&mut self) -> bool {
        if self.exit.is_some() {
            return false;
        }
        // Running when it was looked at means running now too.
        let looked = Instant::now();
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.exited(status);
                false
            }
            Ok(None) => {
                self.running_seen = Some(looked);
                true
            }
            Err(_) => false,
        }
    }

    fn exited(&mut self, status: ExitStatus) {
        self.exit = Some(status);
        self.exit_seen = Some(Instant::now());
        record(&self.transcript, self.start.elapsed(), EventKind::Exited(describe_status(status)));
    }

    /// Waits for the server to exit by `deadline`, reading what it sends
    /// meanwhile and answering its requests, and gives its status if it
    /// exited. A server may be blocked on a request it sent just before
    /// reading `exit` (gopls waits for its progress token after
    /// `initialized`), so waiting without answering could keep it running
    /// through lspcheck's own doing.
    pub async fn serve_until_exit(&mut self, deadline: Instant) -> Option<ExitStatus> {
        // Each pass looks first, so an exit while an answer was prepared is
        // seen as soon as lspcheck is free again.
        while self.is_alive() && Instant::now() < deadline {
            if self.stdout_closed {
                self.wait_exit(deadline).await;
                break;
            }
            // Both futures are cancel safe: whichever loses is dropped
            // without losing a message or the exit status.
            let event = tokio::select! {
                biased;
                status = self.child.wait() => Some(Err(status)),
                _ = sleep_until(deadline) => None,
                inbound = self.rx.recv() => Some(Ok(inbound)),
            };
            match event {
                None | Some(Err(Err(_))) => break,
                Some(Err(Ok(status))) => self.exited(status),
                Some(Ok(None)) => self.stdout_closed = true,
                Some(Ok(Some((at, inbound)))) => {
                    self.handle(at, inbound, deadline).await;
                }
            }
        }
        self.exit
    }

    /// L6 and L7's wait, after the `exit` lspcheck has just written. Like
    /// [`Session::serve_until_exit`], it reads what the server sends and
    /// answers its requests, but a server still running is looked at only
    /// once it has been quiet for a whole `timeout`, not merely once
    /// `timeout` has passed: lspcheck may have been busy with what the
    /// server sent, and the server waiting on lspcheck meanwhile.
    ///
    /// Quiet is counted from Q, the last time lspcheck went back to waiting
    /// with nothing of the server's left to handle: from now, then from after
    /// each thing it takes in, and so after each answer it writes in full;
    /// and from the last read of the server's stdout or stderr, if later.
    /// Just before it looks, lspcheck lets those reads happen and takes in
    /// what they queued, and if there was anything, it counts again. A
    /// server quiet since `exit` is looked at by the deadline, `timeout`
    /// from now. Anything after the deadline ends the wait at once, so it
    /// lasts at most twice `timeout`.
    ///
    /// lspcheck watches for the exit while it waits and while it writes an
    /// answer, but not while it prepares one, so an exit is judged by when
    /// lspcheck saw it. Whether a server still running had really been left
    /// nothing to wait for is known only once the session has ended.
    pub async fn serve_after_exit(&mut self, timeout: Duration) -> ExitWait {
        let sent = self.start.elapsed();
        let due = sent + timeout;
        let deadline = self.start + due;
        let (mut since, mut after) = (sent, None);
        loop {
            // An exit seen while an answer was written decides, though the
            // write went on past the deadline.
            if self.exit.is_some() {
                break;
            }
            let heard = self.heard();
            let quiet = heard.as_ref().map_or(since, |(at, _)| since.max(*at));
            if quiet > due {
                let what = match heard {
                    Some((at, stream)) if at > since => format!("lspcheck read its {stream}"),
                    _ => after.clone().unwrap_or_default(),
                };
                return ExitWait::Busy { at: quiet, what };
            }
            if !self.is_alive() {
                break;
            }
            let look = self.start + quiet + timeout;
            if Instant::now() >= look {
                // Let the readers take in what the server has written, so a
                // backlog isn't mistaken for quiet.
                tokio::task::yield_now().await;
                if let Ok((at, inbound)) = self.rx.try_recv() {
                    if let Some(what) = self.take_after_exit(at, inbound, deadline).await {
                        (since, after) = (self.start.elapsed(), Some(what));
                    }
                    continue;
                }
                if self.heard().map(|(at, _)| at) > Some(quiet) {
                    continue;
                }
                if self.is_alive() {
                    let seen = self.running_seen.expect("just seen running").saturating_duration_since(self.start);
                    return ExitWait::Running(Quiet { sent, due, since, after, seen });
                }
                break;
            }
            if self.stdout_closed {
                self.wait_exit(look).await;
                continue;
            }
            // Both futures are cancel safe: whichever loses is dropped
            // without losing a message or the exit status.
            let event = tokio::select! {
                biased;
                status = self.child.wait() => Some(Err(status)),
                _ = sleep_until(look) => None,
                inbound = self.rx.recv() => Some(Ok(inbound)),
            };
            match event {
                None => {}
                Some(Err(Ok(status))) => self.exited(status),
                Some(Err(Err(_))) => return ExitWait::Unknown,
                Some(Ok(None)) => self.stdout_closed = true,
                Some(Ok(Some((at, inbound)))) => {
                    if let Some(what) = self.take_after_exit(at, inbound, deadline).await {
                        (since, after) = (self.start.elapsed(), Some(what));
                    }
                }
            }
        }
        let since_start = |at: Instant| at.saturating_duration_since(self.start);
        match (self.exit, self.exit_seen) {
            (Some(status), Some(seen)) if seen <= deadline => ExitWait::Exited(status),
            (Some(status), Some(seen)) => {
                ExitWait::Unseen { running: self.running_seen.map(since_start), exited: since_start(seen), status }
            }
            _ => ExitWait::Unknown,
        }
    }

    /// Takes in one thing the stdout task sent while lspcheck waits for the
    /// server to exit, answering a request, and says what lspcheck did with
    /// it; nothing for the end of stdout, which needs nothing more.
    async fn take_after_exit(&mut self, at: Duration, inbound: Inbound, deadline: Instant) -> Option<String> {
        let what = match &inbound {
            Inbound::Closed | Inbound::Stopped => None,
            Inbound::Problem(p) => Some(format!("lspcheck read output it couldn't take as a message ({p})")),
            Inbound::Message(m) => Some(match &m.kind {
                Kind::Request { id, method } => format!("its {method} request {id}"),
                Kind::Notification { method } => format!("lspcheck read its {method} notification"),
                Kind::Response { .. } => "lspcheck read a response from it".into(),
                Kind::Invalid(_) => "lspcheck read a message from it that isn't JSON-RPC".into(),
            }),
        };
        let request = matches!(&inbound, Inbound::Message(m) if matches!(m.kind, Kind::Request { .. }));
        self.handle(at, inbound, deadline).await;
        if !request {
            return what;
        }
        let answered = self.server_requests.last().is_some_and(|r| r.answered_at.is_some());
        what.map(|r| if answered { format!("lspcheck answered {r}") } else { format!("lspcheck didn't answer {r}") })
    }

    /// When lspcheck last read bytes from the server's stdout or stderr, and
    /// which, if it has.
    fn heard(&self) -> Option<(Duration, &'static str)> {
        let stdout = self.evidence.lock().expect("evidence lock").last_read.map(|at| (at, "stdout"));
        let stderr = self.stderr_read.lock().expect("stderr lock").map(|at| (at, "stderr"));
        stdout.into_iter().chain(stderr).max_by_key(|(at, _)| *at)
    }

    pub async fn wait_exit(&mut self, deadline: Instant) -> Option<ExitStatus> {
        if let Some(status) = self.exit {
            return Some(status);
        }
        match timeout_at(deadline, self.child.wait()).await {
            Ok(Ok(status)) => {
                self.exited(status);
                Some(status)
            }
            _ => None,
        }
    }

    /// Records that lspcheck is about to signal processes that may hold
    /// stdout, unless it already has.
    fn signalling(&self) {
        let at = self.start.elapsed();
        self.evidence.lock().expect("evidence lock").signalled.get_or_insert(at);
    }

    /// Waits until the stdout task has seen the end of the stream, or `deadline`.
    async fn wait_for_stdout_end(&self, deadline: Instant) {
        while !self.evidence.lock().expect("evidence lock").done && Instant::now() < deadline {
            sleep(Duration::from_millis(10)).await;
        }
    }

    /// Ends the session: stops the server if it is still running, waits up to
    /// `grace` for its descendants to exit, records any still alive, and stops them.
    pub async fn finish(mut self, grace: Duration) -> Finish {
        self.stdin = None;
        let mut killed = false;
        if self.is_alive() {
            killed = true;
            self.note(STOPPING_NOTE);
            self.signalling();
            #[cfg(unix)]
            {
                self.group.signal(nix::sys::signal::Signal::SIGTERM);
                if self.wait_exit(Instant::now() + Duration::from_millis(500)).await.is_none() {
                    self.group.signal(nix::sys::signal::Signal::SIGKILL);
                }
            }
            let _ = self.child.start_kill();
            let _ = self.wait_exit(Instant::now() + Duration::from_secs(5)).await;
        }

        // Coverage depends on this session still being registered.
        let coverage = process::coverage();
        let mut scan_error = None;
        let pid = self.pid;
        let scan = |group: &Group, deadline: Instant| -> Result<Found, String> {
            process::reap_adopted();
            let mut found = group.descendants(deadline.into_std())?;
            found.confirmed.retain(|p| p.pid != pid);
            if let Some(u) = &mut found.unconfirmed {
                u.processes.retain(|p| p.pid != pid);
            }
            found.unconfirmed = found.unconfirmed.filter(|u| !u.processes.is_empty());
            Ok(found)
        };
        // Every scan and every listing it makes ends by one deadline, which
        // gives the scan begun as the grace ends the time a `ps` run may take.
        let grace_ends = Instant::now() + grace;
        let search_ends = grace_ends + Duration::from_secs(5);
        let found = match settle(|| scan(&self.group, search_ends), grace_ends).await {
            Ok(found) => found,
            Err(e) => {
                scan_error = Some(e);
                Found::default()
            }
        };
        let Found { confirmed: leftovers, unconfirmed } = found;
        for p in &leftovers {
            self.note(format!("still running after the server exited: {p}"));
        }
        if let Some(u) = &unconfirmed {
            for p in &u.processes {
                self.note(format!("still running after the server exited, found through its parent ({}): {p}", u.why));
            }
        }
        // Stop what's left: the group by its id (safe while the sentinel holds
        // it), and adopted processes, which are lspcheck's unreaped children
        // and so can't have been replaced by another process. Stopping one can
        // orphan its own children, which are then adopted too, so repeat for
        // up to two seconds. A process outside the group, found only through
        // its parent, isn't signalled by its id: once its parent is stopped it
        // is adopted and stopped on Linux, and out of sight elsewhere.
        //
        // With none found and the server gone by itself, nothing should hold
        // stdout, so let the stdout task see its end first: a stream the
        // server ended itself is then judged as such. Whatever holds it still,
        // which the scans missed, the signals below may stop, and from then
        // on an end of the stream may be lspcheck's doing. Elsewhere the sweep
        // sends no signal, so the end of the stream stays the server's.
        #[cfg(unix)]
        {
            if !killed && leftovers.is_empty() && unconfirmed.is_none() && scan_error.is_none() {
                self.wait_for_stdout_end(Instant::now() + Duration::from_secs(5)).await;
            }
            self.signalling();
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        let me = std::process::id() as i32;
        let mut not_stopped = Vec::new();
        // The last scan's error, while no scan since has worked: a failed
        // listing says nothing about what is left, so keep sweeping.
        let mut sweep_error;
        loop {
            #[cfg(unix)]
            self.group.signal(nix::sys::signal::Signal::SIGKILL);
            let left = match scan(&self.group, deadline) {
                Ok(left) => {
                    sweep_error = None;
                    left
                }
                Err(e) => {
                    sweep_error = Some(e);
                    if Instant::now() >= deadline {
                        break;
                    }
                    sleep(Duration::from_millis(20)).await;
                    continue;
                }
            };
            if left.is_empty() {
                break;
            }
            if Instant::now() >= deadline {
                not_stopped = left.confirmed;
                break;
            }
            for p in left.confirmed.iter().filter(|p| p.ppid == me) {
                if let Err(e) = process::kill_process(p) {
                    self.note(e);
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
        for p in &not_stopped {
            self.note(format!("lspcheck couldn't stop {p}"));
        }
        if let Some(e) = sweep_error {
            self.note(format!("lspcheck couldn't confirm that every process it started was stopped: {e}"));
        }

        // Let the stdout task see the end of the stream, so its evidence is
        // complete. Every process has been stopped, so only parsing what is
        // already buffered remains; the bound covers a process lspcheck
        // couldn't stop that still holds the pipe.
        self.wait_for_stdout_end(Instant::now() + Duration::from_secs(10)).await;
        // The stdout task has stopped (or the bound above expired), so
        // everything it read is already queued. Record it all without waiting:
        // every process has been stopped, so no server request can be answered,
        // and the work is bounded by --max-output-mib.
        self.take_queued(false);
        self.group.close();
        let mut evidence = std::mem::take(&mut *self.evidence.lock().expect("evidence lock"));
        let now = self.start.elapsed();
        // Reading stopped here, so anything sent from now on went unread,
        // whatever made inspection incomplete earlier.
        if !evidence.done {
            let why = "stdout stayed open after the server exited, so later output wasn't inspected".to_string();
            evidence.limit.get_or_insert((now, why.clone()));
            evidence.stopped.get_or_insert((now, why));
        }
        // A frame still unfinished when reading ended may hold anything sent
        // since it began.
        if !evidence.done
            && let Some(began) = evidence.partial_since
        {
            evidence.lost.push((began, now, "a frame was still unfinished when lspcheck stopped reading".into()));
        }
        // A response without an id read after `initialized` could be a reply
        // to a notification (`exit` included); anything lspcheck missed since,
        // the unfinished frame above included, makes judging J3 inconclusive.
        let ongoing = [evidence.stopped.as_ref(), self.own_limit.as_ref()];
        let j3_incomplete = self.initialized_at.and_then(|from| incomplete_between(&evidence.lost, ongoing, from, now));
        let mut transcript = std::mem::take(&mut *self.transcript.lock().expect("transcript lock"));
        // Writes are recorded after flushing, so the reader may append a
        // later reply first. Put events back in the order they began; this is
        // the one place transcripts are ordered, and every consumer reads them
        // from here.
        transcript.sort_by_key(|event| event.seq);
        let whole = evidence.done && !evidence.cut;
        let (miscounted, unchecked) = miscounted_frames(&evidence.unparsed, &transcript, whole);
        // Whether an unmatched reply is a duplicate, early, aborted or only
        // loosely equal needs the whole session's state, not the moment it
        // was read: a guessed id may be sent later, and a write may fail
        // after the reply.
        let unmatched = std::mem::take(&mut self.unmatched)
            .into_iter()
            .map(|i| {
                let (at, m) = &self.messages[i];
                let id = m.response_id().expect("unmatched response");
                let sent_at = self.used_ids.get(id).copied();
                Unmatched {
                    at: *at,
                    duplicate: self.answers.contains_key(id),
                    unsent: sent_at.is_none(),
                    early: sent_at.is_some_and(|sent| sent > *at),
                    aborted: self.aborted.contains(id),
                    loose: self.used_ids.keys().find(|sent| message::loosely_equal(id, sent)).cloned(),
                    id: id.clone(),
                }
            })
            .collect();
        Finish {
            exit: if killed { None } else { self.exit },
            killed,
            leftovers,
            unconfirmed,
            coverage,
            scan_error,
            not_stopped,
            limit: evidence.limit,
            own_limit: self.own_limit.take(),
            lost: evidence.lost,
            stopped: evidence.stopped,
            last_read: evidence.last_read,
            transcript,
            messages: std::mem::take(&mut self.messages),
            initialized_at: self.initialized_at,
            j3_incomplete,
            abandoned_at: self.abandoned,
            problems: evidence.problems,
            cut_short: evidence.cut_short,
            unmatched,
            answered: std::mem::take(&mut self.answered),
            answers: std::mem::take(&mut self.answers),
            answered_methods: std::mem::take(&mut self.answered_methods),
            sent: {
                let mut sent: Vec<(Duration, Id)> = self
                    .used_ids
                    .iter()
                    .filter(|(id, _)| !self.aborted.contains(*id))
                    .map(|(id, at)| (*at, id.clone()))
                    .collect();
                sent.sort_by_key(|(at, _)| *at);
                sent
            },
            server_requests: std::mem::take(&mut self.server_requests),
            content_types: evidence.content_types,
            non_ascii: evidence.non_ascii,
            miscounted,
            unchecked,
        }
    }
}

/// Scans for leftover processes until a scan finds none or `deadline` has
/// passed. A process found by a scan begun before the deadline may have
/// exited before it, so only one begun at or after the deadline can find a
/// process still running then. For the same reason only that scan's failure
/// counts: an earlier one is tried again.
async fn settle(mut scan: impl FnMut() -> Result<Found, String>, deadline: Instant) -> Result<Found, String> {
    loop {
        let began = Instant::now();
        let left = scan();
        if began >= deadline || left.as_ref().is_ok_and(Found::is_empty) {
            return left;
        }
        sleep_until(deadline.min(Instant::now() + Duration::from_millis(50))).await;
    }
}

/// Reads and parses the server's stdout until it closes, recording every byte
/// and all framing evidence.
async fn read_stdout(
    mut stdout: tokio::process::ChildStdout,
    tx: mpsc::UnboundedSender<(Duration, Inbound)>,
    t: Transcript,
    evidence: Arc<Mutex<Evidence>>,
    start: Instant,
    opts: Options,
) {
    let mut reader = FrameReader::new(opts.max_header, opts.max_body);
    let mut buf = vec![0u8; 64 * 1024];
    let (mut total, mut stopped) = (0usize, false);
    let mut partial_since: Option<Duration> = None;
    let stop = |at: Duration, why: String| {
        record(&t, at, EventKind::Note(why.clone()));
        let mut ev = evidence.lock().expect("evidence lock");
        ev.cut = true;
        ev.stopped.get_or_insert((at, why.clone()));
        ev.limit.get_or_insert((at, why));
        drop(ev);
        let _ = tx.send((at, Inbound::Stopped));
    };
    // Records a frame the stream ended inside as cut short, if the end was
    // seen after lspcheck began signalling, whose signal may have stopped a
    // process in the middle of writing it. Other checks see it as unread, as
    // they would a problem. A server that ended its stream itself but whose
    // end is seen only after that gets the same benefit of the doubt.
    let cut_short = |ev: &mut Evidence, began: Duration, at: Duration, came: String| {
        if ev.signalled.is_none() {
            return false;
        }
        ev.lost.push((began, at, UNREAD.into()));
        let why = format!(
            "lspcheck saw stdout end inside a frame after it began stopping the server's processes, so it may have cut the frame short: {came} came"
        );
        ev.cut_short = Some((at, why));
        true
    };
    // `began`: when the first byte the chunk may contain arrived.
    let deliver = |began: Duration, at: Duration, chunk: Chunk, stopped: &mut bool| {
        record(&t, at, parsed_event(&chunk));
        let mut ev = evidence.lock().expect("evidence lock");
        let content_types = |ev: &mut Evidence, headers: &[Header]| {
            for h in headers.iter().filter(|h| h.name.eq_ignore_ascii_case("content-type")) {
                ev.content_types.push((at, h.value.clone()));
            }
        };
        let problem = match chunk {
            Chunk::Frame { headers, body, start, .. } => {
                content_types(&mut ev, &headers);
                match Message::parse(&body) {
                    Ok(m) => {
                        if ev.non_ascii.is_none() && !body.is_ascii() {
                            ev.non_ascii = Some(at);
                        }
                        let _ = tx.send((at, Inbound::Message(m)));
                        return;
                    }
                    Err(error) => {
                        ev.unparsed.push((at, body.len(), start));
                        Problem::NotJson { body, error }
                    }
                }
            }
            Chunk::Oversized { headers, content_length, limit } => {
                content_types(&mut ev, &headers);
                let why = format!(
                    "a {content_length}-byte frame is over lspcheck's {limit}-byte inspection limit (--max-frame-mib), so it wasn't checked"
                );
                ev.lost.push((began, at, why.clone()));
                ev.limit.get_or_insert((at, why));
                return;
            }
            Chunk::HeaderLimit { limit } => {
                drop(ev);
                *stopped = true;
                stop(
                    at,
                    format!(
                        "a header section is longer than lspcheck's {limit}-byte inspection limit; lspcheck stopped inspecting stdout"
                    ),
                );
                return;
            }
            Chunk::Garbage(b) => Problem::Garbage(b),
            Chunk::BadHeader { block, reason } => Problem::BadHeader { block, reason },
            Chunk::Truncated { headers, content_length, received } => {
                content_types(&mut ev, &headers);
                if cut_short(&mut ev, began, at, format!("{received} of {content_length} body bytes")) {
                    return;
                }
                Problem::Truncated { content_length, received }
            }
            Chunk::UnfinishedHeader { block } => {
                if cut_short(&mut ev, began, at, format!("{} bytes of its header section", block.len())) {
                    return;
                }
                Problem::BadHeader { block, reason: "the stream ended inside a header section".into() }
            }
        };
        // A log line loses nothing; nor does a header section the server
        // followed with no body before ending the stream itself, since a
        // message would have had to be in the body.
        if !matches!(problem, Problem::Garbage(_) | Problem::Truncated { received: 0, .. }) {
            ev.lost.push((began, at, UNREAD.into()));
        }
        ev.problems.push((at, problem.clone()));
        let _ = tx.send((at, Inbound::Problem(problem)));
    };
    loop {
        let n = match stdout.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        total += n;
        let at = start.elapsed();
        // Bytes past an inspection limit count too: L6 and L7 ask whether
        // the server wrote anything at all after a given time.
        evidence.lock().expect("evidence lock").last_read = Some(at);
        if stopped {
            continue;
        }
        if total > opts.max_output {
            stopped = true;
            stop(
                at,
                format!(
                    "the server wrote more than {} bytes to stdout; lspcheck stopped inspecting it (--max-output-mib)",
                    opts.max_output
                ),
            );
            continue;
        }
        record(&t, at, EventKind::Read(buf[..n].to_vec()));
        // Every chunk this read completes began no earlier than the oldest
        // unfinished byte (or this read, if there was none).
        let began = partial_since.unwrap_or(at);
        let mut completed = false;
        for chunk in reader.feed(&buf[..n]) {
            completed = true;
            deliver(began, at, chunk, &mut stopped);
            if stopped {
                break;
            }
        }
        // What is left unfinished began after the last chunk completed, so
        // in this read; with nothing completed, it is still what it was.
        partial_since = if reader.is_idle() {
            None
        } else if completed {
            Some(at)
        } else {
            Some(began)
        };
        evidence.lock().expect("evidence lock").partial_since = partial_since;
    }
    let at = start.elapsed();
    if !stopped {
        let began = partial_since.unwrap_or(at);
        for chunk in reader.finish() {
            deliver(began, at, chunk, &mut stopped);
        }
    } else if let Some(began) = partial_since {
        // Inspection stopped inside a frame, which may hold anything sent
        // since it began.
        let why = "a frame was still unfinished when lspcheck stopped inspecting stdout".to_string();
        evidence.lock().expect("evidence lock").lost.push((began, at, why));
    }
    record(&t, at, EventKind::StdoutClosed);
    evidence.lock().expect("evidence lock").done = true;
    let _ = tx.send((at, Inbound::Closed));
}

/// Frames whose body couldn't be read because their Content-Length wasn't
/// counted in bytes, and how many couldn't be checked. The transcript holds
/// every byte of stdout that was inspected; `whole` says whether that is all
/// of it.
fn miscounted_frames(
    unparsed: &[(Duration, usize, usize)],
    transcript: &[Event],
    whole: bool,
) -> (Vec<(Duration, String)>, usize) {
    if unparsed.is_empty() {
        return (Vec::new(), 0);
    }
    let stdout: Vec<u8> = transcript
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Read(b) => Some(b.as_slice()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .concat();
    // Parsing reads at most about 4 × Content-Length bytes per frame, which
    // is proportional to the output; looking for the header section after
    // a candidate body is bounded by this budget. A few examples are enough.
    let mut budget = 16 * 1024 * 1024;
    let (mut found, mut unchecked) = (Vec::new(), 0);
    for &(at, length, start) in unparsed {
        if found.len() == 16 {
            break;
        }
        let Some(rest) = stdout.get(start..) else { continue };
        match transport::miscounted_length(rest, length, whole, &mut budget) {
            transport::Count::Miscounted { bytes, counts } => {
                found.push((
                    at,
                    format!("Content-Length {length} counts {counts}, but the JSON body that follows is {bytes} bytes"),
                ));
            }
            transport::Count::Unknown => {}
            transport::Count::Unchecked => unchecked += 1,
        }
    }
    (found, unchecked)
}

fn parsed_event(chunk: &Chunk) -> EventKind {
    let (kind, detail) = match chunk {
        Chunk::Frame { body, .. } => ("frame", format!("{} body bytes", body.len())),
        Chunk::Garbage(b) => ("garbage", format!("{} bytes outside any frame", b.len())),
        Chunk::BadHeader { block, reason } => ("bad-header", format!("{} bytes: {reason}", block.len())),
        Chunk::Oversized { content_length, limit, .. } => {
            ("oversized", format!("{content_length} body bytes skipped (limit {limit})"))
        }
        Chunk::Truncated { content_length, received, .. } => {
            ("truncated", format!("{received} of {content_length} body bytes"))
        }
        Chunk::UnfinishedHeader { block } => ("truncated", format!("{} bytes into a header section", block.len())),
        Chunk::HeaderLimit { limit } => ("header-limit", format!("header section longer than {limit} bytes")),
    };
    EventKind::Parsed { kind, detail }
}

/// A random number, from the standard library's per-process hash keys.
pub fn random_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    h.finish()
}

/// Why [`Session::call`] got no response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    Write(WriteError),
    Wait(WaitError),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Write(e) => e.fmt(f),
            CallError::Wait(WaitError::Timeout) => f.write_str("no response before the deadline"),
            CallError::Wait(WaitError::Closed) => f.write_str("the server closed its stdout without responding"),
        }
    }
}

/// "status 0", "status 1", or "signal 9".
pub fn describe_status(status: ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("status {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return format!("signal {sig}");
        }
    }
    "an unknown status".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn failed_writes_make_evidence_incomplete_from_their_start() {
        let id = Id::Str("configuration".into());
        let request = message::request(&id, "workspace/configuration", Some(json!({"items": [{}]})));
        let request = transport::encode(&serde_json::to_vec(&request).unwrap());
        let request = String::from_utf8(request).unwrap().replace('\'', "'\\''");
        let script = format!("printf '%s' '{request}'; sleep 30");
        for automatic_answer in [true, false] {
            let cmd = ServerCommand::new(&["sh".into(), "-c".into(), script.clone().into()]);
            let opts = Options {
                auto_respond: false,
                configuration: Some(json!({"payload": "x".repeat(4 * 1024 * 1024)})),
                ..Options::default()
            };
            let mut s = Session::start(&cmd, opts).await.unwrap();
            let Recv::Message(request) = s.recv(Instant::now() + Duration::from_secs(5)).await else {
                panic!("the stand-in didn't signal it was ready");
            };
            assert_eq!(request.value["method"], "workspace/configuration");
            let since = s.elapsed();
            let expected_began = if automatic_answer {
                s.answer(&id, "workspace/configuration", &request.value, Instant::now()).await;
                let server_request = &s.server_requests[0];
                assert!(server_request.answered_at.is_none());
                server_request.answer_began
            } else {
                let id = Id::int(7);
                s.begin_request(&id, "lspcheck/large", Instant::now() + Duration::from_secs(5)).await.unwrap();
                let request =
                    message::request(&id, "lspcheck/large", Some(json!({"payload": "x".repeat(4 * 1024 * 1024)})));
                let request = transport::encode(&serde_json::to_vec(&request).unwrap());
                let error = s.write_split(&request, &[], Duration::ZERO, Instant::now()).await.unwrap_err();
                s.abort_request(&id);
                assert_eq!(error, WriteError::Timeout);
                if s.abandoned.is_none() {
                    assert_eq!(s.bytes_written(), 0);
                }
                s.abandoned
            };
            let began = s.own_limit.as_ref().unwrap().0;
            let incomplete = s.incomplete_within(since, began);
            let before = began.checked_sub(Duration::from_nanos(1)).unwrap();
            let earlier = s.incomplete_within(Duration::ZERO, before);
            let finish = s.finish(Duration::from_millis(200)).await;
            if let Some(expected) = expected_began {
                assert_eq!(began, expected);
            }
            assert!(incomplete.is_some());
            assert!(earlier.is_none());
            assert!(finish.incomplete_within(since, began).is_some());
            assert!(finish.incomplete_within(Duration::ZERO, before).is_none());
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn answers_past_the_cap_are_withheld_from_when_their_request_was_read() {
        let frames: Vec<u8> = ["first", "second"]
            .iter()
            .flat_map(|id| {
                let request =
                    message::request(&Id::Str((*id).into()), "workspace/configuration", Some(json!({"items": [{}]})));
                transport::encode(&serde_json::to_vec(&request).unwrap())
            })
            .collect();
        let script = format!("printf '%s' '{}'; sleep 30", String::from_utf8(frames).unwrap());
        let cmd = ServerCommand::new(&["sh".into(), "-c".into(), script.into()]);
        let opts = Options { configuration: Some(json!({"payload": "x".repeat(1000)})), ..Options::default() };
        let mut s = Session::start(&cmd, opts).await.unwrap();
        // Room for 2000 bytes in all, 1500 of them kept: the first answer,
        // over 1000 bytes, doesn't fit.
        s.cap_answers(2000, 1500);
        let deadline = Instant::now() + Duration::from_secs(5);
        assert!(matches!(s.recv(deadline).await, Recv::Message(_)));
        assert_eq!(s.bytes_written(), 0);
        let read = s.server_requests[0].at;
        assert_eq!(s.server_requests[0].answer_began, None);
        let (at, why) = s.answer_withheld().unwrap().clone();
        assert_eq!(at, read);
        assert!(
            why.starts_with("lspcheck left the server's workspace/configuration request unanswered: its "),
            "{why}"
        );
        assert_eq!(s.own_limit, Some((read, why.clone())));
        // With the reserve released, the next answer fits and is written;
        // the first withheld answer is still the one reported.
        s.cap_answers(2000, 0);
        assert!(matches!(s.recv(deadline).await, Recv::Message(_)));
        assert!(s.server_requests[1].answered_at.is_some());
        assert!(s.bytes_written() > 1000 && s.bytes_written() <= 2000);
        assert_eq!(s.answer_withheld().map(|(at, _)| *at), Some(read));
        let finish = s.finish(Duration::from_millis(200)).await;
        assert!(finish.transcript.iter().any(|e| matches!(&e.kind, EventKind::Note(n) if *n == why)));
        assert!(finish.incomplete_within(read, read).is_some());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn mutation_absorption_reports_a_backlog_when_its_deadline_expires() {
        let cmd = ServerCommand::new(&["sleep".into(), "30".into()]);
        let opts = Options { write_timeout: Duration::ZERO, ..Options::default() };
        let mut s = Session::start(&cmd, opts).await.unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        s.rx = rx;
        let notification =
            Message::parse(br#"{"jsonrpc":"2.0","method":"window/logMessage","params":{"type":4,"message":"queued"}}"#)
                .unwrap();
        assert!(tx.send((Duration::ZERO, Inbound::Message(notification))).is_ok());
        assert!(!s.absorb_all().await);
        assert!(s.messages.is_empty());
        s.opts.write_timeout = Duration::from_secs(5);
        assert!(s.absorb_all().await);
        assert_eq!(s.messages.len(), 1);
        assert!(s.absorb_all().await);
        assert!(tx.send((Duration::ZERO, Inbound::Closed)).is_ok());
        assert!(s.absorb_all().await);
        assert!(s.stdout_closed());
        s.finish(Duration::from_millis(200)).await;
    }

    #[test]
    fn configuration_items_are_looked_up_by_dotted_section() {
        let settings = json!({"gopls": {"staticcheck": false, "ui": {"hints": [1]}}, "x.y": 1});
        let item = |v: Value| configuration_item(&settings, &v);
        assert_eq!(item(json!({"section": "gopls"})), settings["gopls"]);
        assert_eq!(item(json!({"section": "gopls.staticcheck"})), json!(false));
        assert_eq!(item(json!({"section": "gopls.ui.hints"})), json!([1]));
        // A whole table without a section; null where the path leads nowhere.
        assert_eq!(item(json!({"scopeUri": "file:///x"})), settings);
        assert_eq!(item(json!({"section": ""})), settings);
        assert_eq!(item(json!({"section": "gopls.missing"})), Value::Null);
        assert_eq!(item(json!({"section": "gopls.staticcheck.deeper"})), Value::Null);
        // A dot always separates: a key spelled with one isn't found.
        assert_eq!(item(json!({"section": "x.y"})), Value::Null);
        assert_eq!(item(json!({"section": 3})), Value::Null);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn finished_transcripts_preserve_occurrence_order_when_writes_flush_late() {
        let cmd = ServerCommand::new(&["sleep".into(), "30".into()]);
        let s = Session::start(&cmd, Options::default()).await.unwrap();
        let id = Id::int(7);
        let request = transport::encode(&serde_json::to_vec(&message::request(&id, "probe", None)).unwrap());
        let reply = transport::encode(&serde_json::to_vec(&message::response(&id, Value::Null)).unwrap());
        let exit = transport::encode(&serde_json::to_vec(&message::notification("exit", None)).unwrap());
        // Force the schedule seen on Windows: a write in two pieces begins,
        // and the reader records the reply before the writer records its
        // pieces, all with the same coarse timestamp.
        let at = Duration::from_millis(10);
        let cut = request.len() / 2;
        let (first, second) = (next_seq(), next_seq());
        record(&s.transcript, at, EventKind::Read(reply));
        record_began(&s.transcript, at, second, EventKind::Sent(request[cut..].to_vec()));
        record_began(&s.transcript, at, first, EventKind::Sent(request[..cut].to_vec()));
        record(&s.transcript, at, EventKind::Sent(exit));
        let finish = s.finish(Duration::from_millis(200)).await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recording.jsonl");
        crate::transcript::write(&path, &finish.transcript).unwrap();
        let plan = crate::replay::parse(&path).unwrap();
        // The request comes before its reply, in byte order, so replay keeps
        // the recorded result and waits for it before the next write.
        assert_eq!(plan.requests.len(), 1);
        assert_eq!(plan.requests[0].recorded.as_deref(), Some("result"));
        assert_eq!(plan.steps[2].gates, vec![id]);
        assert!(finish.transcript.windows(2).all(|pair| pair[0].seq < pair[1].seq));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn exit_wait_does_not_consume_queued_input_after_its_deadline() {
        let cmd = ServerCommand::new(&["sleep".into(), "30".into()]);
        let mut s = Session::start(&cmd, Options::default()).await.unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        s.rx = rx;
        let message = Message::classify(json!({"jsonrpc": "2.0", "method": "window/logMessage"}));
        assert!(tx.send((s.elapsed(), Inbound::Message(message))).is_ok());
        let got = s.serve_until_exit(Instant::now() - Duration::from_secs(1)).await;
        assert!(got.is_none(), "{got:?}");
        assert!(s.messages.is_empty());
        s.finish(Duration::from_millis(200)).await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "current_thread")]
    async fn an_exit_seen_only_after_the_deadline_is_not_credited() {
        use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid};
        // Exits with status 1 once it has read a line.
        let cmd = ServerCommand::new(&["sh".into(), "-c".into(), "read line; exit 1".into()]);
        for busy in [true, false] {
            let mut s = Session::start(&cmd, Options::default()).await.unwrap();
            assert!(s.is_alive());
            let looked = s.elapsed();
            // Busy, the deadline is when the wait begins.
            let timeout = if busy { Duration::ZERO } else { Duration::from_secs(30) };
            s.write_raw(b"go\n").await.unwrap();
            if busy {
                // Wait for the exit without reaping it, so the session sees
                // it only after the deadline, as if lspcheck had been
                // preparing an answer while the deadline passed.
                let pid = nix::unistd::Pid::from_raw(s.pid());
                let flags = WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT | WaitPidFlag::WNOHANG;
                let limit = std::time::Instant::now() + Duration::from_secs(30);
                while waitid(Id::Pid(pid), flags).unwrap() == WaitStatus::StillAlive {
                    assert!(std::time::Instant::now() < limit, "the script didn't exit");
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            let began = s.elapsed();
            let got = s.serve_after_exit(timeout).await;
            if busy {
                let ExitWait::Unseen { running: Some(running), exited, status } = got else { panic!("{got:?}") };
                assert!(running <= looked, "{running:?} {looked:?}");
                assert!(exited > began, "{exited:?}");
                assert_eq!(status.code(), Some(1));
            } else {
                assert!(matches!(got, ExitWait::Exited(status) if status.code() == Some(1)), "{got:?}");
            }
            s.finish(Duration::from_millis(200)).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn an_answer_prepared_after_its_deadline_is_still_written() {
        let id = Id::Str("configuration".into());
        let request = message::request(&id, "workspace/configuration", Some(json!({"items": [{}]})));
        let request = transport::encode(&serde_json::to_vec(&request).unwrap());
        let script = format!("printf '%s' '{}'; sleep 30", String::from_utf8(request).unwrap());
        let cmd = ServerCommand::new(&["sh".into(), "-c".into(), script.into()]);
        let opts = Options { auto_respond: false, ..Options::default() };
        let mut s = Session::start(&cmd, opts).await.unwrap();
        let Recv::Message(request) = s.recv(Instant::now() + Duration::from_secs(5)).await else {
            panic!("the script's request didn't arrive");
        };
        // lspcheck has written to the server before, as it has by the time
        // it answers a request; the pipe has room for the answer.
        s.notify("initialized", Some(json!({}))).await.unwrap();
        let before = s.bytes_written();
        // The deadline passed while the answer was prepared. The server may
        // still read it, and J7 judges the requests that follow by it.
        s.answer(&id, "workspace/configuration", &request.value, Instant::now()).await;
        let r = &s.server_requests[0];
        assert!(r.answer_began.is_some() && r.answered_at.is_some(), "{r:?}");
        assert!(s.bytes_written() > before);
        assert_eq!(s.own_limit, None);
        s.finish(Duration::from_millis(200)).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn only_a_scan_begun_after_the_grace_deadline_finds_a_leftover() {
        let helper =
            Found { confirmed: vec![ProcInfo { pid: 7, ppid: 1, pgid: 7, name: "helper".into() }], unconfirmed: None };
        // The first scan begins well before the deadline and sees the helper,
        // which exits before the deadline, but the scan returns only after it.
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut began = Vec::new();
        let left = settle(
            || {
                began.push(Instant::now());
                if began.len() > 1 {
                    return Ok(Found::default());
                }
                std::thread::sleep((deadline + Duration::from_millis(10)).saturating_duration_since(Instant::now()));
                Ok(helper.clone())
            },
            deadline,
        )
        .await;
        assert_eq!(left, Ok(Found::default()));
        assert!(began[0] < deadline && began[1] >= deadline);
        // Seen by a scan begun after the deadline, it is left over; that
        // scan's failure ends the search.
        assert_eq!(settle(|| Ok(helper.clone()), Instant::now()).await, Ok(helper.clone()));
        assert_eq!(settle(|| Err("no ps".into()), Instant::now()).await, Err("no ps".into()));
        // A scan that fails before the deadline is tried again, and the one
        // begun after it decides.
        let deadline = Instant::now() + Duration::from_millis(200);
        let mut scans = 0;
        let left = settle(
            || {
                scans += 1;
                if Instant::now() < deadline { Err("busy".into()) } else { Ok(helper.clone()) }
            },
            deadline,
        )
        .await;
        assert_eq!(left, Ok(helper));
        assert!(scans > 1);
    }

    #[test]
    fn missed_evidence_counts_where_its_span_overlaps_the_wait() {
        let ms = Duration::from_millis;
        let span = |a, b| (ms(a), ms(b), format!("lost {a}-{b}"));
        let between = |lost: &[(Duration, Duration, String)], ongoing: Option<(Duration, String)>| {
            incomplete_between(lost, [ongoing.as_ref(), None], ms(100), ms(200))
        };
        // Complete before the request was sent: it can't have held the reply.
        assert_eq!(between(&[span(10, 90)], None), None);
        // Begun only after lspcheck stopped waiting.
        assert_eq!(between(&[span(210, 300)], None), None);
        // Begun before the request and finished after the wait: it spans it.
        assert_eq!(between(&[span(50, 900)], None).as_deref(), Some("lost 50-900"));
        // Inside the wait, or touching either end.
        assert!(between(&[span(120, 150)], None).is_some());
        assert!(between(&[span(10, 100)], None).is_some());
        assert!(between(&[span(200, 300)], None).is_some());
        // Reading that stopped excuses from any time up to the end of the wait.
        assert!(between(&[], Some((ms(5), "stopped".into()))).is_some());
        assert_eq!(between(&[], Some((ms(201), "stopped".into()))), None);
    }
}
