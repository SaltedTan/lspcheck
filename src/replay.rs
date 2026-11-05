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
use std::cell::Cell;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
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
    /// The event is entirely responses to the server's own requests; the
    /// replay's auto-answers replace it.
    pub skip: bool,
}

/// What a transcript says to do.
#[derive(Debug)]
pub struct Plan {
    /// The workspace to recreate, when the recording has one.
    pub workspace: Option<Workspace>,
    pub steps: Vec<Step>,
    /// The request ids the original run sent, in order.
    pub sent: Vec<Id>,
    /// The recorded outcome of each request, by the id's spelling.
    pub recorded: BTreeMap<String, String>,
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

/// The frame bodies in some bytes, in order.
fn frames(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut reader = FrameReader::default();
    let mut bodies = Vec::new();
    for chunk in reader.feed(bytes) {
        if let Chunk::Frame { body, .. } = chunk {
            bodies.push(body);
        }
    }
    for chunk in reader.finish() {
        if let Chunk::Frame { body, .. } = chunk {
            bodies.push(body);
        }
    }
    bodies
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

/// Reads a transcript and works out what to do: which bytes to send, which to
/// skip, and which recorded replies gate each write.
pub fn parse(path: &Path) -> Result<Plan, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("can't read {}: {e}", path.display()))?;
    let mut plan = Plan {
        workspace: None,
        steps: Vec::new(),
        sent: Vec::new(),
        recorded: BTreeMap::new(),
        exit: None,
        server_requests: 0,
    };
    let mut sent_ids: HashSet<Id> = HashSet::new();
    let mut gates: Vec<Id> = Vec::new();
    let mut in_reader = FrameReader::default();
    let mut last_out_at: Option<f64> = None;
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
                let bodies = frames(&bytes);
                let skip = !bodies.is_empty()
                    && bodies
                        .iter()
                        .all(|b| matches!(Message::parse(b), Ok(m) if matches!(m.kind, Kind::Response { .. })));
                if skip {
                    plan.server_requests += bodies.len();
                }
                for body in &bodies {
                    if let Ok(Message { kind: Kind::Request { id, .. }, .. }) = Message::parse(body) {
                        plan.sent.push(id.clone());
                        sent_ids.insert(id);
                    }
                }
                plan.steps.push(Step { bytes, pause, gates: std::mem::take(&mut gates), skip });
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
                    if sent_ids.contains(&id) {
                        plan.recorded.entry(id.to_string()).or_insert_with(|| describe(&outcome));
                        if !gates.contains(&id) {
                            gates.push(id);
                        }
                    }
                }
            }
            Some("meta") => {
                if v.get("event").and_then(Value::as_str) == Some("exited") {
                    plan.exit = v.get("how").and_then(Value::as_str).map(str::to_string);
                }
                if let Some(dir) = v.get("workspace").and_then(Value::as_str) {
                    let files = v
                        .get("files")
                        .and_then(Value::as_object)
                        .into_iter()
                        .flatten()
                        .map(|(name, text)| (name.clone(), text.as_str().unwrap_or_default().to_string()))
                        .collect();
                    plan.workspace = Some(Workspace { path: PathBuf::from(dir), files });
                }
            }
            _ => {}
        }
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

/// Recreates a recorded workspace. An existing non-empty path is an error,
/// because the files in it would not be the recorded ones.
fn make_workspace(w: &Workspace) -> Result<(), String> {
    if w.path.exists() && std::fs::read_dir(&w.path).is_ok_and(|mut e| e.next().is_some()) {
        return Err(format!("the workspace {} already exists and is not empty", w.path.display()));
    }
    std::fs::create_dir_all(&w.path).map_err(|e| format!("can't create {}: {e}", w.path.display()))?;
    for (name, text) in &w.files {
        let path = w.path.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("can't create {}: {e}", parent.display()))?;
        }
        std::fs::write(&path, text).map_err(|e| format!("can't write {}: {e}", path.display()))?;
    }
    Ok(())
}

