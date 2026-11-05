// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! Re-running a recorded session against a server, usually a fixed build of
//! the same one, so its author can reproduce a failure without lspcheck's
//! scenario logic.
//!
//! The client's bytes are re-sent exactly, `out` event by `out` event, as
//! separate writes. Before each write, the replay waits for the responses the
//! original run had already read, matched by id, so a server that answers in
//! order is not flooded. Responses the original run read that answered the
//! server's own requests are not re-sent: the session's auto-answers answer
//! the new server's requests instead. Every wait is bounded. The replay does
//! not judge checks; it reports what the server did versus what the recording
//! holds.

use crate::checks::file_uri;
use crate::message::{Id, Kind, Message, Outcome};
use crate::process::ServerCommand;
use crate::session::{self, Options, Session};
use crate::transcript;
use crate::transport::{Chunk, FrameReader};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;
use tokio::time::{Instant, sleep};

/// A recorded workspace: its absolute path and the files in it.
#[derive(Debug, Clone)]
pub struct Workspace {
    pub path: PathBuf,
    /// (relative name, text) pairs.
    pub files: Vec<(String, String)>,
}

/// One `out` event of the recording, in order.
#[derive(Debug)]
pub struct Step {
    /// The bytes that reached the server's stdin.
    pub bytes: Vec<u8>,
    /// A pause to keep before this write, reproducing a deliberate gap in the
    /// recording (split writes are 10 ms apart).
    pub pause: Duration,
    /// The responses the original run had read before this write; the replay
    /// waits for them, matched by id.
    pub gates: Vec<Id>,
    /// Request occurrences whose first byte is in this event.
    pub requests: Vec<usize>,
    /// The event is entirely responses to the server's own requests; the
    /// replay's auto-answers replace it.
    pub skip: bool,
}

/// One request frame occurrence in the recording.
#[derive(Debug)]
pub struct RequestOccurrence {
    pub id: Id,
    first_step: usize,
    frame_start: usize,
    complete_step: usize,
    pub complete_line: usize,
    start_line: usize,
    pub recorded: Option<String>,
}

/// The recorded stdin close and the responses lspcheck had read before it.
#[derive(Debug)]
pub struct StdinClose {
    pub after_steps: usize,
    pub gates: Vec<Id>,
}

/// What a transcript says to do.
#[derive(Debug)]
pub struct Plan {
    /// The workspace to recreate, when the recording has one.
    pub workspace: Option<Workspace>,
    pub steps: Vec<Step>,
    /// The request frame occurrences in send order.
    pub requests: Vec<RequestOccurrence>,
    pub stdin_close: Option<StdinClose>,
    /// The recorded exit status, when the server exited.
    pub exit: Option<String>,
    /// The server-to-client requests the original run answered.
    pub server_requests: usize,
}

/// One request's recorded outcome versus the replayed one.
#[derive(Debug, Clone)]
pub struct RequestOutcome {
    pub id: String,
    pub recorded: String,
    pub replayed: String,
}

/// What the replay did, for the summary.
#[derive(Debug)]
pub struct Summary {
    pub input: PathBuf,
    pub output: PathBuf,
    pub server: String,
    /// The workspace recreated for the replay, and whether it was removed.
    pub workspace: Option<(PathBuf, bool)>,
    pub requests: Vec<RequestOutcome>,
    /// The exit status recorded, and the one the replay saw.
    pub exit: (Option<String>, String),
    /// The server-to-client requests answered, recorded and replayed.
    pub server_requests: (usize, usize),
}

impl Summary {
    pub fn render(&self) -> String {
        let mut s = format!("Replayed {} against {}\n", self.input.display(), self.server);
        match &self.workspace {
            Some((path, removed)) => {
                let tail = if *removed { "recreated, removed" } else { "recreated, still present" };
                s.push_str(&format!("  workspace: {} ({tail})\n", path.display()));
            }
            None => s.push_str("  workspace: none recorded; paths in the recorded bytes may not exist\n"),
        }
        s.push_str("  requests:\n");
        for r in &self.requests {
            s.push_str(&format!("    {}: {} -> {}\n", r.id, r.recorded, r.replayed));
        }
        let recorded = self.exit.0.as_deref().unwrap_or("no exit");
        s.push_str(&format!("  exit: {recorded} -> {}\n", self.exit.1));
        let (rec, rep) = self.server_requests;
        s.push_str(&format!("  server-to-client requests answered: {rec} -> {rep}\n"));
        s.push_str("  replay does not judge checks; to judge, run: lspcheck --only <ID> -- ");
        s.push_str(&self.server);
        s.push('\n');
        s
    }
}

