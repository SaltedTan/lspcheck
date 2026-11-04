// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! The check catalogue and the scenarios that exercise it.
//!
//! A scenario is one server process driven through one script; it reports a
//! verdict for each check it exercises. A check that a scenario couldn't reach
//! (because the server failed earlier) gets `error` or `not-exercised`, never
//! `pass`.

use crate::message::{Kind, Message, Outcome, codes};
use crate::process::{self, ServerCommand};
use crate::session::{self, CallError, Finish, Options, Session};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::time::{Instant, sleep};

/// Why a check exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Basis {
    /// A normative rule of the LSP or JSON-RPC specification.
    Must,
    /// A recommendation of the specification.
    Should,
    /// Hygiene the specification doesn't require.
    Policy,
}

impl Basis {
    pub fn label(self) -> &'static str {
        match self {
            Basis::Must => "MUST",
            Basis::Should => "SHOULD",
            Basis::Policy => "POLICY",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Area {
    Framing,
    JsonRpc,
    Lifecycle,
}

/// A check's identity and documentation.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct CheckInfo {
    pub id: &'static str,
    pub area: Area,
    pub basis: Basis,
    pub title: &'static str,
    /// The rule's section of the specification.
    pub spec: &'static str,
}

const LSP: &str =
    "https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/";

macro_rules! check {
    ($id:literal, $area:ident, $basis:ident, $title:literal, $anchor:literal) => {
        CheckInfo {
            id: $id,
            area: Area::$area,
            basis: Basis::$basis,
            title: $title,
            spec: concat!("#", $anchor),
        }
    };
}

/// Every check lspcheck knows, in report order.
pub const CATALOGUE: &[CheckInfo] = &[
    check!(
        "F1",
        Framing,
        Must,
        "Every byte on stdout belongs to a well-formed frame",
        "baseProtocol"
    ),
    check!(
        "J4",
        JsonRpc,
        Must,
        "An unknown request gets MethodNotFound (-32601), including a $/ request",
        "dollarRequests"
    ),
    check!(
        "L1",
        Lifecycle,
        Should,
        "A request before initialize gets ServerNotInitialized (-32002)",
        "initialize"
    ),
    check!(
        "L2",
        Lifecycle,
        Must,
        "initialize returns capabilities and an offered position encoding",
        "initialize"
    ),
    check!(
        "L4",
        Lifecycle,
        Must,
        "shutdown returns null and the server stays alive until exit",
        "shutdown"
    ),
    check!(
        "L6",
        Lifecycle,
        Should,
        "exit after shutdown ends the process with status 0",
        "exit"
    ),
    check!(
        "L7",
        Lifecycle,
        Should,
        "exit without shutdown ends the process with status 1",
        "exit"
    ),
    check!(
        "L9",
        Lifecycle,
        Policy,
        "No descendant process outlives the server",
        "exit"
    ),
];

pub fn info(id: &str) -> &'static CheckInfo {
    CATALOGUE
        .iter()
        .find(|c| c.id == id)
        .unwrap_or_else(|| panic!("unknown check {id}"))
}

pub fn spec_url(c: &CheckInfo) -> String {
    format!("{LSP}{}", c.spec)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    Pass,
    Fail,
    /// Not applicable to this server.
    Skip,
    /// Applicable, but the run produced no evidence either way.
    NotExercised,
    /// lspcheck or the environment failed; says nothing about the rule.
    Error,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::Fail => "FAIL",
            Verdict::Skip => "SKIP",
            Verdict::NotExercised => "N/E",
            Verdict::Error => "ERROR",
        }
    }
}

/// One check's outcome.
#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    pub id: &'static str,
    pub verdict: Verdict,
    pub detail: String,
    pub scenario: &'static str,
}

