// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! One run of the server under test: its pipes, a transcript of every byte,
//! and the messages and stream problems seen so far.
//!
//! Stdout and stderr are read by their own tasks for the whole session, so a
//! server that writes a lot can never block on a full pipe while lspcheck is
//! writing to it, and every write has its own deadline.

use crate::message::{self, Id, Kind, Message, codes};
use crate::process::{self, Group, ProcInfo, ServerCommand};
use crate::transport::{self, Chunk, FrameReader};
use serde_json::{Value, json};
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
    pub write_timeout: Duration,
    pub max_body: usize,
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
    /// Bytes written to the server's stdin.
    Sent(Vec<u8>),
    /// Something read from the server's stdout.
    Received(Chunk),
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
            Problem::Truncated {
                content_length,
                received,
            } => {
                write!(
                    f,
                    "stream ended {received} bytes into a {content_length}-byte body"
                )
            }
        }
    }
}

/// A short, escaped rendering of some bytes.
pub fn preview(bytes: &[u8]) -> String {
    const MAX: usize = 80;
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX)]);
    let more = if bytes.len() > MAX { "…" } else { "" };
    format!("{:?}{more}", text)
}

/// What one receive produced.
#[derive(Debug)]
pub enum Recv {
    Message(Message),
    Problem(Problem),
    /// Stdout has closed.
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
    /// Whether stdout was still open when lspcheck gave up waiting for it to close.
    pub stdout_left_open: bool,
    pub transcript: Vec<Event>,
    pub messages: Vec<(Duration, Message)>,
    pub problems: Vec<(Duration, Problem)>,
}

type Transcript = Arc<Mutex<Vec<Event>>>;

/// A running server.
pub struct Session {
    child: Child,
    pid: i32,
    stdin: Option<ChildStdin>,
    rx: mpsc::UnboundedReceiver<(Duration, Option<Chunk>)>,
    stdout_closed: bool,
    start: Instant,
    transcript: Transcript,
    /// Every message received, in order.
    pub messages: Vec<(Duration, Message)>,
    /// Every stream problem seen, in order.
    pub problems: Vec<(Duration, Problem)>,
    group: Group,
    exit: Option<ExitStatus>,
    next_id: i64,
    opts: Options,
}