/// Removes a workspace again, reporting whether it is gone.
fn remove_workspace(path: &Path) -> bool {
    match std::fs::remove_dir_all(path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

/// A workspace that is removed even when the replay fails partway through.
/// Only a workspace the guard created is removed: refusing a pre-existing
/// one must leave it alone.
struct WorkspaceGuard {
    path: Option<PathBuf>,
    removed: Cell<bool>,
    created: Cell<bool>,
}

impl WorkspaceGuard {
    fn new(w: Option<&Workspace>) -> Result<WorkspaceGuard, String> {
        let guard =
            WorkspaceGuard { path: w.map(|w| w.path.clone()), removed: Cell::new(false), created: Cell::new(false) };
        if let Some(w) = w {
            make_workspace(w)?;
            guard.created.set(true);
        }
        Ok(guard)
    }

    /// Removes the workspace, reporting whether it is gone.
    fn remove(&self) -> bool {
        match &self.path {
            Some(path) if self.created.get() => {
                let removed = remove_workspace(path);
                self.removed.set(removed);
                removed
            }
            _ => true,
        }
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
    let plan = parse(input)?;
    let guard = WorkspaceGuard::new(plan.workspace.as_ref())?;
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

    // Each recorded write, in order, gated on the replies the original run
    // had already read. A write that fails ends the sending: the server is
    // gone or stopped reading, and nothing more can reach it.
    let mut sending = true;
    for step in &plan.steps {
        if step.skip || !sending {
            continue;
        }
        for id in &step.gates {
            match session.wait_response(id, Instant::now() + timeout).await {
                Ok(_) => {}
                Err(_) => session.note(format!("no reply to id {id} within {} s (--timeout)", timeout.as_secs())),
            }
        }
        if !step.pause.is_zero() {
            sleep(step.pause).await;
        }
        if let Err(e) = session.write_split(&step.bytes, &[], Duration::ZERO, Instant::now() + timeout).await {
            session.note(format!("replay stopped sending: {e}"));
            sending = false;
        }
    }

    // The rest of the recorded requests, then the exit the recording ends
    // with, if it has one.
    for id in &plan.sent {
        let _ = session.wait_response(id, Instant::now() + timeout).await;
    }
    if plan.exit.is_some() {
        let _ = session.wait_exit(Instant::now() + exit_timeout).await;
    }
    let finish = session.finish(Duration::from_secs(1)).await;

    transcript::write(output, &finish.transcript).map_err(|e| format!("can't write {}: {e}", output.display()))?;
    let workspace_removed = guard.remove();

    let requests = plan
        .sent
        .iter()
        .map(|id| {
            let replayed = finish
                .messages
                .iter()
                .find(|(_, m)| m.response_id() == Some(id))
                .map(|(_, m)| {
                    let Kind::Response { outcome, .. } = &m.kind else {
                        return "not a response".to_string();
                    };
                    describe(outcome)
                })
                .unwrap_or_else(|| "no reply".into());
            RequestOutcome {
                id: id.to_string(),
                recorded: plan.recorded.get(&id.to_string()).cloned().unwrap_or_else(|| "no reply".into()),
                replayed,
            }
        })
        .collect();
    let exit = match finish.exit {
        Some(status) => session::describe_status(status),
        None if finish.killed => "stopped by lspcheck".into(),
        None => "still running".into(),
    };
    let server_requests = (
        plan.server_requests,
        finish
            .transcript
            .iter()
            .filter_map(|e| match &e.kind {
                session::EventKind::Sent(b) => Some(b.as_slice()),
                _ => None,
            })
            .flat_map(frames)
            .filter_map(|b| Message::parse(&b).ok())
            .filter(|m| matches!(m.kind, Kind::Response { .. }))
            .count(),
    );
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
        assert_eq!(p.sent, vec![Id::int(1)]);
        assert_eq!(p.steps.len(), 3);
        assert!(!p.steps[0].skip);
        assert!(p.steps[1].skip, "a whole response frame is skipped");
        assert!(!p.steps[2].skip);
        assert_eq!(p.steps[0].gates, Vec::<Id>::new(), "nothing was read before the first write");
        assert_eq!(p.steps[1].gates, vec![Id::int(1)], "the reply read before the second write gates it");
        assert_eq!(p.steps[2].gates, Vec::<Id>::new(), "already waited for at the previous write");
        assert_eq!(p.steps[1].pause, Duration::from_millis(15), "a gap under 200 ms is reproduced");
        assert_eq!(p.steps[2].pause, Duration::from_millis(10));
        assert_eq!(p.recorded.get("1"), Some(&"result".to_string()));
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
        assert_eq!(p.sent, vec![Id::int(7)]);
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
        assert!(p.recorded.is_empty());
    }
}