/// How a run is set up.
#[derive(Debug, Clone)]
pub struct Config {
    pub server: ServerCommand,
    pub sentinel: Option<PathBuf>,
    pub request_timeout: Duration,
    pub startup_timeout: Duration,
    pub exit_timeout: Duration,
    /// How long descendants may take to exit after the server does.
    pub leftover_grace: Duration,
}

/// The transcript and verdicts of one scenario.
#[derive(Debug)]
pub struct ScenarioRun {
    pub name: &'static str,
    pub results: Vec<CheckResult>,
    pub finish: Option<Finish>,
}

/// A scenario's name and the checks it reports.
pub struct Scenario {
    pub name: &'static str,
    pub checks: &'static [&'static str],
}

pub const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "lifecycle",
        checks: &["F1", "L2", "J4", "L4", "L6", "L9"],
    },
    Scenario {
        name: "pre-initialize",
        checks: &["L1"],
    },
    Scenario {
        name: "exit-without-shutdown",
        checks: &["L7"],
    },
];

/// Runs one scenario by name.
pub async fn run_scenario(name: &'static str, cfg: &Config) -> ScenarioRun {
    let scenario = SCENARIOS
        .iter()
        .find(|s| s.name == name)
        .expect("known scenario");
    let workspace = match tempfile::Builder::new().prefix("lspcheck-").tempdir() {
        Ok(dir) => dir,
        Err(e) => {
            return all(
                scenario,
                Verdict::Error,
                &format!("could not create a workspace: {e}"),
            );
        }
    };
    let root = workspace.path().to_path_buf();
    let mut opts = Options {
        sentinel: cfg.sentinel.clone(),
        ..Options::default()
    };
    opts.workspace_folders = json!([{"uri": file_uri(&root), "name": "workspace"}]);
    let mut cmd = cfg.server.clone();
    if cmd.cwd.is_none() {
        cmd.cwd = Some(root.clone());
    }
    let session = match Session::start(&cmd, opts).await {
        Ok(s) => s,
        Err(e) => {
            return all(
                scenario,
                Verdict::Error,
                &format!("could not start {}: {e}", cfg.server.display()),
            );
        }
    };
    let mut ctx = Ctx {
        s: session,
        cfg,
        root,
        results: Vec::new(),
        scenario: name,
    };
    match name {
        "lifecycle" => lifecycle(&mut ctx).await,
        "pre-initialize" => pre_initialize(&mut ctx).await,
        "exit-without-shutdown" => exit_without_shutdown(&mut ctx).await,
        _ => unreachable!(),
    }
    let Ctx { s, results, .. } = ctx;
    let finish = s.finish(cfg.leftover_grace).await;
    let mut results = results;
    if scenario.checks.contains(&"F1") {
        results.push(judge_f1(name, &finish));
    }
    if scenario.checks.contains(&"L9") {
        results.push(judge_l9(name, &finish));
    }
    for id in scenario.checks {
        if !results.iter().any(|r| r.id == *id) {
            results.push(result(
                name,
                id,
                Verdict::Error,
                "the scenario ended before reaching this check",
            ));
        }
    }
    drop(workspace);
    ScenarioRun {
        name,
        results,
        finish: Some(finish),
    }
}

fn all(scenario: &Scenario, verdict: Verdict, detail: &str) -> ScenarioRun {
    ScenarioRun {
        name: scenario.name,
        results: scenario
            .checks
            .iter()
            .map(|id| result(scenario.name, id, verdict, detail))
            .collect(),
        finish: None,
    }
}

fn result(
    scenario: &'static str,
    id: &str,
    verdict: Verdict,
    detail: impl Into<String>,
) -> CheckResult {
    CheckResult {
        id: info(id).id,
        verdict,
        detail: detail.into(),
        scenario,
    }
}

struct Ctx<'a> {
    s: Session,
    cfg: &'a Config,
    root: PathBuf,
    results: Vec<CheckResult>,
    scenario: &'static str,
}