/// How a response ended, for the summary.
fn describe(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Result(_) => "result".into(),
        Outcome::Error { code: Some(code), .. } => format!("error {code}"),
        Outcome::Error { .. } => "an error without an integer code".into(),
        Outcome::Malformed(_) => "a malformed response".into(),
    }
}

fn is_skippable_response(message: &Message) -> bool {
    matches!(&message.kind, Kind::Response { outcome: Outcome::Result(_) | Outcome::Error { .. }, .. })
}

fn first_response_in_window(
    id: &Id,
    start: Duration,
    end: Option<Duration>,
    messages: &[(Duration, Message)],
) -> Option<String> {
    messages
        .iter()
        .find(|(at, message)| *at >= start && end.is_none_or(|end| *at < end) && message.response_id() == Some(id))
        .and_then(|(_, message)| match &message.kind {
            Kind::Response { outcome, .. } => Some(describe(outcome)),
            _ => None,
        })
}

async fn wait_gates(session: &mut Session, gates: &[Id], timeout: Duration) {
    for id in gates {
        if session.wait_response(id, Instant::now() + timeout).await.is_err() {
            session.note(format!("no reply to id {id} within {} s (--timeout)", timeout.as_secs()));
        }
    }
}

/// Reads a transcript and works out what to do: which bytes to send, which to
/// skip, and which recorded replies gate each write.
pub fn parse(path: &Path) -> Result<Plan, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("can't read {}: {e}", path.display()))?;
    let mut plan = Plan {
        workspace: None,
        steps: Vec::new(),
        requests: Vec::new(),
        stdin_close: None,
        exit: None,
        server_requests: 0,
    };
    let mut sent_ids: HashSet<Id> = HashSet::new();
    let mut gates: Vec<Id> = Vec::new();
    let mut responses = Vec::new();
    let mut in_reader = FrameReader::default();
    let mut out_reader = FrameReader::default();
    let mut last_out_at: Option<f64> = None;
    let mut out_offset = 0usize;
    let mut out_events = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        if raw.trim().is_empty() {
            continue;
        }
        let line = n + 1;
        let v: Value =
            serde_json::from_str(raw).map_err(|e| format!("{}: line {line}: not JSON: {e}", path.display()))?;
        match v.get("dir").and_then(Value::as_str) {
            Some("out") => {
                let bytes = event_bytes(&v)
                    .ok_or_else(|| format!("{}: line {line}: an out event has no bytes", path.display()))?;
                let at = v.get("t_ms").and_then(Value::as_f64);
                // A gap under 200 ms between two writes is a deliberate pause
                // (split writes are 10 ms apart), so it is reproduced.
                let pause = match last_out_at {
                    Some(last) => {
                        at.filter(|&t| t > last && t - last < 200.0).map(|t| Duration::from_millis((t - last) as u64))
                    }
                    None => None,
                }
                .unwrap_or_default();
                last_out_at = at;
                let before_idle = out_reader.is_idle();
                let chunks = out_reader.feed(&bytes);
                let after_idle = out_reader.is_idle();
                let frames_only = !chunks.is_empty() && chunks.iter().all(|c| matches!(c, Chunk::Frame { .. }));
                let messages: Vec<Option<Message>> = chunks
                    .iter()
                    .map(|chunk| match chunk {
                        Chunk::Frame { body, .. } => Message::parse(body).ok(),
                        _ => None,
                    })
                    .collect();
                let skip = before_idle
                    && after_idle
                    && frames_only
                    && messages.iter().all(|message| message.as_ref().is_some_and(is_skippable_response));
                if skip {
                    plan.server_requests += messages.len();
                }
                let step_index = plan.steps.len();
                let event_start = out_offset;
                let event_end = event_start + bytes.len();
                out_offset = event_end;
                out_events.push((event_start, event_end, step_index, n));
                for (chunk, message) in chunks.iter().zip(messages.iter()) {
                    if let (Chunk::Frame { frame_start, .. }, Some(Message { kind: Kind::Request { id, .. }, .. })) =
                        (chunk, message)
                    {
                        plan.requests.push(RequestOccurrence {
                            id: id.clone(),
                            first_step: 0,
                            frame_start: *frame_start,
                            complete_step: step_index,
                            complete_line: n,
                            start_line: 0,
                            recorded: None,
                        });
                        sent_ids.insert(id.clone());
                    }
                }
                plan.steps.push(Step { bytes, pause, gates: std::mem::take(&mut gates), requests: Vec::new(), skip });
            }
            Some("in") => {
                let bytes = event_bytes(&v)
                    .ok_or_else(|| format!("{}: line {line}: an in event has no bytes", path.display()))?;
                for chunk in in_reader.feed(&bytes) {
                    let Chunk::Frame { body, .. } = chunk else { continue };
                    let Ok(Message { kind: Kind::Response { id, outcome }, .. }) = Message::parse(&body) else {
                        continue;
                    };
                    // Only replies to requests lspcheck sent gate the replay;
                    // the original run never waited for the rest.
                    if sent_ids.contains(&id) && !gates.contains(&id) {
                        gates.push(id.clone());
                    }
                    responses.push((n, id, describe(&outcome)));
                }
            }
            Some("meta") => {
                if v.get("event").and_then(Value::as_str) == Some("exited") {
                    plan.exit = v.get("how").and_then(Value::as_str).map(str::to_string);
                }
                if let Some(dir) = v.get("workspace").and_then(Value::as_str) {
                    let mut files = Vec::new();
                    if let Some(entries) = v.get("files").and_then(Value::as_object) {
                        for (name, value) in entries {
                            let text = value.as_str().ok_or_else(|| {
                                format!("{}: line {line}: workspace file {name:?} is not text", path.display())
                            })?;
                            files.push((name.clone(), text.to_string()));
                        }
                    }
                    plan.workspace = Some(Workspace { path: PathBuf::from(dir), files });
                }
                if v.get("event").and_then(Value::as_str) == Some("stdin closed") {
                    plan.stdin_close =
                        Some(StdinClose { after_steps: plan.steps.len(), gates: std::mem::take(&mut gates) });
                }
            }
            _ => {}
        }
    }
    for (index, request) in plan.requests.iter_mut().enumerate() {
        let (first_step, start_line) = out_events
            .iter()
            .find(|(start, end, _, _)| *start <= request.frame_start && request.frame_start < *end)
            .map(|(_, _, step, line)| (*step, *line))
            .expect("every request frame starts in an out event");
        request.first_step = first_step;
        request.start_line = start_line;
        plan.steps[first_step].requests.push(index);
    }
    for index in 0..plan.requests.len() {
        let id = plan.requests[index].id.clone();
        let complete_line = plan.requests[index].complete_line;
        let next_start = plan.requests[index + 1..].iter().find(|next| next.id == id).map(|next| next.start_line);
        plan.requests[index].recorded = responses
            .iter()
            .find(|(line, response_id, _)| {
                response_id == &id && *line > complete_line && next_start.is_none_or(|next| *line < next)
            })
            .map(|(_, _, outcome)| outcome.clone());
    }
    Ok(plan)
}

