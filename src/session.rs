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
use crate::process::{self, Coverage, Group, ProcInfo, ServerCommand};
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
        }
    }
}

/// One transcript entry.
#[derive(Debug, Clone)]
pub struct Event {
    /// Time since the session started.
    pub at: Duration,
    pub kind: EventKind,
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
        files: Vec<(String, String)>,
    },
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
    /// When the first readable frame with non-ASCII text in its body arrived.
    non_ascii: Option<Duration>,
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
    /// Responses that matched no outstanding request, with what explains them.
    pub unmatched: Vec<Unmatched>,
    /// Requests that were answered, with when their response was read.
    pub answered: Vec<(Duration, Id)>,
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
    /// Why lspcheck may have missed something the server sent between `from`
    /// and `to`, judged over the whole session (see [`Session::incomplete_within`]).
    pub fn incomplete_within(&self, from: Duration, to: Duration) -> Option<String> {
        incomplete_between(&self.lost, [self.stopped.as_ref(), self.own_limit.as_ref()], from, to)
    }
}

type Transcript = Arc<Mutex<Vec<Event>>>;

fn record(t: &Transcript, at: Duration, kind: EventKind) {
    t.lock().expect("transcript lock").push(Event { at, kind });
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
    /// Every id a request was sent with, and when it was registered.
    used_ids: HashMap<Id, Duration>,
    /// Ids handed out by [`Session::fresh_id`], sent or not.
    reserved: HashSet<Id>,
    /// Ids whose write failed before the whole request reached the server.
    aborted: HashSet<Id>,
    /// Requests that were answered, with when the response was read.
    answered: Vec<(Duration, Id)>,
    unmatched: Vec<(Duration, Id)>,
    /// The server's requests, in the order they were read.
    server_requests: Vec<ServerRequest>,
    group: Group,
    exit: Option<ExitStatus>,
    opts: Options,
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
        tokio::spawn(async move {
            let mut buf = vec![0u8; 16 * 1024];
            let (mut kept, mut dropped) = (0usize, 0usize);
            loop {
                match stderr.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let keep = n.min(limit.saturating_sub(kept));
                        if keep > 0 {
                            record(&t, start.elapsed(), EventKind::Stderr(buf[..keep].to_vec()));
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
            messages: Vec::new(),
            initialized_at: None,
            own_limit: None,
            abandoned: None,
            pending: HashMap::new(),
            answers: HashMap::new(),
            used_ids: HashMap::new(),
            reserved: HashSet::new(),
            aborted: HashSet::new(),
            answered: Vec::new(),
            unmatched: Vec::new(),
            server_requests: Vec::new(),
            group,
            exit: None,
            opts,
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
    pub fn record_workspace(&self, path: PathBuf, files: Vec<(String, String)>) {
        record(&self.transcript, self.start.elapsed(), EventKind::Workspace { path, files });
    }

    /// Why lspcheck may have missed something the server sent so far: an
    /// inspection limit, or frames that couldn't be read. Unlike
    /// [`Session::incomplete`], lspcheck's own failures to write aren't counted.
    pub fn missed_output(&self) -> Option<String> {
        let ev = self.evidence.lock().expect("evidence lock");
        let unread = ev.problems.iter().any(|(_, p)| !matches!(p, Problem::Garbage(_)));
        ev.limit
            .as_ref()
            .map(|(_, why)| why.clone())
            .or_else(|| unread.then(|| "some of the server's frames couldn't be read (see F1)".to_string()))
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
                        && self.own_limit.is_none()
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
        let deadline = deadline.min(Instant::now() + self.opts.write_timeout);
        let began = self.start.elapsed();
        if self.abandoned.is_some() {
            return Err(WriteError::Abandoned);
        }
        let Some(stdin) = self.stdin.as_mut() else { return Err(WriteError::Closed) };
        let (t, start) = (&self.transcript, self.start);
        let ends: Vec<usize> =
            cuts.iter().copied().filter(|&c| c > 0 && c < bytes.len()).chain([bytes.len()]).collect();
        // The piece being written: where it starts and when it began.
        let (mut written, mut piece, mut piece_at) = (0usize, 0usize, start.elapsed());
        let result = timeout_at(deadline, async {
            for (i, &end) in ends.iter().enumerate() {
                if end <= written {
                    continue;
                }
                if i > 0 && !pause.is_zero() {
                    sleep(pause).await;
                }
                piece_at = start.elapsed();
                while written < end {
                    let n = stdin.write(&bytes[written..end]).await?;
                    if n == 0 {
                        return Err(std::io::Error::from(std::io::ErrorKind::WriteZero));
                    }
                    written += n;
                }
                stdin.flush().await?;
                record(t, piece_at, EventKind::Sent(bytes[piece..written].to_vec()));
                piece = written;
            }
            Ok(())
        })
        .await;
        if written > piece {
            record(&self.transcript, piece_at, EventKind::Sent(bytes[piece..written].to_vec()));
        }
        let outcome = match result {
            Ok(Ok(())) => Ok(()),
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
                self.limit_self(format!(
                    "lspcheck's write stopped partway through a frame ({e}), so it sent nothing more"
                ));
            }
        }
        outcome
    }

    /// Records that lspcheck's own side of the conversation is incomplete.
    fn limit_self(&mut self, why: String) {
        self.note(why.clone());
        if self.own_limit.is_none() {
            self.own_limit = Some((self.start.elapsed(), why));
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

    /// Whether lspcheck has sent a request with this id in this session.
    pub fn sent(&self, id: &Id) -> bool {
        self.used_ids.contains_key(id)
    }

    /// The ids of unmatched responses read at or after `since`.
    pub fn unmatched_ids(&self, since: Duration) -> Vec<Id> {
        self.unmatched.iter().filter(|(at, _)| *at >= since).map(|(_, id)| id.clone()).collect()
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
        self.begin_request(id, deadline).await?;
        let result = self.send_until(&message::request(id, method, params), deadline).await;
        if result.is_err() {
            self.abort_request(id);
        }
        result
    }

    /// Registers a request about to be written some other way (split, with
    /// other frames, or with extra headers), so only a later response can
    /// answer it. If writing it fails, call [`Session::abort_request`].
    pub async fn begin_request(&mut self, id: &Id, deadline: Instant) -> Result<(), WriteError> {
        if self.pending.contains_key(id) {
            return Err(WriteError::DuplicateId(id.to_string()));
        }
        // Responses lspcheck has already read can't answer this request.
        self.absorb(deadline).await;
        if Instant::now() >= deadline {
            return Err(WriteError::Timeout);
        }
        self.used_ids.insert(id.clone(), self.start.elapsed());
        self.answers.remove(id);
        self.pending.insert(id.clone(), self.start.elapsed());
        Ok(())
    }

    /// Forgets a request registered with [`Session::begin_request`] that couldn't be written.
    pub fn abort_request(&mut self, id: &Id) {
        self.pending.remove(id);
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
    async fn absorb(&mut self, deadline: Instant) {
        tokio::task::yield_now().await;
        let mut n = 0u32;
        while !self.stdout_closed && Instant::now() < deadline {
            match self.rx.try_recv() {
                Ok((at, inbound)) => {
                    let _ = self.handle(at, inbound, deadline).await;
                }
                Err(_) => break,
            }
            n += 1;
            if n.is_multiple_of(256) {
                tokio::task::yield_now().await;
            }
        }
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
        match self.pending.get(id) {
            Some(&sent) if at >= sent => {
                self.pending.remove(id);
                self.answers.insert(id.clone(), self.messages.len() - 1);
                self.answered.push((at, id.clone()));
            }
            _ => self.unmatched.push((at, id.clone())),
        }
    }

    async fn answer(&mut self, id: &Id, method: &str, request: &Value, deadline: Instant) {
        let reply = match method {
            "workspace/configuration" => {
                let n = request.pointer("/params/items").and_then(Value::as_array).map_or(0, Vec::len);
                message::response(id, Value::Array(vec![Value::Null; n]))
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
        self.server_requests[i].answer_began = Some(self.start.elapsed());
        let written = self.send_until(&reply, deadline).await;
        if written.is_ok() {
            self.server_requests[i].answered_at = Some(self.start.elapsed());
        }
        if let Err(e) = written
            && e != WriteError::Closed
        {
            // The server may be waiting for this answer, so what follows can't
            // be blamed on it. (A closed stdin means the server is exiting or
            // closed it itself; nothing more is expected of it.)
            self.limit_self(format!("lspcheck couldn't answer the server's {method} request ({e})"));
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
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.exited(status);
                false
            }
            Ok(None) => true,
            Err(_) => false,
        }
    }

    fn exited(&mut self, status: ExitStatus) {
        self.exit = Some(status);
        record(&self.transcript, self.start.elapsed(), EventKind::Exited(describe_status(status)));
    }

    /// Waits for the server to exit on its own.
    /// Waits for the server to exit by `deadline`, reading what it sends
    /// meanwhile and answering its requests. A server may be blocked on a
    /// request it sent just before reading `exit` (gopls waits for its
    /// progress token after `initialized`), so waiting without answering
    /// could keep it running through lspcheck's own doing.
    pub async fn serve_until_exit(&mut self, deadline: Instant) -> Option<ExitStatus> {
        loop {
            if let Some(status) = self.exit {
                return Some(status);
            }
            if self.stdout_closed {
                return self.wait_exit(deadline).await;
            }
            // Both futures are cancel safe: whichever loses is dropped
            // without losing a message or the exit status.
            let event = tokio::select! {
                biased;
                status = self.child.wait() => Err(status),
                inbound = self.rx.recv() => Ok(inbound),
                _ = sleep_until(deadline) => return None,
            };
            match event {
                Err(Ok(status)) => {
                    self.exited(status);
                    return Some(status);
                }
                Err(Err(_)) => return None,
                Ok(None) => self.stdout_closed = true,
                Ok(Some((at, inbound))) => {
                    self.handle(at, inbound, deadline).await;
                }
            }
        }
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

    /// Ends the session: stops the server if it is still running, waits up to
    /// `grace` for its descendants to exit, records any still alive, and stops them.
    pub async fn finish(mut self, grace: Duration) -> Finish {
        self.stdin = None;
        let mut killed = false;
        if self.is_alive() {
            killed = true;
            self.note(STOPPING_NOTE);
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
        let scan = |group: &Group| -> Result<Vec<ProcInfo>, String> {
            process::reap_adopted();
            let mut left = group.members()?;
            for p in process::adopted_orphans()? {
                if !left.iter().any(|q| q.pid == p.pid) {
                    left.push(p);
                }
            }
            left.retain(|p| p.pid != pid);
            Ok(left)
        };
        let deadline = Instant::now() + grace;
        let leftovers = loop {
            match scan(&self.group) {
                Err(e) => {
                    scan_error = Some(e);
                    break Vec::new();
                }
                Ok(left) if left.is_empty() || Instant::now() >= deadline => break left,
                Ok(_) => sleep(Duration::from_millis(50)).await,
            }
        };
        for p in &leftovers {
            self.note(format!("still running after the server exited: {p}"));
        }
        // Stop what's left: the group by its id (safe while the sentinel holds
        // it), and adopted processes, which are lspcheck's unreaped children
        // and so can't have been replaced by another process. Stopping one can
        // orphan its own children, which are then adopted too, so repeat for
        // up to two seconds.
        let deadline = Instant::now() + Duration::from_secs(2);
        let me = std::process::id() as i32;
        let mut not_stopped = Vec::new();
        loop {
            #[cfg(unix)]
            self.group.signal(nix::sys::signal::Signal::SIGKILL);
            let left = scan(&self.group).unwrap_or_default();
            if left.is_empty() {
                break;
            }
            if Instant::now() >= deadline {
                not_stopped = left;
                break;
            }
            for p in left.iter().filter(|p| p.ppid == me) {
                if let Err(e) = process::kill_process(p) {
                    self.note(e);
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
        for p in &not_stopped {
            self.note(format!("lspcheck couldn't stop {p}"));
        }

        // Let the stdout task see the end of the stream, so its evidence is
        // complete. Every process has been stopped, so only parsing what is
        // already buffered remains; the bound covers a process lspcheck
        // couldn't stop that still holds the pipe.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.evidence.lock().expect("evidence lock").done && Instant::now() < deadline {
            sleep(Duration::from_millis(10)).await;
        }
        // The stdout task has stopped (or the bound above expired), so
        // everything it read is already queued. Record it all without waiting:
        // every process has been stopped, so no server request can be answered,
        // and the work is bounded by --max-output-mib.
        self.take_queued(false);
        self.group.close();
        let mut evidence = std::mem::take(&mut *self.evidence.lock().expect("evidence lock"));
        let now = self.start.elapsed();
        if !evidence.done && evidence.limit.is_none() {
            evidence.limit =
                Some((now, "stdout stayed open after the server exited, so later output wasn't inspected".into()));
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
        let transcript = std::mem::take(&mut *self.transcript.lock().expect("transcript lock"));
        let whole = evidence.done && !evidence.cut;
        let (miscounted, unchecked) = miscounted_frames(&evidence.unparsed, &transcript, whole);
        // Whether an unmatched reply is a duplicate, early, aborted or only
        // loosely equal needs the whole session's state, not the moment it
        // was read: a guessed id may be sent later, and a write may fail
        // after the reply.
        let unmatched = std::mem::take(&mut self.unmatched)
            .into_iter()
            .map(|(at, id)| {
                let sent_at = self.used_ids.get(&id).copied();
                Unmatched {
                    at,
                    duplicate: self.answers.contains_key(&id),
                    unsent: sent_at.is_none(),
                    early: sent_at.is_some_and(|sent| sent > at),
                    aborted: self.aborted.contains(&id),
                    loose: self.used_ids.keys().find(|sent| message::loosely_equal(&id, sent)).cloned(),
                    id,
                }
            })
            .collect();
        Finish {
            exit: if killed { None } else { self.exit },
            killed,
            leftovers,
            coverage,
            scan_error,
            not_stopped,
            limit: evidence.limit,
            own_limit: self.own_limit.take(),
            lost: evidence.lost,
            stopped: evidence.stopped,
            transcript,
            messages: std::mem::take(&mut self.messages),
            initialized_at: self.initialized_at,
            j3_incomplete,
            abandoned_at: self.abandoned,
            problems: evidence.problems,
            unmatched,
            answered: std::mem::take(&mut self.answered),
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
                Problem::Truncated { content_length, received }
            }
        };
        if !matches!(problem, Problem::Garbage(_)) {
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
        if stopped {
            continue;
        }
        let at = start.elapsed();
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