impl Ctx<'_> {
    fn report(&mut self, id: &str, verdict: Verdict, detail: impl Into<String>) {
        let r = result(self.scenario, id, verdict, detail);
        self.results.push(r);
    }

    fn pass(&mut self, id: &str) {
        self.report(id, Verdict::Pass, "");
    }

    fn fail(&mut self, id: &str, detail: impl Into<String>) {
        self.report(id, Verdict::Fail, detail);
    }

    /// Initializes the server. On failure, returns why.
    async fn initialize(&mut self) -> Result<Message, String> {
        let params = initialize_params(&self.root);
        let reply = self
            .s
            .call("initialize", Some(params), self.cfg.startup_timeout)
            .await
            .map_err(|e| {
                format!(
                    "initialize: {}",
                    describe_call_error(&e, self.cfg.startup_timeout)
                )
            })?;
        match &reply.kind {
            Kind::Response {
                outcome: Outcome::Result(_),
                ..
            } => {}
            Kind::Response { outcome, .. } => {
                return Err(format!("initialize failed: {}", describe_outcome(outcome)));
            }
            _ => unreachable!("wait_response returns responses"),
        }
        self.s
            .notify("initialized", Some(json!({})))
            .await
            .map_err(|e| format!("initialized: {e}"))?;
        Ok(reply)
    }
}

/// Initialize, probe, shutdown, exit: the path every client takes.
async fn lifecycle(ctx: &mut Ctx<'_>) {
    let reply = match ctx.initialize().await {
        Ok(reply) => reply,
        Err(why) => {
            ctx.fail("L2", &why);
            return;
        }
    };
    judge_l2(ctx, &reply);

    let t = ctx.cfg.request_timeout;
    let mut problems = Vec::new();
    for method in ["lspcheck/unknownRequest", "$/lspcheck/unknownRequest"] {
        match ctx.s.call(method, Some(json!({})), t).await {
            Ok(m) => match &m.kind {
                Kind::Response {
                    outcome:
                        Outcome::Error {
                            code: Some(codes::METHOD_NOT_FOUND),
                            ..
                        },
                    ..
                } => {}
                Kind::Response { outcome, .. } => problems.push(format!(
                    "{method}: {} instead of MethodNotFound",
                    describe_outcome(outcome)
                )),
                _ => unreachable!(),
            },
            Err(e) => problems.push(format!("{method}: {}", describe_call_error(&e, t))),
        }
    }
    if problems.is_empty() {
        ctx.pass("J4");
    } else {
        ctx.fail("J4", problems.join("; "));
    }

    match ctx.s.call("shutdown", None, t).await {
        Ok(m) => {
            match &m.kind {
                Kind::Response {
                    outcome: Outcome::Result(Value::Null),
                    ..
                } => {
                    // The server must keep running so the reply can be delivered.
                    let replied = ctx.s.elapsed();
                    sleep(Duration::from_millis(300)).await;
                    if ctx.s.is_alive() {
                        ctx.pass("L4");
                    } else {
                        let after = ctx.s.elapsed().saturating_sub(replied);
                        ctx.fail("L4", format!("the server exited within {} ms of replying to shutdown, before exit", after.as_millis()));
                    }
                }
                Kind::Response { outcome, .. } => ctx.fail(
                    "L4",
                    format!("shutdown returned {}, not null", describe_outcome(outcome)),
                ),
                _ => unreachable!(),
            }
        }
        Err(e) => {
            ctx.fail("L4", format!("shutdown: {}", describe_call_error(&e, t)));
            return;
        }
    }

    if !ctx.s.is_alive() {
        ctx.report(
            "L6",
            Verdict::NotExercised,
            "the server exited before exit was sent (see L4)",
        );
        return;
    }
    if let Err(e) = ctx.s.notify("exit", None).await {
        ctx.report("L6", Verdict::Error, format!("could not send exit: {e}"));
        return;
    }
    let deadline = Instant::now() + ctx.cfg.exit_timeout;
    match ctx.s.wait_exit(deadline).await {
        Some(status) if status.code() == Some(0) => ctx.pass("L6"),
        Some(status) => ctx.fail(
            "L6",
            format!("exited with {}", session::describe_status(status)),
        ),
        None => ctx.fail(
            "L6",
            format!(
                "still running {} s after exit",
                ctx.cfg.exit_timeout.as_secs()
            ),
        ),
    }
}

