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
use tokio::time::{Instant, sleep, timeout_at};

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
    Exited(String),
    Note(String),
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

/// What the stdout task observed, independently of what was consumed.
#[derive(Debug, Default)]
struct Evidence {
    problems: Vec<(Duration, Problem)>,
    content_types: Vec<(Duration, String)>,
    limit: Option<(Duration, String)>,
    /// Every frame skipped for its size, not only the first (`limit`).
    skipped: Vec<(Duration, String)>,
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
    pub transcript: Vec<Event>,
    /// Every message the session consumed.
    pub messages: Vec<(Duration, Message)>,
    /// Every stream problem in the whole of stdout.
    pub problems: Vec<(Duration, Problem)>,
    /// Responses that matched no outstanding request (unknown, early, or repeated ids).
    pub unmatched: Vec<(Duration, Id)>,
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
    /// When and why this session's own actions left evidence incomplete.
    own_limit: Option<(Duration, String)>,
    /// Set when a write stopped partway through a frame.
    abandoned: bool,
    /// Requests sent and not yet answered, with when they were sent.
    pending: HashMap<Id, Duration>,
    /// Requests answered: the index of the response in `messages`.
    answers: HashMap<Id, usize>,
    used_ids: HashSet<Id>,
    unmatched: Vec<(Duration, Id)>,
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
            own_limit: None,
            abandoned: false,
            pending: HashMap::new(),
            answers: HashMap::new(),
            used_ids: HashSet::new(),
            unmatched: Vec::new(),
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
        let during = |at: &Duration| (from..to).contains(at);
        let skipped = ev.skipped.iter().find(|(at, _)| during(at)).map(|(_, why)| why.clone());
        let unread = ev
            .problems
            .iter()
            .any(|(at, p)| during(at) && !matches!(p, Problem::Garbage(_)))
            .then(|| "some of the server's frames couldn't be read (see F1)".to_string());
        let since = [ev.stopped.as_ref(), self.own_limit.as_ref()]
            .into_iter()
            .flatten()
            .find(|(at, _)| *at < to)
            .map(|(_, why)| why.clone());
        skipped.or(unread).or(since)
    }

    /// Takes in everything the server has sent that was already read, without
    /// waiting for more.
    pub async fn take_in(&mut self) {
        self.absorb(Instant::now() + self.opts.write_timeout).await;
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
        if self.abandoned {
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
                self.abandoned = true;
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
        let body = serde_json::to_vec(value).expect("JSON values serialize");
        self.write_until(&transport::encode(&body), deadline).await
    }

    /// A fresh integer id in LSP's 32-bit range. Its unpredictability is what
    /// keeps a server from preparing a response before the request exists.
    pub fn fresh_id(&mut self) -> Id {
        loop {
            let n = (random_u64() % (i32::MAX as u64)) as i64 + 1;
            let id = Id::int(n);
            if self.used_ids.insert(id.clone()) {
                return id;
            }
        }
    }

    /// Whether lspcheck has sent a request with this id in this session.
    pub fn sent(&self, id: &Id) -> bool {
        self.used_ids.contains(id)
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
        self.used_ids.insert(id.clone());
        self.answers.remove(id);
        self.pending.insert(id.clone(), self.start.elapsed());
        Ok(())
    }

    /// Forgets a request registered with [`Session::begin_request`] that couldn't be written.
    pub fn abort_request(&mut self, id: &Id) {
        self.pending.remove(id);
    }

    pub async fn notify(&mut self, method: &str, params: Option<Value>) -> Result<(), WriteError> {
        self.send(&message::notification(method, params)).await
    }

    /// Closes the server's stdin.
    pub fn close_stdin(&mut self) {
        self.stdin = None;
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
        let Some(id) = m.response_id() else { return };
        match self.pending.get(id) {
            Some(&sent) if at >= sent => {
                self.pending.remove(id);
                self.answers.insert(id.clone(), self.messages.len() - 1);
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
        if let Err(e) = self.send_until(&reply, deadline).await
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
            self.note("lspcheck is stopping the server");
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
        // Classify responses that arrived and weren't waited for, briefly.
        self.drain(Instant::now() + Duration::from_millis(200)).await;
        self.group.close();
        let mut evidence = std::mem::take(&mut *self.evidence.lock().expect("evidence lock"));
        if !evidence.done && evidence.limit.is_none() {
            evidence.limit = Some((
                self.start.elapsed(),
                "stdout stayed open after the server exited, so later output wasn't inspected".into(),
            ));
        }
        let transcript = std::mem::take(&mut *self.transcript.lock().expect("transcript lock"));
        let whole = evidence.done && !evidence.cut;
        let (miscounted, unchecked) = miscounted_frames(&evidence.unparsed, &transcript, whole);
        Finish {
            exit: if killed { None } else { self.exit },
            killed,
            leftovers,
            coverage,
            scan_error,
            not_stopped,
            limit: evidence.limit,
            own_limit: self.own_limit.take(),
            transcript,
            messages: std::mem::take(&mut self.messages),
            problems: evidence.problems,
            unmatched: std::mem::take(&mut self.unmatched),
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
    let stop = |at: Duration, why: String| {
        record(&t, at, EventKind::Note(why.clone()));
        let mut ev = evidence.lock().expect("evidence lock");
        ev.cut = true;
        ev.stopped.get_or_insert((at, why.clone()));
        ev.limit.get_or_insert((at, why));
        drop(ev);
        let _ = tx.send((at, Inbound::Stopped));
    };
    let deliver = |at: Duration, chunk: Chunk, stopped: &mut bool| {
        record(&t, at, parsed_event(&chunk));
        let mut ev = evidence.lock().expect("evidence lock");
        let content_types = |ev: &mut Evidence, headers: &[Header]| {
            for h in headers.iter().filter(|h| h.name.eq_ignore_ascii_case("content-type")) {
                ev.content_types.push((at, h.value.clone()));
            }
        };
        let problem = match chunk {
            Chunk::Frame { headers, body, start } => {
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
                ev.skipped.push((at, why.clone()));
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
        for chunk in reader.feed(&buf[..n]) {
            deliver(at, chunk, &mut stopped);
            if stopped {
                break;
            }
        }
    }
    let at = start.elapsed();
    if !stopped {
        for chunk in reader.finish() {
            deliver(at, chunk, &mut stopped);
        }
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