/// The bytes of an `out`/`in`/`err` event, from `data` or `data_base64`.
fn event_bytes(v: &Value) -> Option<Vec<u8>> {
    if let Some(text) = v.get("data").and_then(Value::as_str) {
        return Some(text.as_bytes().to_vec());
    }
    v.get("data_base64").and_then(Value::as_str).and_then(transcript::base64_decode)
}

fn validate_workspace_files(files: &[(String, String)]) -> Result<Vec<PathBuf>, String> {
    let mut paths: Vec<PathBuf> = Vec::with_capacity(files.len());
    for (name, _) in files {
        let invalid = || format!("invalid workspace file key {name:?}");
        if name.is_empty()
            || name.starts_with('/')
            || name.starts_with('\\')
            || (name.as_bytes().get(1) == Some(&b':') && name.as_bytes()[0].is_ascii_alphabetic())
        {
            return Err(invalid());
        }
        let parts: Vec<&str> = name.split(['/', '\\']).collect();
        if parts.iter().any(|part| part.is_empty() || *part == "." || *part == "..") {
            return Err(invalid());
        }
        let mut path = PathBuf::new();
        for part in parts {
            path.push(part);
        }
        if path.is_absolute() || path.components().any(|part| !matches!(part, Component::Normal(_))) {
            return Err(invalid());
        }
        if paths.iter().any(|existing| path == *existing || path.starts_with(existing) || existing.starts_with(&path)) {
            return Err(format!("workspace file key {name:?} conflicts with another file key"));
        }
        paths.push(path);
    }
    Ok(paths)
}

/// A workspace guard removes only files and directories it created.
struct WorkspaceGuard {
    path: Option<PathBuf>,
    created_root: bool,
    created_files: Vec<PathBuf>,
    created_dirs: Vec<PathBuf>,
    cleaned: bool,
}

