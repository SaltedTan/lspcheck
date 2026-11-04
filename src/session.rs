// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! One run of the server under test: its pipes, a transcript of every byte,
//! and the messages and stream problems seen so far.
//!
//! Stdout and stderr are read by their own tasks for the whole session, so a
//! server that writes a lot can never block on a full pipe while lspcheck is
//! writing to it. Every write and every wait has an absolute deadline. Stdout
//! is inspected up to an output budget; past it, lspcheck keeps reading and
//! discarding so the server isn't blocked, and records that its evidence is
//! incomplete.

use crate::message::{self, Id, Kind, Message, codes};
use crate::process::{self, Coverage, Group, ProcInfo, ServerCommand};
use crate::transport::{self, Chunk, FrameReader};
use serde_json::{Value, json};
use std::collections::HashMap;
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
    /// What the frame reader made of the bytes read so far.
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
            Problem::BadHeader { block, reason } => {
                write!(f, "bad header ({reason}): {}", preview(block))
            }
            Problem::NotJson { body, error } => {
                write!(f, "frame body is not JSON ({error}): {}", preview(body))
            }
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
    /// Stdout has closed, or lspcheck stopped inspecting it (see [`Session::limit`]).
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
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::Closed => f.write_str("the server's stdin is closed"),
            WriteError::Timeout => f.write_str("the server stopped reading its stdin"),
            WriteError::Io(e) => write!(f, "writing to the server failed: {e}"),
        }
    }
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
    /// Why lspcheck's evidence for this session is incomplete, if it is.
    pub limit: Option<String>,
    /// Whether stdout was still open when lspcheck gave up waiting for it to close.
    pub stdout_left_open: bool,
    pub transcript: Vec<Event>,
    pub messages: Vec<(Duration, Message)>,
    pub problems: Vec<(Duration, Problem)>,
    /// Responses that matched no outstanding request (unknown, early, or repeated ids).
    pub unmatched: Vec<(Duration, Id)>,
    /// `Content-Type` values the server sent.
    pub content_types: Vec<(Duration, String)>,
}

type Transcript = Arc<Mutex<Vec<Event>>>;

fn record(t: &Transcript, at: Duration, kind: EventKind) {
    t.lock().expect("transcript lock").push(Event { at, kind });
}

enum Inbound {
    Chunk(Chunk),
    /// The output budget ran out; nothing more will be forwarded.
    Limit(String),
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
    /// Every message received, in order.
    pub messages: Vec<(Duration, Message)>,
    /// Every stream problem seen, in order.
    pub problems: Vec<(Duration, Problem)>,
    /// Why evidence is incomplete, once it is.
    pub limit: Option<String>,
    /// Requests sent and not yet answered, with when they were sent.
    pending: HashMap<Id, Duration>,
    /// Requests answered: the index of the response in `messages`.
    answers: HashMap<Id, usize>,
    unmatched: Vec<(Duration, Id)>,
    content_types: Vec<(Duration, String)>,
    group: Group,
    exit: Option<ExitStatus>,
    next_id: i64,
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

        let (tx, rx) = mpsc::unbounded_channel();
        let mut stdout = child.stdout.take().expect("piped stdout");
        let t = transcript.clone();
        let (max_body, max_output) = (opts.max_body, opts.max_output);
        tokio::spawn(async move {
            let mut reader = FrameReader::new(transport::DEFAULT_MAX_HEADER_BYTES, max_body);
            let mut buf = vec![0u8; 64 * 1024];
            let (mut total, mut over) = (0usize, false);
            loop {
                let n = match stdout.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                total += n;
                if over {
                    continue;
                }
                let at = start.elapsed();
                if total > max_output {
                    over = true;
                    let why = format!(
                        "the server wrote more than {max_output} bytes to stdout; lspcheck stopped inspecting it (--max-output-mib)"
                    );
                    record(&t, at, EventKind::Note(why.clone()));
                    let _ = tx.send((at, Inbound::Limit(why)));
                    continue;
                }
                record(&t, at, EventKind::Read(buf[..n].to_vec()));
                for chunk in reader.feed(&buf[..n]) {
                    record(&t, at, parsed_event(&chunk));
                    let _ = tx.send((at, Inbound::Chunk(chunk)));
                }
            }
            let at = start.elapsed();
            if !over {
                for chunk in reader.finish() {
                    record(&t, at, parsed_event(&chunk));
                    let _ = tx.send((at, Inbound::Chunk(chunk)));
                }
            }
            record(&t, at, EventKind::StdoutClosed);
            let _ = tx.send((at, Inbound::Closed));
        });

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
            messages: Vec::new(),
            problems: Vec::new(),
            limit: None,
            pending: HashMap::new(),
            answers: HashMap::new(),
            unmatched: Vec::new(),
            content_types: Vec::new(),
            group,
            exit: None,
            next_id: 1,
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

    /// Writes bytes exactly as given, within the write timeout.
    pub async fn write_raw(&mut self, bytes: &[u8]) -> Result<(), WriteError> {
        self.write_until(bytes, Instant::now() + self.opts.write_timeout).await
    }