/// A request before initialize.
async fn pre_initialize(ctx: &mut Ctx<'_>) {
    let t = ctx.cfg.request_timeout;
    match ctx
        .s
        .call("workspace/symbol", Some(json!({"query": ""})), t)
        .await
    {
        Ok(m) => match &m.kind {
            Kind::Response {
                outcome:
                    Outcome::Error {
                        code: Some(codes::SERVER_NOT_INITIALIZED),
                        ..
                    },
                ..
            } => ctx.pass("L1"),
            Kind::Response { outcome, .. } => ctx.fail(
                "L1",
                format!(
                    "got {} instead of ServerNotInitialized",
                    describe_outcome(outcome)
                ),
            ),
            _ => unreachable!(),
        },
        Err(e) => ctx.fail("L1", describe_call_error(&e, t)),
    }
    // Leave politely so the server isn't killed mid-thought.
    if ctx.initialize().await.is_ok() {
        let _ = ctx.s.call("shutdown", None, t).await;
        let _ = ctx.s.notify("exit", None).await;
        let _ = ctx.s.wait_exit(Instant::now() + ctx.cfg.exit_timeout).await;
    }
}

/// exit without shutdown.
async fn exit_without_shutdown(ctx: &mut Ctx<'_>) {
    if let Err(why) = ctx.initialize().await {
        ctx.report("L7", Verdict::Error, why);
        return;
    }
    if let Err(e) = ctx.s.notify("exit", None).await {
        ctx.report("L7", Verdict::Error, format!("could not send exit: {e}"));
        return;
    }
    match ctx.s.wait_exit(Instant::now() + ctx.cfg.exit_timeout).await {
        Some(status) if status.code() == Some(1) => ctx.pass("L7"),
        Some(status) => ctx.fail(
            "L7",
            format!("exited with {}", session::describe_status(status)),
        ),
        None => ctx.fail(
            "L7",
            format!(
                "still running {} s after exit",
                ctx.cfg.exit_timeout.as_secs()
            ),
        ),
    }
}

const OFFERED_ENCODINGS: [&str; 3] = ["utf-8", "utf-32", "utf-16"];

fn judge_l2(ctx: &mut Ctx<'_>, reply: &Message) {
    let Kind::Response {
        outcome: Outcome::Result(result),
        ..
    } = &reply.kind
    else {
        unreachable!()
    };
    let Some(caps) = result.get("capabilities") else {
        ctx.fail("L2", "the result has no capabilities");
        return;
    };
    if !caps.is_object() {
        ctx.fail("L2", format!("capabilities is {caps}, not an object"));
        return;
    }
    match caps.get("positionEncoding") {
        None => ctx.pass("L2"),
        Some(Value::String(enc)) if OFFERED_ENCODINGS.contains(&enc.as_str()) => ctx.pass("L2"),
        Some(other) => ctx.fail(
            "L2",
            format!("positionEncoding {other} is not one of the offered {OFFERED_ENCODINGS:?}"),
        ),
    }
}

fn judge_f1(scenario: &'static str, finish: &Finish) -> CheckResult {
    match finish.problems.first() {
        None => result(scenario, "F1", Verdict::Pass, ""),
        Some((at, p)) => {
            let more = finish.problems.len() - 1;
            let tail = if more > 0 {
                format!(" (and {more} more)")
            } else {
                String::new()
            };
            result(
                scenario,
                "F1",
                Verdict::Fail,
                format!("at {} ms: {p}{tail}", at.as_millis()),
            )
        }
    }
}