impl WorkspaceGuard {
    fn new(w: Option<&Workspace>) -> Result<WorkspaceGuard, String> {
        let mut guard = WorkspaceGuard {
            path: w.map(|w| w.path.clone()),
            created_root: false,
            created_files: Vec::new(),
            created_dirs: Vec::new(),
            cleaned: false,
        };
        if let Some(w) = w {
            let paths = validate_workspace_files(&w.files)?;
            match std::fs::symlink_metadata(&w.path) {
                Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                    return Err(format!("the workspace {} is not a real directory", w.path.display()));
                }
                Ok(_) => {
                    let mut entries = std::fs::read_dir(&w.path)
                        .map_err(|e| format!("can't read workspace {}: {e}", w.path.display()))?;
                    if entries.next().is_some() {
                        return Err(format!("the workspace {} already exists and is not empty", w.path.display()));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    let parent = w.path.parent().unwrap_or_else(|| Path::new("."));
                    if !parent.is_dir() {
                        return Err(format!("the parent directory {} does not exist", parent.display()));
                    }
                    std::fs::create_dir(&w.path)
                        .map_err(|e| format!("can't create workspace {}: {e}", w.path.display()))?;
                    guard.created_root = true;
                }
                Err(e) => return Err(format!("can't inspect workspace {}: {e}", w.path.display())),
            }
            guard.write_files(w, &paths)?;
        }
        Ok(guard)
    }

    fn write_files(&mut self, w: &Workspace, paths: &[PathBuf]) -> Result<(), String> {
        for ((name, text), relative) in w.files.iter().zip(paths) {
            let root = self.path.as_ref().expect("workspace path");
            let mut parent = root.clone();
            if let Some(relative_parent) = relative.parent() {
                for component in relative_parent.components() {
                    parent.push(component);
                    match std::fs::symlink_metadata(&parent) {
                        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
                        Ok(_) => return Err(format!("workspace parent {} is not a real directory", parent.display())),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            std::fs::create_dir(&parent)
                                .map_err(|e| format!("can't create workspace directory {}: {e}", parent.display()))?;
                            self.created_dirs.push(parent.clone());
                        }
                        Err(e) => return Err(format!("can't inspect workspace directory {}: {e}", parent.display())),
                    }
                }
            }
            let file_path = root.join(relative);
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&file_path)
                .map_err(|e| format!("can't create workspace file {name:?} at {}: {e}", file_path.display()))?;
            self.created_files.push(file_path.clone());
            file.write_all(text.as_bytes())
                .map_err(|e| format!("can't write workspace file {name:?} at {}: {e}", file_path.display()))?;
        }
        Ok(())
    }

    /// Removes created contents, and removes the workspace only if it created it.
    fn remove(&mut self) -> bool {
        if self.cleaned {
            return self.created_root && self.path.as_ref().is_none_or(|path| !path.exists());
        }
        self.cleaned = true;
        for file in self.created_files.drain(..).rev() {
            let _ = std::fs::remove_file(file);
        }
        for dir in self.created_dirs.drain(..).rev() {
            let _ = std::fs::remove_dir(dir);
        }
        if self.created_root
            && let Some(path) = &self.path
        {
            return match std::fs::remove_dir(path) {
                Ok(()) => true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
                Err(_) => false,
            };
        }
        false
    }
}