fn record(t: &Transcript, at: Duration, kind: EventKind) {
    t.lock().expect("transcript lock").push(Event { at, kind });
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
        let start = Instant::now();
        let transcript: Transcript = Arc::default();

        let (tx, rx) = mpsc::unbounded_channel();
        let mut stdout = child.stdout.take().expect("piped stdout");
        let t = transcript.clone();
        let max_body = opts.max_body;
        tokio::spawn(async move {
            let mut reader = FrameReader::new(transport::DEFAULT_MAX_HEADER_BYTES, max_body);
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match stdout.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let at = start.elapsed();
                        for chunk in reader.feed(&buf[..n]) {
                            record(&t, at, EventKind::Received(chunk.clone()));
                            let _ = tx.send((at, Some(chunk)));
                        }
                    }
                }
            }
            let at = start.elapsed();
            for chunk in reader.finish() {
                record(&t, at, EventKind::Received(chunk.clone()));
                let _ = tx.send((at, Some(chunk)));
            }
            record(&t, at, EventKind::StdoutClosed);
            let _ = tx.send((at, None));
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
                record(
                    &t,
                    start.elapsed(),
                    EventKind::Note(format!("{dropped} more stderr bytes not kept")),
                );
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
            group,
            exit: None,
            next_id: 1,
            opts,
        })
    }

    pub fn pid(&self) -> i32 {
        self.pid
    }

    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    pub fn note(&self, text: impl Into<String>) {
        record(
            &self.transcript,
            self.start.elapsed(),
            EventKind::Note(text.into()),
        );
    }

    /// Writes bytes exactly as given.
    pub async fn write_raw(&mut self, bytes: &[u8]) -> Result<(), WriteError> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(WriteError::Closed);
        };
        record(
            &self.transcript,
            self.start.elapsed(),
            EventKind::Sent(bytes.to_vec()),
        );
        let deadline = Instant::now() + self.opts.write_timeout;
        let result = timeout_at(deadline, async {
            stdin.write_all(bytes).await?;
            stdin.flush().await
        })
        .await;
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                self.stdin = None;
                Err(WriteError::Closed)
            }
            Ok(Err(e)) => Err(WriteError::Io(e.to_string())),
            Err(_) => Err(WriteError::Timeout),
        }
    }

    /// Frames and writes one message.
    pub async fn send(&mut self, value: &Value) -> Result<(), WriteError> {
        let body = serde_json::to_vec(value).expect("JSON values serialize");
        self.write_raw(&transport::encode(&body)).await
    }

    /// The next integer id this session would use.
    pub fn fresh_id(&mut self) -> Id {
        let id = Id::int(self.next_id);
        self.next_id += 1;
        id
    }

    pub async fn request(&mut self, method: &str, params: Option<Value>) -> Result<Id, WriteError> {
        let id = self.fresh_id();
        self.send(&message::request(&id, method, params)).await?;
        Ok(id)
    }

    pub async fn notify(&mut self, method: &str, params: Option<Value>) -> Result<(), WriteError> {
        self.send(&message::notification(method, params)).await
    }

    /// Closes the server's stdin.
    pub fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// Receives the next message or stream problem, answering server requests
    /// on the way when `auto_respond` is set.
    pub async fn recv(&mut self, deadline: Instant) -> Recv {
        if self.stdout_closed {
            return Recv::Closed;
        }
        let (at, chunk) = match timeout_at(deadline, self.rx.recv()).await {
            Err(_) => return Recv::Timeout,
            Ok(None) | Ok(Some((_, None))) => {
                self.stdout_closed = true;
                return Recv::Closed;
            }
            Ok(Some((at, Some(chunk)))) => (at, chunk),
        };
        let problem = match chunk {
            Chunk::Frame { body, .. } => match Message::parse(&body) {
                Ok(m) => {
                    self.messages.push((at, m.clone()));
                    if self.opts.auto_respond
                        && let Kind::Request { id, method } = &m.kind
                    {
                        self.answer(id, method, &m.value).await;
                    }
                    return Recv::Message(m);
                }
                Err(error) => Problem::NotJson { body, error },
            },
            Chunk::Garbage(b) => Problem::Garbage(b),
            Chunk::BadHeader { block, reason } => Problem::BadHeader { block, reason },
            Chunk::Truncated {
                content_length,
                body,
                ..
            } => Problem::Truncated {
                content_length,
                received: body.len(),
            },
        };
        self.problems.push((at, problem.clone()));
        Recv::Problem(problem)
    }

    async fn answer(&mut self, id: &Id, method: &str, request: &Value) {
        let reply = match method {
            "workspace/configuration" => {
                let n = request
                    .pointer("/params/items")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                message::response(id, Value::Array(vec![Value::Null; n]))
            }
            "workspace/workspaceFolders" => {
                message::response(id, self.opts.workspace_folders.clone())
            }
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
            _ => message::error_response(
                id,
                codes::METHOD_NOT_FOUND,
                "lspcheck does not handle this request",
            ),
        };
        let _ = self.send(&reply).await;
    }

    /// Waits for the response to `id`, which may already have arrived.
    pub async fn wait_response(
        &mut self,
        id: &Id,
        deadline: Instant,
    ) -> Result<Message, WaitError> {
        if let Some((_, m)) = self
            .messages
            .iter()
            .find(|(_, m)| m.response_id() == Some(id))
        {
            return Ok(m.clone());
        }
        loop {
            match self.recv(deadline).await {
                Recv::Message(m) if m.response_id() == Some(id) => return Ok(m),
                Recv::Message(_) | Recv::Problem(_) => {}
                Recv::Closed => return Err(WaitError::Closed),
                Recv::Timeout => return Err(WaitError::Timeout),
            }
        }
    }

    /// Sends a request and waits for its response.
    pub async fn call(
        &mut self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Message, CallError> {
        let id = self
            .request(method, params)
            .await
            .map_err(CallError::Write)?;
        self.wait_response(&id, Instant::now() + timeout)
            .await
            .map_err(CallError::Wait)
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
        record(
            &self.transcript,
            self.start.elapsed(),
            EventKind::Exited(describe_status(status)),
        );
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
                if self
                    .wait_exit(Instant::now() + Duration::from_millis(500))
                    .await
                    .is_none()
                {
                    self.group.signal(nix::sys::signal::Signal::SIGKILL);
                }
            }
            let _ = self.child.start_kill();
            let _ = self
                .wait_exit(Instant::now() + Duration::from_secs(5))
                .await;
        }

        let mut known = self.group.own.clone();
        known.push(self.pid);
        let deadline = Instant::now() + grace;
        let leftovers = loop {
            process::reap_adopted_zombies(&known);
            let mut left = self.group.members();
            for p in process::adopted_orphans(&known) {
                if !left.iter().any(|q| q.pid == p.pid) {
                    left.push(p);
                }
            }
            left.retain(|p| p.pid != self.pid);
            if left.is_empty() || Instant::now() >= deadline {
                break left;
            }
            sleep(Duration::from_millis(50)).await;
        };
        for p in &leftovers {
            self.note(format!("still running after the server exited: {p}"));
        }
        // Stop what's left. Killing an adopted process can orphan its own
        // children, which are then adopted too, so repeat until none remain.
        for _ in 0..20 {
            #[cfg(unix)]
            self.group.signal(nix::sys::signal::Signal::SIGKILL);
            let mut left = process::adopted_orphans(&known);
            left.extend(
                self.group
                    .members()
                    .into_iter()
                    .filter(|p| p.pid != self.pid),
            );
            if left.is_empty() {
                break;
            }
            for p in &left {
                process::kill_and_reap(p);
            }
            sleep(Duration::from_millis(20)).await;
        }
        process::reap_adopted_zombies(&known);

        self.drain(Instant::now() + Duration::from_millis(500))
            .await;
        let stdout_left_open = !self.stdout_closed;
        self.group.close();
        let transcript = std::mem::take(&mut *self.transcript.lock().expect("transcript lock"));
        Finish {
            exit: if killed { None } else { self.exit },
            killed,
            leftovers,
            stdout_left_open,
            transcript,
            messages: std::mem::take(&mut self.messages),
            problems: std::mem::take(&mut self.problems),
        }
    }
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
            CallError::Wait(WaitError::Closed) => {
                f.write_str("the server closed its stdout without responding")
            }
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