fn judge_l9(scenario: &'static str, finish: &Finish) -> CheckResult {
    if !process::tracks_descendants() {
        return result(
            scenario,
            "L9",
            Verdict::NotExercised,
            "lspcheck can't track descendants on this platform yet",
        );
    }
    if finish.killed {
        return result(
            scenario,
            "L9",
            Verdict::NotExercised,
            "lspcheck had to stop the server, so its cleanup wasn't tested",
        );
    }
    if finish.leftovers.is_empty() {
        return result(scenario, "L9", Verdict::Pass, "");
    }
    let list: Vec<String> = finish.leftovers.iter().map(|p| p.to_string()).collect();
    result(
        scenario,
        "L9",
        Verdict::Fail,
        format!("still running after the server exited: {}", list.join(", ")),
    )
}

fn describe_outcome(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Result(v) => format!("result {}", session::preview(v.to_string().as_bytes())),
        Outcome::Error {
            code: Some(code),
            message,
            ..
        } => {
            format!(
                "error {code}{}",
                message
                    .as_deref()
                    .map(|m| format!(" ({m})"))
                    .unwrap_or_default()
            )
        }
        Outcome::Error { error, .. } => format!("an error without an integer code: {error}"),
        Outcome::Malformed(why) => format!("a malformed response that {why}"),
    }
}

fn describe_call_error(e: &CallError, timeout: Duration) -> String {
    match e {
        CallError::Wait(session::WaitError::Timeout) => {
            format!("no response within {} s", timeout.as_secs())
        }
        other => other.to_string(),
    }
}

/// Initialize params for a client with common capabilities.
pub fn initialize_params(root: &Path) -> Value {
    let uri = file_uri(root);
    json!({
        "processId": std::process::id(),
        "clientInfo": {"name": "lspcheck", "version": env!("CARGO_PKG_VERSION")},
        "locale": "en",
        "rootPath": root.to_string_lossy(),
        "rootUri": uri,
        "workspaceFolders": [{"uri": uri, "name": "workspace"}],
        "trace": "off",
        "capabilities": {
            "general": {"positionEncodings": OFFERED_ENCODINGS},
            "workspace": {
                "configuration": true,
                "workspaceFolders": true,
                "applyEdit": false,
                "didChangeConfiguration": {"dynamicRegistration": false},
                "symbol": {"dynamicRegistration": false}
            },
            "window": {"workDoneProgress": true, "showMessage": {}, "showDocument": {"support": false}},
            "textDocument": {
                "synchronization": {"dynamicRegistration": false, "didSave": true, "willSave": false, "willSaveWaitUntil": false},
                "hover": {"contentFormat": ["markdown", "plaintext"]},
                "definition": {"linkSupport": false},
                "references": {},
                "documentSymbol": {"hierarchicalDocumentSymbolSupport": true},
                "completion": {"completionItem": {"snippetSupport": false}},
                "publishDiagnostics": {"relatedInformation": true, "versionSupport": true}
            }
        }
    })
}

/// A `file:` URI for an absolute path, percent-encoding all but unreserved bytes.
pub fn file_uri(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let mut out = String::from("file://");
    if !text.starts_with('/') {
        out.push('/');
    }
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) || (b == b':' && out.len() <= 10) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uris_encode_reserved_bytes() {
        assert_eq!(
            file_uri(Path::new("/tmp/a b/é")),
            "file:///tmp/a%20b/%C3%A9"
        );
        assert_eq!(file_uri(Path::new("C:\\x y")), "file:///C:/x%20y");
    }

    #[test]
    fn every_scenario_check_is_in_the_catalogue_once() {
        let mut seen = Vec::new();
        for s in SCENARIOS {
            for id in s.checks {
                info(id);
                assert!(!seen.contains(id), "{id} is reported by two scenarios");
                seen.push(id);
            }
        }
        assert_eq!(
            seen.len(),
            CATALOGUE.len(),
            "every catalogued check is reported by a scenario"
        );
    }
}