impl Drop for WorkspaceGuard {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

/// Re-runs a recorded session against `server` and returns what it saw.
pub async fn replay(
    input: &Path,
    server: &ServerCommand,
    timeout: Duration,
    exit_timeout: Duration,
    output: &Path,
) -> Result<Summary, String> {
    match std::fs::symlink_metadata(output) {
        Ok(_) => {
            return Err(format!(
                "output {} already exists; pass --transcript to choose another path or remove it",
                output.display()
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("can't inspect output {}: {e}", output.display())),
    }
    let plan = parse(input)?;
    let mut guard = WorkspaceGuard::new(plan.workspace.as_ref())?;
    let workspace = guard.path.clone();

    let mut cmd = server.clone();
    if let Some(dir) = &workspace {
        cmd.cwd = Some(dir.clone());
    }
    let opts = Options {
        sentinel: std::env::current_exe().ok(),
        workspace_folders: workspace
            .as_ref()
            .map(|root| json!([{"uri": file_uri(root), "name": "workspace"}]))
            .unwrap_or(Value::Null),
        ..Options::default()
    };
    let mut session = match Session::start(&cmd, opts).await {
        Ok(s) => s,
        Err(e) => return Err(format!("can't start {}: {e}", server.display())),
    };

    if let Some(recorded) = &plan.workspace {
        session.record_workspace(recorded.path.clone(), recorded.files.clone());
    }
    // Each recorded event, in order, waits for the replies the original run
    // had already read, including events replaced by the session's auto-answer.
    let mut sending = true;
    let mut close = plan.stdin_close;
    let mut send_times: Vec<Option<Duration>> = vec![None; plan.requests.len()];
    let mut registered = vec![false; plan.requests.len()];
    let mut registration_failed = vec![None; plan.requests.len()];
    for (step_index, step) in plan.steps.iter().enumerate() {
        if close.as_ref().is_some_and(|event| event.after_steps == step_index) {
            let event = close.take().expect("checked close");
            wait_gates(&mut session, &event.gates, timeout).await;
            session.close_stdin();
        }
        wait_gates(&mut session, &step.gates, timeout).await;
        if !step.pause.is_zero() {
            sleep(step.pause).await;
        }
        if step.skip || !sending {
            continue;
        }
        for &request_index in &step.requests {
            let id = &plan.requests[request_index].id;
            match session.begin_request(id, Instant::now() + timeout).await {
                Ok(()) => {
                    registered[request_index] = true;
                }
                Err(session::WriteError::DuplicateId(_)) => {
                    registration_failed[request_index] = Some("not attributable (id reused while pending)");
                    session.note(format!("request id {id} was reused while pending; replay will still send it"));
                }
                Err(e) => {
                    registration_failed[request_index] = Some("not attributable (request registration failed)");
                    session
                        .note(format!("could not register request id {id} before replay write: {e}; sending anyway"));
                }
            }
            send_times[request_index] = Some(session.elapsed());
        }
        if let Err(e) = session.write_split(&step.bytes, &[], Duration::ZERO, Instant::now() + timeout).await {
            session.note(format!("replay stopped sending: {e}"));
            for (request_index, is_registered) in registered.iter_mut().enumerate() {
                if *is_registered && plan.requests[request_index].complete_step >= step_index {
                    session.abort_request(&plan.requests[request_index].id);
                    *is_registered = false;
                }
            }
            sending = false;
        }
    }
    if let Some(event) = close {
        wait_gates(&mut session, &event.gates, timeout).await;
        session.close_stdin();
    }

    // Drain any last registered requests once per id; gates above have already
    // waited for replies observed before later writes.
    let mut wait_ids = HashSet::new();
    for (index, request) in plan.requests.iter().enumerate() {
        if registered[index] {
            wait_ids.insert(request.id.clone());
        }
    }
    for id in wait_ids {
        let _ = session.wait_response(&id, Instant::now() + timeout).await;
    }
    if plan.exit.is_some() {
        let _ = session.wait_exit(Instant::now() + exit_timeout).await;
    }
    let finish = session.finish(Duration::from_secs(1)).await;

    transcript::write_new(output, &finish.transcript).map_err(|e| format!("can't write {}: {e}", output.display()))?;
    let workspace_removed = guard.remove();

    let requests = plan
        .requests
        .iter()
        .enumerate()
        .map(|(index, request)| {
            let next_send = plan.requests[index + 1..]
                .iter()
                .enumerate()
                .find(|(_, next)| next.id == request.id)
                .and_then(|(relative, _)| send_times[index + 1 + relative]);
            let replayed = registration_failed[index]
                .map(str::to_string)
                .or_else(|| {
                    send_times[index]
                        .and_then(|at| first_response_in_window(&request.id, at, next_send, &finish.messages))
                })
                .unwrap_or_else(|| "no reply".into());
            RequestOutcome {
                id: request.id.to_string(),
                recorded: request.recorded.clone().unwrap_or_else(|| "no reply".into()),
                replayed,
            }
        })
        .collect();
    let exit = match finish.exit {
        Some(status) => session::describe_status(status),
        None if finish.killed => "stopped by lspcheck".into(),
        None => "still running".into(),
    };
    let sent: Vec<u8> = finish
        .transcript
        .iter()
        .filter_map(|event| match &event.kind {
            session::EventKind::Sent(bytes) => Some(bytes.as_slice()),
            _ => None,
        })
        .flatten()
        .copied()
        .collect();
    let mut reader = FrameReader::default();
    let replayed_answers = reader
        .feed(&sent)
        .into_iter()
        .filter_map(|chunk| match chunk {
            Chunk::Frame { body, .. } => Message::parse(&body).ok(),
            _ => None,
        })
        .filter(|message| matches!(message.kind, Kind::Response { .. }))
        .count();
    let server_requests = (plan.server_requests, replayed_answers);
    Ok(Summary {
        input: input.to_path_buf(),
        output: output.to_path_buf(),
        server: server.display(),
        workspace: workspace.map(|p| (p, workspace_removed)),
        requests,
        exit: (plan.exit, exit),
        server_requests,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::request;
    use crate::transport;
    use serde_json::json;

    /// Frames a body the way a client would.
    fn frame(body: &Value) -> Vec<u8> {
        transport::encode(&serde_json::to_vec(body).expect("JSON values serialize"))
    }

    fn line(dir: &str, t_ms: f64, bytes: &[u8]) -> String {
        let mut v = json!({"t_ms": t_ms, "dir": dir});
        let obj = v.as_object_mut().expect("an object");
        match std::str::from_utf8(bytes) {
            Ok(text) => obj.insert("data".into(), text.into()),
            Err(_) => obj.insert("data_base64".into(), transcript::base64(bytes).into()),
        };
        v.to_string()
    }

    fn plan(jsonl: &str) -> Plan {
        let dir = tempfile::tempdir().expect("a tempdir");
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, jsonl).expect("writes the transcript");
        parse(&path).expect("parses")
    }

    #[test]
    fn parses_out_events_gates_and_skips() {
        let init = frame(&request(&Id::int(1), "initialize", Some(json!({}))));
        let reply = frame(&json!({"jsonrpc": "2.0", "id": 1, "result": {}}));
        // A response to the server's own request: the replay skips it.
        let answer = frame(&json!({"jsonrpc": "2.0", "id": "cfg", "result": []}));
        let notify = frame(&json!({"jsonrpc": "2.0", "method": "exit"}));
        let jsonl = [
            line("out", 0.0, &init),
            line("in", 5.0, &reply),
            line("out", 15.0, &answer),
            line("out", 25.0, &notify),
            r#"{"t_ms": 30.0, "dir": "meta", "event": "exited", "how": "status 0"}"#.to_string(),
        ]
        .join("\n");
        let p = plan(&jsonl);
        assert_eq!(p.requests.iter().map(|request| request.id.clone()).collect::<Vec<_>>(), vec![Id::int(1)]);
        assert_eq!(p.steps.len(), 3);
        assert!(!p.steps[0].skip);
        assert!(p.steps[1].skip, "a whole response frame is skipped");
        assert!(!p.steps[2].skip);
        assert_eq!(p.steps[0].gates, Vec::<Id>::new(), "nothing was read before the first write");
        assert_eq!(p.steps[1].gates, vec![Id::int(1)], "the reply read before the second write gates it");
        assert_eq!(p.steps[2].gates, Vec::<Id>::new(), "already waited for at the previous write");
        assert_eq!(p.steps[1].pause, Duration::from_millis(15), "a gap under 200 ms is reproduced");
        assert_eq!(p.steps[2].pause, Duration::from_millis(10));
        assert_eq!(p.requests[0].recorded.as_deref(), Some("result"));
        assert_eq!(p.exit.as_deref(), Some("status 0"));
        assert_eq!(p.server_requests, 1);
    }

    #[test]
    fn base64_bytes_are_decoded() {
        // Non-UTF-8 bytes are recorded as data_base64 and must be decoded.
        let req = frame(&request(&Id::int(7), "m", None));
        let garbage = b"\xff\xfe\x00garbage";
        let jsonl = [line("out", 0.0, &req), line("in", 1.0, garbage)].join("\n");
        let p = plan(&jsonl);
        assert_eq!(p.steps[0].bytes, req);
        assert_eq!(p.requests.iter().map(|request| request.id.clone()).collect::<Vec<_>>(), vec![Id::int(7)]);
    }

    #[test]
    fn a_workspace_line_is_recorded() {
        let jsonl = [
            r#"{"t_ms": 0.0, "dir": "meta", "workspace": "/tmp/ws", "files": {"doc.txt": "hello\n"}}"#.to_string(),
            line("out", 1.0, b""),
        ]
        .join("\n");
        let p = plan(&jsonl);
        let w = p.workspace.expect("a workspace");
        assert_eq!(w.path, PathBuf::from("/tmp/ws"));
        assert_eq!(w.files, vec![("doc.txt".to_string(), "hello\n".to_string())]);
    }

    #[test]
    fn a_gap_over_200_ms_is_not_a_pause() {
        let a = frame(&json!({"jsonrpc": "2.0", "method": "a"}));
        let b = frame(&json!({"jsonrpc": "2.0", "method": "b"}));
        let jsonl = [line("out", 0.0, &a), line("out", 300.0, &b)].join("\n");
        let p = plan(&jsonl);
        assert_eq!(p.steps[1].pause, Duration::ZERO);
    }

    #[test]
    fn a_reply_to_an_id_never_sent_does_not_gate() {
        let reply = frame(&json!({"jsonrpc": "2.0", "id": 99, "result": null}));
        let req = frame(&request(&Id::int(1), "m", None));
        let jsonl = [line("in", 0.0, &reply), line("out", 1.0, &req)].join("\n");
        let p = plan(&jsonl);
        assert_eq!(p.steps[0].gates, Vec::<Id>::new());
        assert!(p.requests[0].recorded.is_none());
    }

    #[test]
    fn out_frames_are_parsed_continuously_and_registered_at_their_first_byte() {
        let req = frame(&request(&Id::int(7), "m", None));
        let split = 14;
        let jsonl = [line("out", 0.0, &req[..split]), line("out", 10.0, &req[split..])].join("\n");
        let p = plan(&jsonl);
        assert_eq!(p.requests.len(), 1);
        assert_eq!(p.steps[0].requests, [0]);
        assert!(p.steps[1].requests.is_empty());
        assert_eq!(p.requests[0].first_step, 0);
    }

    #[test]
    fn response_plus_partial_request_or_garbage_is_not_skipped() {
        let response = frame(&json!({"jsonrpc": "2.0", "id": "cfg", "result": []}));
        let request = frame(&request(&Id::int(1), "m", None));
        let partial = &request[..request.len() / 2];
        let p = plan(&line("out", 0.0, &[response.as_slice(), partial].concat()));
        assert!(!p.steps[0].skip);

        let p = plan(&line("out", 0.0, &[response.as_slice(), b"garbage"].concat()));
        assert!(!p.steps[0].skip);
    }

    #[test]
    fn whole_responses_after_consumed_garbage_are_skipped() {
        let response = frame(&json!({"jsonrpc": "2.0", "id": "cfg", "result": []}));
        let p = plan(&[line("out", 0.0, b"server log\n"), line("out", 1.0, &response)].join("\n"));
        assert!(!p.steps[0].skip);
        assert!(p.steps[1].skip);
    }

    #[test]
    fn the_second_half_of_a_split_response_is_not_skipped() {
        let response = frame(&json!({"jsonrpc": "2.0", "id": "cfg", "result": []}));
        let split = response.len() / 2;
        let p = plan(&[line("out", 0.0, &response[..split]), line("out", 10.0, &response[split..])].join("\n"));
        assert!(!p.steps[0].skip);
        assert!(!p.steps[1].skip);
    }

    #[test]
    fn two_whole_responses_are_skipped() {
        let first = frame(&json!({"jsonrpc": "2.0", "id": "a", "result": []}));
        let second = frame(&json!({"jsonrpc": "2.0", "id": "b", "error": {"code": -32601}}));
        let p = plan(&line("out", 0.0, &[first, second].concat()));
        assert!(p.steps[0].skip);
        assert_eq!(p.server_requests, 2);
    }

    #[test]
    fn skipped_steps_keep_their_response_gates() {
        let req = frame(&request(&Id::int(1), "m", None));
        let response = frame(&json!({"jsonrpc": "2.0", "id": 1, "result": null}));
        let answer = frame(&json!({"jsonrpc": "2.0", "id": "cfg", "result": []}));
        let p = plan(&[line("out", 0.0, &req), line("in", 1.0, &response), line("out", 2.0, &answer)].join("\n"));
        assert!(p.steps[1].skip);
        assert_eq!(p.steps[1].gates, [Id::int(1)]);
    }

    #[test]
    fn a_stdin_closed_meta_event_keeps_its_position_and_gates() {
        let req = frame(&request(&Id::int(1), "m", None));
        let response = frame(&json!({"jsonrpc": "2.0", "id": 1, "result": null}));
        let jsonl = [
            line("out", 0.0, &req),
            line("in", 1.0, &response),
            r#"{"t_ms":2,"dir":"meta","event":"stdin closed"}"#.to_string(),
        ]
        .join("\n");
        let p = plan(&jsonl);
        let close = p.stdin_close.expect("stdin close");
        assert_eq!(close.after_steps, 1);
        assert_eq!(close.gates, [Id::int(1)]);
    }

    #[test]
    fn request_outcomes_are_per_occurrence_and_use_exact_id_equality() {
        let response = |json: &str| Message::parse(json.as_bytes()).expect("message");
        let messages = vec![
            (Duration::from_millis(1), response(r#"{"jsonrpc":"2.0","id":1,"result":null}"#)),
            (Duration::from_millis(3), response(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601}}"#)),
        ];
        let id = Id::int(1);
        assert_eq!(
            first_response_in_window(&id, Duration::ZERO, Some(Duration::from_millis(2)), &messages).as_deref(),
            Some("result")
        );
        assert_eq!(
            first_response_in_window(&id, Duration::from_millis(2), None, &messages).as_deref(),
            Some("error -32601")
        );
        assert_eq!(
            first_response_in_window(
                &id,
                Duration::from_millis(2),
                None,
                &[(Duration::ZERO, response(r#"{"jsonrpc":"2.0","id":1,"result":null}"#))],
            ),
            None
        );
        // Session ids compare numeric JSON values exactly, so 1 and 1.0 are the same id.
        assert_eq!(
            first_response_in_window(
                &id,
                Duration::ZERO,
                None,
                &[(Duration::from_millis(1), response(r#"{"jsonrpc":"2.0","id":1.0,"result":null}"#))],
            )
            .as_deref(),
            Some("result")
        );
    }

    #[test]
    fn recorded_outcomes_stop_at_the_next_occurrence_of_a_reused_id() {
        let req = frame(&request(&Id::int(1), "m", None));
        let result = frame(&json!({"jsonrpc": "2.0", "id": 1, "result": null}));
        let error = frame(&json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32601}}));
        let p = plan(
            &[line("out", 0.0, &req), line("in", 1.0, &result), line("out", 2.0, &req), line("in", 3.0, &error)]
                .join("\n"),
        );
        assert_eq!(p.requests.len(), 2);
        assert_eq!(p.requests[0].recorded.as_deref(), Some("result"));
        assert_eq!(p.requests[1].recorded.as_deref(), Some("error -32601"));
    }

    fn workspace(path: PathBuf, files: &[(&str, &str)]) -> Workspace {
        Workspace { path, files: files.iter().map(|(name, text)| ((*name).into(), (*text).into())).collect() }
    }

    #[test]
    fn invalid_workspace_keys_are_rejected_before_creation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspace");
        let escaped = dir.path().join("victim");
        for key in ["../victim", ""] {
            assert!(WorkspaceGuard::new(Some(&workspace(root.clone(), &[(key, "unsafe")]))).is_err());
            assert!(!root.exists());
            assert!(!escaped.exists());
        }
        let absolute = dir.path().join("absolute-victim");
        assert!(
            WorkspaceGuard::new(Some(&workspace(root.clone(), &[(absolute.to_str().unwrap(), "unsafe")]))).is_err()
        );
        assert!(!root.exists());
        assert!(!absolute.exists());
    }

    #[test]
    fn conflicting_workspace_keys_are_rejected_before_creation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspace");
        let w = workspace(root.clone(), &[("a", "one"), ("a/b", "two")]);
        assert!(WorkspaceGuard::new(Some(&w)).is_err());
        assert!(!root.exists());
    }

    #[test]
    fn a_workspace_with_a_missing_parent_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("missing/workspace");
        assert!(WorkspaceGuard::new(Some(&workspace(root.clone(), &[("doc.txt", "text")]))).is_err());
        assert!(!root.exists());
        assert!(!root.parent().unwrap().exists());
    }

    #[test]
    fn an_existing_empty_workspace_is_kept_but_its_files_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        let mut guard = WorkspaceGuard::new(Some(&workspace(root.clone(), &[("nested/doc.txt", "text")]))).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("nested/doc.txt")).unwrap(), "text");
        assert!(!guard.remove());
        assert!(root.is_dir());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_workspace_symlink_is_refused_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspace");
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        symlink(&target, &root).unwrap();
        assert!(WorkspaceGuard::new(Some(&workspace(root.clone(), &[("doc.txt", "text")]))).is_err());
        assert!(std::fs::symlink_metadata(&root).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_dir(target).unwrap().count(), 0);
    }

    #[test]
    fn partially_written_workspace_is_cleaned_up() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspace");
        // An embedded NUL is a normal path component but the OS refuses to create it.
        let w = workspace(root.clone(), &[("a", "one"), ("\0", "two")]);
        assert!(WorkspaceGuard::new(Some(&w)).is_err());
        assert!(!root.exists());
    }
}