    /// Writes bytes by `deadline` (or the write timeout, if sooner), recording
    /// exactly how many reached the pipe.
    async fn write_until(&mut self, bytes: &[u8], deadline: Instant) -> Result<(), WriteError> {
        let deadline = deadline.min(Instant::now() + self.opts.write_timeout);
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(WriteError::Closed);
        };
        let at = self.start.elapsed();
        let mut written = 0usize;
        let result = timeout_at(deadline, async {
            while written < bytes.len() {
                let n = stdin.write(&bytes[written..]).await?;
                if n == 0 {
                    return Err(std::io::Error::from(std::io::ErrorKind::WriteZero));
                }
                written += n;
            }
            stdin.flush().await
        })
        .await;
        if written > 0 {
            record(&self.transcript, at, EventKind::Sent(bytes[..written].to_vec()));
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
        }
        outcome
    }

    /// Frames and writes one message.
    pub async fn send(&mut self, value: &Value) -> Result<(), WriteError> {
        self.send_until(value, Instant::now() + self.opts.write_timeout).await
    }

    async fn send_until(&mut self, value: &Value, deadline: Instant) -> Result<(), WriteError> {
        let body = serde_json::to_vec(value).expect("JSON values serialize");
        self.write_until(&transport::encode(&body), deadline).await
    }

    /// The next integer id this session would use.
    pub fn fresh_id(&mut self) -> Id {
        let id = Id::int(self.next_id);
        self.next_id += 1;
        id
    }

    /// Sends a request, registering it so only a later response can answer it.
    pub async fn request(&mut self, method: &str, params: Option<Value>) -> Result<Id, WriteError> {
        let id = self.fresh_id();
        self.request_with_id(&id, method, params).await?;
        Ok(id)
    }

    pub async fn request_with_id(&mut self, id: &Id, method: &str, params: Option<Value>) -> Result<(), WriteError> {
        self.pending.insert(id.clone(), self.start.elapsed());
        self.send(&message::request(id, method, params)).await
    }

    pub async fn notify(&mut self, method: &str, params: Option<Value>) -> Result<(), WriteError> {
        self.send(&message::notification(method, params)).await
    }

    /// Closes the server's stdin.
    pub fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// Receives the next message or stream problem, answering server requests
    /// on the way when `auto_respond` is set. Nothing is processed after `deadline`.
    pub async fn recv(&mut self, deadline: Instant) -> Recv {
        loop {
            if self.stdout_closed {
                return Recv::Closed;
            }
            if Instant::now() >= deadline {
                return Recv::Timeout;
            }
            let (at, inbound) = match timeout_at(deadline, self.rx.recv()).await {
                Err(_) => return Recv::Timeout,
                Ok(None) | Ok(Some((_, Inbound::Closed))) => {
                    self.stdout_closed = true;
                    return Recv::Closed;
                }
                Ok(Some((_, Inbound::Limit(why)))) => {
                    self.limit.get_or_insert(why);
                    self.stdout_closed = true;
                    return Recv::Closed;
                }
                Ok(Some((at, Inbound::Chunk(chunk)))) => (at, chunk),
            };
            let problem = match inbound {
                Chunk::Frame { headers, body } => {
                    for h in headers.iter().filter(|h| h.name.eq_ignore_ascii_case("content-type")) {
                        self.content_types.push((at, h.value.clone()));
                    }
                    match Message::parse(&body) {
                        Ok(m) => {
                            self.accept(at, &m);
                            if self.opts.auto_respond
                                && let Kind::Request { id, method } = &m.kind
                            {
                                self.answer(id, method, &m.value, deadline).await;
                            }
                            return Recv::Message(m);
                        }
                        Err(error) => Problem::NotJson { body, error },
                    }
                }
                Chunk::Oversized { content_length, limit, .. } => {
                    let why = format!(
                        "a {content_length}-byte frame is over lspcheck's {limit}-byte inspection limit (--max-frame-mib), so it wasn't checked"
                    );
                    self.note(why.clone());
                    self.limit.get_or_insert(why);
                    continue;
                }
                Chunk::Garbage(b) => Problem::Garbage(b),
                Chunk::BadHeader { block, reason } => Problem::BadHeader { block, reason },
                Chunk::Truncated { content_length, received, .. } => Problem::Truncated { content_length, received },
            };
            self.problems.push((at, problem.clone()));
            return Recv::Problem(problem);
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
        if let Err(e) = self.send_until(&reply, deadline).await {
            self.note(format!("couldn't answer the server's {method} request: {e}"));
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

    /// Sends a request and waits for its response.
    pub async fn call(&mut self, method: &str, params: Option<Value>, timeout: Duration) -> Result<Message, CallError> {
        let deadline = Instant::now() + timeout;
        let id = self.request(method, params).await.map_err(CallError::Write)?;
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
        // Stop what's left. Stopping an adopted process can orphan its own
        // children, which are then adopted too, so repeat for up to two seconds.
        let deadline = Instant::now() + Duration::from_secs(2);
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
            for p in &left {
                if let Err(e) = process::kill_process(p) {
                    self.note(e);
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
        for p in &not_stopped {
            self.note(format!("lspcheck couldn't stop {p}"));
        }

        self.drain(Instant::now() + Duration::from_millis(500)).await;
        let stdout_left_open = !self.stdout_closed;
        self.group.close();
        let transcript = std::mem::take(&mut *self.transcript.lock().expect("transcript lock"));
        Finish {
            exit: if killed { None } else { self.exit },
            killed,
            leftovers,
            coverage,
            scan_error,
            not_stopped,
            limit: self.limit.take(),
            stdout_left_open,
            transcript,
            messages: std::mem::take(&mut self.messages),
            problems: std::mem::take(&mut self.problems),
            unmatched: std::mem::take(&mut self.unmatched),
            content_types: std::mem::take(&mut self.content_types),
        }
    }
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
    };
    EventKind::Parsed { kind, detail }
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
