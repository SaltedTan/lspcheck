// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! The check catalogue and the scenarios that exercise it.
//!
//! A scenario is one server process driven through one script. Most checks
//! belong to one scenario; the cross-cutting ones ([`CROSS_CUTTING`]) are
//! judged from what every scenario observed. A check that couldn't be reached
//! gets `error` or `not-exercised`, never `pass`, and a failure seen after
//! lspcheck hit one of its own inspection limits is reported as an error,
//! because the missing evidence might explain it.

use crate::documents::{self, Change, OpenDoc, Published, Source, SyncKind, Version};
use crate::message::{self, Id, Kind, Message, Outcome, codes};
use crate::process::{Coverage, ProcInfo, ServerCommand};
use crate::session::{self, CallError, Finish, Options, Problem, Recv, Session, Unmatched, WaitError, WriteError};
use crate::text::{self, ContentChange, Encoding, Position};
use crate::transcript;
use crate::transport;
use serde::Serialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::time::Instant;

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
    Cancellation,
    Documents,
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

const LSP: &str = "https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/";

macro_rules! check {
    ($id:literal, $area:ident, $basis:ident, $title:literal, $anchor:literal) => {
        CheckInfo { id: $id, area: Area::$area, basis: Basis::$basis, title: $title, spec: concat!("#", $anchor) }
    };
}

/// Every check lspcheck knows, in report order.
pub const CATALOGUE: &[CheckInfo] = &[
    check!("F1", Framing, Must, "Every byte on stdout belongs to a well-formed frame", "baseProtocol"),
    check!("F2", Framing, Must, "Content-Length counts the body's bytes, not its characters", "headerPart"),
    check!("F3", Framing, Must, "Input split across writes, down to single header bytes, is read", "baseProtocol"),
    check!("F4", Framing, Must, "Several frames in one write are all read", "baseProtocol"),
    check!("F5", Framing, Must, "Input with 2-, 3- and 4-byte UTF-8 characters keeps the stream in step", "headerPart"),
    check!("F6", Framing, Must, "A Content-Type header with charset=utf-8 from the client is accepted", "headerPart"),
    check!("F7", Framing, Should, "The legacy charset=utf8 spelling from the client is accepted", "contentPart"),
    check!("F8", Framing, Should, "A Content-Type the server sends declares UTF-8", "contentPart"),
    check!("J1", JsonRpc, Must, "A reply keeps its request's id: a number stays a number, exactly", "responseMessage"),
    check!("J2", JsonRpc, Must, "Exactly one response per request: none duplicated or unasked", "responseMessage"),
    check!("J3", JsonRpc, Must, "Notifications get no response, including unknown and $/ ones", "notificationMessage"),
    check!(
        "J4",
        JsonRpc,
        Must,
        "An unknown request gets MethodNotFound (-32601), including a $/ request",
        "dollarRequests"
    ),
    check!(
        "J5",
        JsonRpc,
        Must,
        "Envelopes are well formed: jsonrpc 2.0, one of result/error, an integer code",
        "responseMessage"
    ),
    check!("J6", JsonRpc, Must, "Concurrent requests are each answered with the right id", "requestMessage"),
    check!(
        "L1",
        Lifecycle,
        Should,
        "Before initialize, a request gets ServerNotInitialized (-32002) and a notification no reply",
        "initialize"
    ),
    check!("L2", Lifecycle, Must, "initialize returns capabilities and an offered position encoding", "initialize"),
    check!("L3", Lifecycle, Policy, "A second initialize gets an error and the server keeps serving", "initialize"),
    check!("L4", Lifecycle, Must, "shutdown returns null and the server keeps running until exit", "shutdown"),
    check!("L5", Lifecycle, Should, "A request after shutdown gets InvalidRequest (-32600)", "shutdown"),
    check!("L6", Lifecycle, Should, "exit after shutdown ends the process with status 0", "exit"),
    check!("L7", Lifecycle, Should, "exit without shutdown ends the process with status 1", "exit"),
    check!("L8", Lifecycle, Policy, "End of input without exit ends the process", "exit"),
    check!("L9", Lifecycle, Policy, "No descendant process outlives the server", "exit"),
    check!("C1", Cancellation, Must, "A cancelled request still gets exactly one response", "cancelRequest"),
    check!("C2", Cancellation, Must, "Cancelling an unknown or finished request produces nothing", "cancelRequest"),
    check!("C3", Cancellation, Must, "No response to $/cancelRequest itself", "cancelRequest"),
    check!(
        "D1",
        Documents,
        Must,
        "Positions fall on character boundaries of the text, in the negotiated encoding",
        "position"
    ),
    check!(
        "D2",
        Documents,
        Must,
        "A diagnostics version is one the document had when the notification was read",
        "publishDiagnosticsParams"
    ),
    check!("D3", Documents, Must, "After a didChange, positions refer to the changed text", "textDocumentSync"),
];

/// Checks judged from every scenario's observations.
pub const CROSS_CUTTING: &[&str] = &["F1", "F2", "F8", "J1", "J2", "J3", "J5", "L9"];

pub fn info(id: &str) -> &'static CheckInfo {
    CATALOGUE.iter().find(|c| c.id == id).unwrap_or_else(|| panic!("unknown check {id}"))
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
    /// lspcheck or the environment failed, or evidence is incomplete; says nothing about the rule.
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
    /// The scenario the evidence came from, or "all" for cross-cutting checks.
    pub scenario: &'static str,
    /// For a failure because something expected never arrived: when lspcheck
    /// stopped waiting. Evidence lspcheck didn't see before then could explain
    /// it. A failure seen directly has none.
    #[serde(skip)]
    pub absent_at: Option<Duration>,
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
    pub max_body: usize,
    pub max_output: usize,
    /// Write each scenario's transcript into this directory.
    pub transcripts: Option<PathBuf>,
}

/// What a scenario observed that cross-cutting checks use.
#[derive(Debug)]
pub struct Observations {
    pub problems: Vec<(Duration, Problem)>,
    pub content_types: Vec<(Duration, String)>,
    pub non_ascii: Option<Duration>,
    pub miscounted: Vec<(Duration, String)>,
    pub unchecked: usize,
    pub killed: bool,
    pub leftovers: Vec<ProcInfo>,
    pub coverage: Coverage,
    pub scan_error: Option<String>,
    pub not_stopped: Vec<ProcInfo>,
    pub limit: Option<(Duration, String)>,
    /// Every message the server sent, in order.
    pub messages: Vec<(Duration, Message)>,
    /// When lspcheck sent `initialized`, if it did.
    pub initialized_at: Option<Duration>,
    /// Why evidence since `initialized` is incomplete, if it is.
    pub j3_incomplete: Option<String>,
    /// When a write of lspcheck's that stopped partway through a frame began.
    pub abandoned_at: Option<Duration>,
    /// How the json-rpc scenario's dedicated J3 exercise went, if it ran.
    pub j3_exercise: Option<(Verdict, String)>,
    /// Responses that matched no outstanding request, classified.
    pub unmatched: Vec<Unmatched>,
    /// Requests that were answered, with when their response was read.
    pub answered: Vec<(Duration, Id)>,
}

/// One scenario's verdicts and observations.
#[derive(Debug)]
pub struct ScenarioRun {
    pub name: &'static str,
    pub results: Vec<CheckResult>,
    /// `None` when the server couldn't be started.
    pub observed: Option<Observations>,
    /// Where its transcript was written, if it was.
    pub transcript: Option<PathBuf>,
}

/// A scenario's name and the checks it alone reports.
pub struct Scenario {
    pub name: &'static str,
    pub checks: &'static [&'static str],
}

pub const SCENARIOS: &[Scenario] = &[
    Scenario { name: "lifecycle", checks: &["L2", "J4", "L4", "L6"] },
    Scenario { name: "json-rpc", checks: &["J6"] },
    Scenario { name: "pre-initialize", checks: &["L1"] },
    Scenario { name: "exit-without-shutdown", checks: &["L7"] },
    Scenario { name: "framing", checks: &["F4", "F6", "F7", "F3", "F5"] },
    Scenario { name: "after-initialize", checks: &["L3", "L5"] },
    Scenario { name: "cancellation", checks: &["C1", "C2", "C3"] },
    Scenario { name: "documents", checks: &["D1", "D2", "D3"] },
    Scenario { name: "end-of-input", checks: &["L8"] },
];

/// Runs the scenarios needed for `selected` (every check when empty) and
/// returns them with cross-cutting verdicts appended as a final "all" run.
pub async fn run(cfg: &Config, selected: &[String]) -> Result<Vec<ScenarioRun>, String> {
    let wants = |id: &str| selected.is_empty() || selected.iter().any(|s| s == id);
    let any_cross = CROSS_CUTTING.iter().any(|c| wants(c));
    let mut runs = Vec::new();
    for s in SCENARIOS {
        if any_cross || s.checks.iter().any(|c| wants(c)) {
            runs.push(run_scenario(s, cfg).await?);
        }
    }
    let cross: Vec<CheckResult> = [
        judge_f1(&runs),
        judge_f2(&runs),
        judge_f8(&runs),
        unless_unreadable(&runs, judge_j1(&runs)),
        unless_unreadable(&runs, judge_j2(&runs)),
        judge_j3(&runs),
        unless_unreadable(&runs, judge_j5(&runs)),
        judge_l9(&runs),
    ]
    .into_iter()
    .collect();
    runs.push(ScenarioRun { name: "all", results: cross, observed: None, transcript: None });
    for run in &mut runs {
        run.results.retain(|r| wants(r.id));
    }
    Ok(runs)
}

/// Runs one scenario. Errors only when its transcript can't be written.
async fn run_scenario(scenario: &'static Scenario, cfg: &Config) -> Result<ScenarioRun, String> {
    let name = scenario.name;
    let unstarted = |detail: String| ScenarioRun {
        name,
        results: scenario.checks.iter().map(|id| result(name, id, Verdict::Error, detail.clone())).collect(),
        observed: None,
        transcript: None,
    };
    let workspace = match tempfile::Builder::new().prefix("lspcheck-").tempdir() {
        Ok(dir) => dir,
        Err(e) => return Ok(unstarted(format!("could not create a workspace: {e}"))),
    };
    let root = workspace.path().to_path_buf();
    // The URIs lspcheck builds should match the ones a server derives from
    // the real path (on macOS, /var is /private/var). Windows' canonical
    // form is a verbatim path that doesn't make a usable URI, so it is left.
    #[cfg(unix)]
    let root = root.canonicalize().unwrap_or(root);
    let opts = Options {
        sentinel: cfg.sentinel.clone(),
        max_body: cfg.max_body,
        max_output: cfg.max_output,
        workspace_folders: json!([{"uri": file_uri(&root), "name": "workspace"}]),
        ..Options::default()
    };
    let mut cmd = cfg.server.clone();
    if cmd.cwd.is_none() {
        cmd.cwd = Some(root.clone());
    }
    let session = match Session::start(&cmd, opts).await {
        Ok(s) => s,
        Err(e) => {
            return Ok(unstarted(format!("could not start {}: {e}", cfg.server.display())));
        }
    };
    let mut ctx = Ctx { s: session, cfg, root, results: Vec::new(), scenario: name, j3_exercise: None };
    // The workspace, so a transcript of this scenario can be replayed in a
    // recreated one (files written later are recorded when they are written).
    ctx.s.record_workspace(ctx.root.clone(), workspace_files(&ctx.root));
    match name {
        "lifecycle" => lifecycle(&mut ctx).await,
        "json-rpc" => json_rpc(&mut ctx).await,
        "pre-initialize" => pre_initialize(&mut ctx).await,
        "exit-without-shutdown" => exit_without_shutdown(&mut ctx).await,
        "framing" => framing(&mut ctx).await,
        "after-initialize" => after_initialize(&mut ctx).await,
        "cancellation" => cancellation(&mut ctx).await,
        "documents" => documents(&mut ctx).await,
        "end-of-input" => end_of_input(&mut ctx).await,
        _ => unreachable!("unknown scenario {name}"),
    }
    let Ctx { s, mut results, j3_exercise, .. } = ctx;
    let finish = s.finish(cfg.leftover_grace).await;
    for id in scenario.checks {
        if !results.iter().any(|r| r.id == *id) {
            results.push(result(name, id, Verdict::Error, "the scenario ended before reaching this check"));
        }
    }
    // A failure because something never arrived might be explained by
    // evidence lspcheck didn't see before it stopped waiting: frames it
    // skipped, frames it couldn't read (a log line loses nothing, so it
    // doesn't count), or its own side of the conversation failing. A failure
    // seen directly, such as a wrong result, stands, and so does one whose
    // wait ended before anything went unseen.
    let unread = finish
        .problems
        .iter()
        .find(|(_, p)| !matches!(p, Problem::Garbage(_)))
        .map(|(at, _)| (*at, "some of the server's frames couldn't be read (see F1)".to_string()));
    let explanations: Vec<&(Duration, String)> =
        [&finish.limit, &finish.own_limit, &unread].into_iter().flatten().collect();
    for r in results.iter_mut().filter(|r| r.verdict == Verdict::Fail) {
        let Some(waited) = r.absent_at else { continue };
        if let Some((_, why)) = explanations.iter().filter(|(at, _)| *at <= waited).min_by_key(|(at, _)| *at) {
            r.verdict = Verdict::Error;
            r.detail = format!("inconclusive, because {why}: {}", r.detail);
        }
    }
    let transcript = match &cfg.transcripts {
        Some(dir) => {
            let path = dir.join(format!("{name}.jsonl"));
            transcript::write(&path, &finish.transcript).map_err(|e| format!("can't write {}: {e}", path.display()))?;
            Some(path)
        }
        None => None,
    };
    drop(workspace);
    Ok(ScenarioRun { name, results, observed: Some(observations(finish, j3_exercise)), transcript })
}

fn observations(f: Finish, j3_exercise: Option<(Verdict, String)>) -> Observations {
    Observations {
        problems: f.problems,
        content_types: f.content_types,
        non_ascii: f.non_ascii,
        miscounted: f.miscounted,
        unchecked: f.unchecked,
        killed: f.killed,
        leftovers: f.leftovers,
        coverage: f.coverage,
        scan_error: f.scan_error,
        not_stopped: f.not_stopped,
        limit: f.limit,
        messages: f.messages,
        initialized_at: f.initialized_at,
        j3_incomplete: f.j3_incomplete,
        abandoned_at: f.abandoned_at,
        j3_exercise,
        unmatched: f.unmatched,
        answered: f.answered,
    }
}

fn result(scenario: &'static str, id: &str, verdict: Verdict, detail: impl Into<String>) -> CheckResult {
    CheckResult { id: info(id).id, verdict, detail: detail.into(), scenario, absent_at: None }
}

struct Ctx<'a> {
    s: Session,
    cfg: &'a Config,
    root: PathBuf,
    results: Vec<CheckResult>,
    scenario: &'static str,
    /// How the json-rpc scenario's dedicated J3 exercise went.
    j3_exercise: Option<(Verdict, String)>,
}

/// Why initialization didn't succeed.
enum InitFailure {
    /// A protocol violation: the server broke a rule.
    Violation(String),
    /// No reply arrived, which unseen evidence might explain.
    Absent(String),
    /// A lawful outcome that still stops the scenario (an initialize error, a slow start).
    Blocked(String),
}

impl Ctx<'_> {
    fn report(&mut self, id: &str, verdict: Verdict, detail: impl Into<String>) {
        let r = result(self.scenario, id, verdict, detail);
        self.results.push(r);
    }

    fn pass(&mut self, id: &str) {
        self.report(id, Verdict::Pass, "");
    }

    fn pass_with(&mut self, id: &str, detail: impl Into<String>) {
        self.report(id, Verdict::Pass, detail);
    }

    fn fail(&mut self, id: &str, detail: impl Into<String>) {
        self.report(id, Verdict::Fail, detail);
    }

    /// A failure because something expected never arrived.
    fn fail_absent(&mut self, id: &str, detail: impl Into<String>) {
        let waited = self.s.elapsed();
        self.fail(id, detail);
        self.results.last_mut().expect("just reported").absent_at = Some(waited);
    }

    /// The verdict for a call that got no response: a failure when the request
    /// was delivered, and an error when lspcheck couldn't deliver it.
    fn fail_call(&mut self, id: &str, detail: impl Into<String>, e: &CallError) {
        match e {
            CallError::Wait(_) => self.fail_absent(id, detail),
            CallError::Write(_) => self.report(id, Verdict::Error, detail),
        }
    }

    /// Initializes the server and sends `initialized`.
    async fn initialize(&mut self) -> Result<Message, InitFailure> {
        let params = initialize_params(&self.root);
        self.initialize_with(params).await
    }

    /// Initializes the server with these params and sends `initialized`.
    async fn initialize_with(&mut self, params: Value) -> Result<Message, InitFailure> {
        let t = self.cfg.startup_timeout;
        let reply = match self.s.call("initialize", Some(params), t).await {
            Ok(reply) => reply,
            Err(CallError::Wait(WaitError::Timeout)) => {
                return Err(InitFailure::Blocked(format!(
                    "no reply to initialize within {} s (--startup-timeout)",
                    t.as_secs()
                )));
            }
            Err(CallError::Wait(WaitError::Closed)) => {
                return Err(InitFailure::Absent("the server closed stdout without replying to initialize".into()));
            }
            Err(e) => return Err(InitFailure::Blocked(format!("initialize: {e}"))),
        };
        match &reply.kind {
            Kind::Response { outcome: Outcome::Result(_), .. } => {}
            Kind::Response { outcome: outcome @ Outcome::Error { code: Some(_), message: Some(_), .. }, .. } => {
                // LSP lets a server refuse to initialize (InitializeError).
                return Err(InitFailure::Blocked(format!(
                    "the server refused to initialize with {}; it may need a workspace or settings lspcheck doesn't provide",
                    describe_outcome(outcome)
                )));
            }
            Kind::Response { outcome, .. } => {
                return Err(InitFailure::Violation(format!("initialize got {}", describe_outcome(outcome))));
            }
            _ => unreachable!("wait_response returns responses"),
        }
        if let Err(e) = self.s.notify("initialized", Some(json!({}))).await {
            return Err(InitFailure::Blocked(format!("initialized: {e}")));
        }
        Ok(reply)
    }
}

/// Initialize, probe, shutdown, exit: the path every client takes.
async fn lifecycle(ctx: &mut Ctx<'_>) {
    let reply = match ctx.initialize().await {
        Ok(reply) => reply,
        Err(InitFailure::Violation(why)) => return ctx.fail("L2", why),
        Err(InitFailure::Absent(why)) => return ctx.fail_absent("L2", why),
        Err(InitFailure::Blocked(why)) => return ctx.report("L2", Verdict::Error, why),
    };
    judge_l2(ctx, &reply);

    let t = ctx.cfg.request_timeout;
    let mut problems = Vec::new();
    let (mut seen_wrong, mut unsent) = (false, false);
    for method in ["lspcheck/unknownRequest", "$/lspcheck/unknownRequest"] {
        match ctx.s.call(method, Some(json!({})), t).await {
            Ok(m) => match &m.kind {
                Kind::Response { outcome: Outcome::Error { code: Some(codes::METHOD_NOT_FOUND), .. }, .. } => {}
                Kind::Response { outcome, .. } => {
                    seen_wrong = true;
                    problems.push(format!("{method}: {} instead of MethodNotFound", describe_outcome(outcome)))
                }
                _ => unreachable!(),
            },
            Err(e) => {
                unsent |= matches!(e, CallError::Write(_));
                problems.push(format!("{method}: {}", describe_call_error(&e, t)))
            }
        }
    }
    if problems.is_empty() {
        ctx.pass("J4");
    } else if seen_wrong {
        ctx.fail("J4", problems.join("; "));
    } else if unsent {
        ctx.report("J4", Verdict::Error, problems.join("; "));
    } else {
        ctx.fail_absent("J4", problems.join("; "));
    }

    match ctx.s.call("shutdown", None, t).await {
        Ok(m) => match &m.kind {
            Kind::Response { outcome: Outcome::Result(Value::Null), .. } => {
                // The server must keep running so the reply can be delivered.
                let watch = Duration::from_millis(300);
                match ctx.s.wait_exit(Instant::now() + watch).await {
                    None => ctx.pass("L4"),
                    Some(status) => ctx.fail(
                        "L4",
                        format!(
                            "the server exited with {} after replying to shutdown, before exit",
                            session::describe_status(status)
                        ),
                    ),
                }
            }
            Kind::Response { outcome, .. } => {
                ctx.fail("L4", format!("shutdown returned {}, not null", describe_outcome(outcome)))
            }
            _ => unreachable!(),
        },
        Err(e) => return ctx.fail_call("L4", format!("shutdown: {}", describe_call_error(&e, t)), &e),
    }

    if !ctx.s.is_alive() {
        return ctx.report("L6", Verdict::NotExercised, "the server exited before exit was sent (see L4)");
    }
    if let Err(e) = ctx.s.notify("exit", None).await {
        return ctx.report("L6", Verdict::Error, format!("could not send exit: {e}"));
    }
    match ctx.s.wait_exit(Instant::now() + ctx.cfg.exit_timeout).await {
        Some(status) if status.code() == Some(0) => ctx.pass("L6"),
        Some(status) => ctx.fail("L6", format!("exited with {}", session::describe_status(status))),
        None => ctx.fail("L6", format!("still running {} s after exit", ctx.cfg.exit_timeout.as_secs())),
    }
}

/// Concurrent requests, then notifications that must not be answered.
async fn json_rpc(ctx: &mut Ctx<'_>) {
    if let Err(InitFailure::Violation(why) | InitFailure::Absent(why) | InitFailure::Blocked(why)) =
        ctx.initialize().await
    {
        ctx.report("J6", Verdict::Error, format!("initialize didn't succeed: {why}"));
        ctx.j3_exercise = Some((Verdict::Error, format!("initialize didn't succeed: {why}")));
        return;
    }
    let t = ctx.cfg.request_timeout;
    check_j6(ctx, t).await;
    // J3 is judged across every scenario with well-formed input; this dedicated
    // exercise is what guarantees unknown and $/ notifications were sent.
    ctx.j3_exercise = Some(check_j3(ctx, t).await);

    // Leave politely so the server isn't killed mid-thought.
    if ctx.s.call("shutdown", None, t).await.is_ok() {
        let _ = ctx.s.notify("exit", None).await;
        let _ = ctx.s.wait_exit(Instant::now() + ctx.cfg.exit_timeout).await;
    }
}

/// Four requests written before any reply is read: each must be answered with
/// its exact id, in any order.
async fn check_j6(ctx: &mut Ctx<'_>, t: Duration) {
    let started = ctx.s.elapsed();
    // Two numeric ids and two string ids. The digit string catches a server
    // that turns numeric strings into numbers; the other catches the rest.
    let digit = format!("{:06}", session::random_u64() % 1_000_000);
    let text = format!("lspcheck-{}", session::random_u64() % 1_000_000_000);
    let ids = [ctx.s.fresh_id(), ctx.s.fresh_id(), Id::Str(digit), Id::Str(text)];
    let bodies: Vec<Vec<u8>> = ids
        .iter()
        .map(|id| {
            let body =
                serde_json::to_vec(&message::request(id, PROBE, Some(json!({})))).expect("JSON values serialize");
            transport::encode(&body)
        })
        .collect();
    // One deadline covers registering, writing and waiting for all four.
    let deadline = Instant::now() + t;
    let mut failure = None;
    let mut registered = 0;
    for (i, id) in ids.iter().enumerate() {
        if let Err(e) = ctx.s.begin_request(id, deadline).await {
            failure = Some(format!("lspcheck couldn't register a request: {e}"));
            registered = i;
            break;
        }
        registered = i + 1;
    }
    if failure.is_none() {
        for (i, body) in bodies.iter().enumerate() {
            // The shared deadline, not a fresh write timeout: a write that
            // can't finish by then is an error like any other, and the time
            // left to wait for the replies mustn't shrink with every write.
            if let Err(e) = ctx.s.write_split(body, &[], Duration::ZERO, deadline).await {
                // The server may have seen part of the failed write, so those
                // ids are aborted; the earlier, whole ones stay outstanding.
                ids[i..].iter().for_each(|id| ctx.s.abort_request(id));
                failure = Some(format!("lspcheck couldn't send a request: {e}"));
                break;
            }
        }
    } else {
        ids[..registered].iter().for_each(|id| ctx.s.abort_request(id));
    }
    if let Some(why) = failure {
        return ctx.report("J6", Verdict::Error, why);
    }

    let mut missing = Vec::new();
    let mut closed = false;
    for id in &ids {
        match ctx.s.wait_response(id, deadline).await {
            Ok(_) => {}
            Err(WaitError::Timeout) => missing.push(id.clone()),
            Err(WaitError::Closed) => {
                missing.push(id.clone());
                closed = true;
                break;
            }
        }
    }
    if missing.is_empty() {
        return ctx.pass("J6");
    }
    let list: Vec<String> = missing.iter().map(Id::to_string).collect();
    let mut detail = format!(
        "{} of {} concurrent requests were not answered with their id: {}",
        missing.len(),
        ids.len(),
        list.join(", ")
    );
    if closed {
        detail.push_str(" (the server closed its stdout)");
    }
    let unasked = ctx.s.unmatched_ids(started);
    if !unasked.is_empty() {
        let list: Vec<String> = unasked.iter().map(Id::to_string).collect();
        detail.push_str(&format!("; meanwhile responses with unmatched ids arrived: {}", list.join(", ")));
    }
    ctx.fail_absent("J6", detail);
}

/// Two unknown notifications, then two probes: nothing must answer the
/// notifications. The second probe's reply ends the window, so a late reply
/// still counts. This is the guaranteed exercise behind J3's cross-scenario
/// verdict, so it returns rather than reports.
async fn check_j3(ctx: &mut Ctx<'_>, t: Duration) -> (Verdict, String) {
    const UNKNOWN: [&str; 2] = ["lspcheck/unknownNotification", "$/lspcheck/unknownNotification"];
    let before = ctx.s.messages.len();
    let from = ctx.s.elapsed();
    for method in UNKNOWN {
        if let Err(e) = ctx.s.notify(method, Some(json!({}))).await {
            return (Verdict::Error, format!("lspcheck couldn't send {method}: {e}"));
        }
    }
    // A reply could still be coming after a probe fails, so keep what was
    // already read: a reply seen there is a failure either way.
    let mut probe_error = None;
    for _ in 0..2 {
        if let Err(e) = ctx.s.call(PROBE, Some(json!({})), t).await {
            probe_error = Some(e);
            break;
        }
    }
    // The second probe's answer, or where lspcheck gave up waiting for it,
    // ends the window.
    let to = ctx.s.elapsed();
    let replies: Vec<String> = ctx.s.messages[before..].iter().filter_map(|(_, m)| reply_to_notification(m)).collect();
    if !replies.is_empty() {
        let more = replies.len() - 1;
        let tail = if more > 0 { format!(" (and {more} more)") } else { String::new() };
        return (Verdict::Fail, format!("a notification was answered with {}{tail}", replies[0]));
    }
    if let Some(e) = probe_error {
        return (Verdict::Error, format!("couldn't tell: the server stopped answering ({e})"));
    }
    // A frame skipped or unreadable in the window could have been the reply.
    match ctx.s.incomplete_within(from, to) {
        Some(why) => {
            (Verdict::Error, format!("inconclusive, because {why}: it may have held a reply to a notification"))
        }
        None => (Verdict::Pass, String::new()),
    }
}

/// Whether a message is a response where a notification's reply would be, and
/// how to describe it.
fn reply_to_notification(m: &Message) -> Option<String> {
    match &m.kind {
        Kind::Response { id: Id::Null, outcome } => {
            Some(format!("a response with id null ({})", describe_outcome(outcome)))
        }
        Kind::Invalid(_)
            if (m.value.get("result").is_some() || m.value.get("error").is_some())
                && m.value.get("id").is_none_or(Value::is_null) =>
        {
            Some(format!("a response without an id: {}", session::preview(m.value.to_string().as_bytes())))
        }
        _ => None,
    }
}

/// A notification and a request before initialize.
async fn pre_initialize(ctx: &mut Ctx<'_>) {
    let t = ctx.cfg.request_timeout;
    let before = ctx.s.messages.len();
    if let Err(e) = ctx.s.notify("workspace/didChangeConfiguration", Some(json!({"settings": {}}))).await {
        ctx.report("L1", Verdict::Error, format!("could not send a notification: {e}"));
        return;
    }
    let request = ctx.s.call("workspace/symbol", Some(json!({"query": ""})), t).await;
    // When lspcheck stopped waiting for the answer: only what it missed
    // before then could explain a missing answer.
    let waited = ctx.s.elapsed();
    // A reply to the notification can come after the request's answer, so
    // look at everything read until the answer to initialize.
    let init = ctx.initialize().await;
    let reply = ctx.s.messages[before..].iter().find_map(|(_, x)| match &x.kind {
        Kind::Response { id, outcome } if !ctx.s.sent(id) => {
            Some(format!("{} with id {id}", describe_outcome(outcome)))
        }
        Kind::Invalid(_) if x.value.get("result").is_some() || x.value.get("error").is_some() => {
            Some(format!("a response without an id: {}", session::preview(x.value.to_string().as_bytes())))
        }
        _ => None,
    });
    let replied = reply.map(|r| format!("a notification sent before initialize got a reply: {r}"));
    match &request {
        Ok(m) => {
            let mut problems = Vec::new();
            match &m.kind {
                Kind::Response {
                    outcome: Outcome::Error { code: Some(codes::SERVER_NOT_INITIALIZED), .. }, ..
                } => {}
                Kind::Response { outcome, .. } => {
                    problems.push(format!("got {} instead of ServerNotInitialized", describe_outcome(outcome)))
                }
                _ => unreachable!(),
            }
            problems.extend(replied);
            if problems.is_empty() {
                ctx.pass("L1");
            } else {
                ctx.fail("L1", problems.join("; "));
            }
        }
        // The reply was seen, whatever happened to the request.
        Err(e) => match replied {
            Some(replied) => ctx.fail("L1", format!("{replied}; the request got {}", describe_call_error(e, t))),
            None => {
                ctx.fail_call("L1", describe_call_error(e, t), e);
                if let Some(r) = ctx.results.last_mut().filter(|r| r.absent_at.is_some()) {
                    r.absent_at = Some(waited);
                }
            }
        },
    }
    // Leave politely so the server isn't killed mid-thought.
    if init.is_ok() {
        let _ = ctx.s.call("shutdown", None, t).await;
        let _ = ctx.s.notify("exit", None).await;
        let _ = ctx.s.wait_exit(Instant::now() + ctx.cfg.exit_timeout).await;
    }
}

/// A second initialize, then a request after shutdown.
async fn after_initialize(ctx: &mut Ctx<'_>) {
    if let Err(InitFailure::Violation(why) | InitFailure::Absent(why) | InitFailure::Blocked(why)) =
        ctx.initialize().await
    {
        for id in ["L3", "L5"] {
            ctx.report(id, Verdict::Error, format!("initialize didn't succeed: {why}"));
        }
        return;
    }
    let t = ctx.cfg.request_timeout;
    // Whether the server answers a plain request, so that whether it still
    // does after the second initialize means something.
    let answers = ctx.s.call(PROBE, Some(json!({})), t).await.is_ok();

    // L3. Why the server can no longer be relied on, if it can't.
    let mut stopped: Option<&str> = None;
    match ctx.s.call("initialize", Some(initialize_params(&ctx.root)), t).await {
        Ok(m) => match &m.kind {
            Kind::Response { outcome: Outcome::Error { code: Some(_), .. }, .. } => {
                if !answers {
                    ctx.pass_with(
                        "L3",
                        "whether it keeps serving wasn't checked: it didn't answer a plain request before either",
                    );
                } else {
                    match ctx.s.call(PROBE, Some(json!({})), t).await {
                        Ok(_) => ctx.pass("L3"),
                        Err(e) => {
                            stopped = Some("the server stopped answering after the second initialize (see L3)");
                            let why = format!(
                                "after the error for the second initialize, a plain request went unanswered ({})",
                                describe_call_error(&e, t)
                            );
                            ctx.fail_call("L3", why, &e);
                        }
                    }
                }
            }
            Kind::Response { outcome, .. } => {
                ctx.fail("L3", format!("a second initialize got {} instead of an error", describe_outcome(outcome)))
            }
            _ => unreachable!(),
        },
        Err(e) => {
            stopped = Some("the second initialize went unanswered (see L3)");
            ctx.fail_call("L3", format!("a second initialize went unanswered ({})", describe_call_error(&e, t)), &e);
        }
    }

    // L5.
    if let Some(why) = stopped {
        return ctx.report("L5", Verdict::Error, format!("not judged: {why}"));
    }
    if let Err(e) = ctx.s.call("shutdown", None, t).await {
        return ctx.report("L5", Verdict::Error, format!("shutdown: {}", describe_call_error(&e, t)));
    }
    if !ctx.s.is_alive() {
        return ctx.report("L5", Verdict::NotExercised, "the server exited after shutdown, before exit (see L4)");
    }
    match ctx.s.call("workspace/symbol", Some(json!({"query": ""})), t).await {
        Ok(m) => match &m.kind {
            Kind::Response { outcome: Outcome::Error { code: Some(codes::INVALID_REQUEST), .. }, .. } => ctx.pass("L5"),
            Kind::Response { outcome, .. } => ctx.fail(
                "L5",
                format!("a request after shutdown got {} instead of InvalidRequest", describe_outcome(outcome)),
            ),
            _ => unreachable!(),
        },
        Err(e) => ctx.fail_call(
            "L5",
            format!("a request after shutdown went unanswered ({})", describe_call_error(&e, t)),
            &e,
        ),
    }
    if ctx.s.notify("exit", None).await.is_ok() {
        let _ = ctx.s.wait_exit(Instant::now() + ctx.cfg.exit_timeout).await;
    }
}

/// Whether the initialize result advertises `workspaceSymbolProvider`, either
/// as `true` or as an options object.
fn advertises_symbol(reply: &Message) -> bool {
    let Kind::Response { outcome: Outcome::Result(result), .. } = &reply.kind else { return false };
    matches!(result.pointer("/capabilities/workspaceSymbolProvider"), Some(Value::Bool(true) | Value::Object(_)))
}

/// The index in `messages` of the first response to `id` at or after `from`.
fn response_index(messages: &[(Duration, Message)], id: &Id, from: usize) -> Option<usize> {
    (from..messages.len()).find(|&i| messages[i].1.response_id() == Some(id))
}

fn describe_response(m: &Message) -> String {
    match &m.kind {
        Kind::Response { outcome, .. } => describe_outcome(outcome),
        _ => unreachable!("wait_response returns a response"),
    }
}

/// How a cancelled request was answered, for C1's detail.
fn describe_cancelled_answer(m: &Message) -> String {
    match &m.kind {
        Kind::Response { outcome: Outcome::Result(_), .. } => "answered with a result".into(),
        Kind::Response { outcome: Outcome::Error { code: Some(codes::REQUEST_CANCELLED), .. }, .. } => {
            "answered with RequestCancelled (-32800)".into()
        }
        _ => format!("answered with {}", describe_response(m)),
    }
}

/// Cancellation: one response to a cancelled request, none to a cancel of an
/// unknown or finished one, and no response to `$/cancelRequest` itself.
///
/// What a server says about a cancel can come at any time, so the verdicts
/// are given at the end, over everything read before lspcheck sent `exit`.
/// An inspection limit or an unreadable frame before then turns what would be
/// a pass into an error, because the frame lspcheck missed may have been the
/// reply these checks look for.
async fn cancellation(ctx: &mut Ctx<'_>) {
    const CHECKS: [&str; 3] = ["C1", "C2", "C3"];
    let reply = match ctx.initialize().await {
        Ok(reply) => reply,
        Err(InitFailure::Violation(why) | InitFailure::Absent(why) | InitFailure::Blocked(why)) => {
            for id in CHECKS {
                ctx.report(id, Verdict::Error, format!("initialize didn't succeed: {why}"));
            }
            return;
        }
    };
    let t = ctx.cfg.request_timeout;

    // A plain request first, so that a reply to `initialized` is most likely
    // read before the cancels (C3 only counts replies read after the first
    // cancel was written).
    if ctx.s.call(PROBE, Some(json!({})), t).await.is_err() {
        for id in CHECKS {
            ctx.report(id, Verdict::Error, "not judged: the plain request sent after `initialized` went unanswered");
        }
        leave(ctx).await;
        return;
    };

    // C1. The request and its cancellation go out before anything is read, so
    // the server has both when it decides the request's fate. `workspace/symbol`
    // is used when it is advertised, because the stand-in can hold that one.
    let (method, params, which) = if advertises_symbol(&reply) {
        ("workspace/symbol", json!({"query": ""}), "workspace/symbol (the server advertises workspaceSymbolProvider)")
    } else {
        (PROBE, json!({}), "lspcheck/probe (the server doesn't advertise workspaceSymbolProvider)")
    };
    let r = ctx.s.fresh_id();
    let request = serde_json::to_vec(&message::request(&r, method, Some(params))).expect("JSON values serialize");
    let mut bytes = transport::encode(&request);
    let split = bytes.len();
    bytes.extend(frame(&message::notification("$/cancelRequest", Some(json!({"id": r.to_value()})))));
    let deadline = Instant::now() + t;
    let sent = match ctx.s.begin_request(&r, deadline).await {
        Err(e) => Err(format!("couldn't prepare the request: {e}")),
        Ok(()) => {
            // Anything read before the request was written can't answer it.
            let from = (ctx.s.messages.len(), ctx.s.elapsed());
            match ctx.s.write_split(&bytes, &[split], Duration::ZERO, deadline).await {
                Ok(()) => Ok(from),
                Err(e) => {
                    ctx.s.abort_request(&r);
                    Err(format!("couldn't send the request and its cancellation: {e}"))
                }
            }
        }
    };
    let (after_send, written_at) = match sent {
        Ok(from) => from,
        Err(why) => {
            ctx.report("C1", Verdict::Error, format!("{which}: lspcheck {why}"));
            for id in ["C2", "C3"] {
                ctx.report(id, Verdict::Error, "not judged: the C1 request couldn't be sent");
            }
            leave(ctx).await;
            return;
        }
    };
    let answer = ctx.s.wait_response(&r, deadline).await;
    // When lspcheck stopped waiting for R: only a frame it missed between
    // writing R and then could have been R's answer.
    let r_waited = ctx.s.elapsed();
    let first = answer.as_ref().ok().and_then(|_| response_index(&ctx.s.messages, &r, after_send));

    // Whether the server still answers, without which nothing it didn't
    // send can be judged.
    let mut stopped: Option<String> = None;
    let mut c1_probe: Option<CallError> = None;
    if let Err(e) = ctx.s.call(PROBE, Some(json!({})), t).await {
        stopped = Some("the server stopped answering (see C1)".into());
        c1_probe = Some(e);
    }

    // C2. An id never sent, then R if it was answered (a pending R may
    // lawfully be cancelled). Each cancel is followed by its own plain
    // request, so no read holds three of lspcheck's frames: a server that
    // mishandles coalesced frames (F4) can't fail this check for that.
    let unknown = ctx.s.fresh_id();
    let c2_from = ctx.s.messages.len();
    // When the cancel for R was sent: a second response to R read before it
    // is C1's finding, one read after it may answer this cancel.
    let mut r_cancelled_at: Option<Duration> = None;
    // A plain request in C2 that went unanswered: the error, and when it was
    // sent and when lspcheck stopped waiting.
    let mut c2_problem: Option<(CallError, Duration, Duration)> = None;
    let mut c2_unsent: Option<WriteError> = None;
    // When C2's first cancel was sent: only what came after can be about it.
    let mut c2_started: Option<Duration> = None;
    if stopped.is_none() {
        let targets: &[&Id] = if first.is_some() { &[&unknown, &r] } else { &[&unknown] };
        for cancelled in targets {
            let at = ctx.s.elapsed();
            c2_started.get_or_insert(at);
            let note = message::notification("$/cancelRequest", Some(json!({"id": cancelled.to_value()})));
            if let Err(e) = ctx.s.send(&note).await {
                c2_unsent = Some(e);
                break;
            }
            if *cancelled == &r {
                r_cancelled_at = Some(at);
            }
            let asked = ctx.s.elapsed();
            if let Err(e) = ctx.s.call(PROBE, Some(json!({})), t).await {
                c2_problem = Some((e, asked, ctx.s.elapsed()));
                stopped = Some("the server stopped answering (see C2)".into());
                break;
            }
        }
    }

    let end = leave(ctx).await;
    // What lspcheck missed counts against a check only when it was missed
    // where that check's evidence would be: from R to C2's cancel of R for
    // C1, from C2's first cancel for C2, and from R for C3.
    let incomplete_c1 = ctx.s.incomplete_within(written_at, r_cancelled_at.unwrap_or(end));
    let incomplete_c2 = c2_started.and_then(|from| ctx.s.incomplete_within(from, end));
    let incomplete_c3 = ctx.s.incomplete_within(written_at, end);
    let inconclusive = |why: &str, what: &str| format!("inconclusive, because {why}: it may have held {what}");
    let after_r_cancel = |at: Duration| r_cancelled_at.is_some_and(|c| at >= c);
    // Indices of the messages read before `end`, from `from` on.
    let msgs = &ctx.s.messages;
    let window = |from: usize| (from..msgs.len()).filter(|&i| msgs[i].0 < end);

    // C1.
    let c1 = match (&answer, first) {
        (Err(wait), _) => {
            let e = CallError::Wait(*wait);
            let why = format!("{which}: no response after $/cancelRequest ({})", describe_call_error(&e, t));
            match ctx.s.incomplete_within(written_at, r_waited) {
                Some(missed) => (Verdict::Error, format!("inconclusive, because {missed}: {why}")),
                None => (Verdict::Fail, why),
            }
        }
        (Ok(m), Some(first)) => {
            let how = describe_cancelled_answer(m);
            let again = window(first + 1).find(|&i| msgs[i].1.response_id() == Some(&r) && !after_r_cancel(msgs[i].0));
            if let Some(i) = again {
                let then = describe_response(&msgs[i].1);
                (Verdict::Fail, format!("{which} was answered twice: {how}, then {then}"))
            } else if let Some(e) = &c1_probe {
                let why = format!(
                    "inconclusive: {which}: {how}, but the plain request that followed {}, so a second reply could still come",
                    describe_call_error(e, t)
                );
                (Verdict::Error, why)
            } else if let Some(why) = &incomplete_c1 {
                (Verdict::Error, inconclusive(why, &format!("a second response to {which}")))
            } else {
                (Verdict::Pass, format!("{which}: {how}, and only once"))
            }
        }
        (Ok(_), None) => unreachable!("an answered request is recorded in messages"),
    };
    // C2: a response carrying a cancelled id.
    let offending = window(c2_from).find_map(|i| {
        let (at, m) = &msgs[i];
        let id = m.response_id()?;
        (*id == unknown || (*id == r && after_r_cancel(*at))).then(|| (id.clone(), describe_response(m)))
    });
    // C3. Until `exit` the only notifications lspcheck sends are
    // `initialized` and the cancels, and JSON-RPC gives a response without an
    // id nothing that says which one it answers, so only one that names
    // `$/cancelRequest` (as a MethodNotFound message usually does) is known
    // to answer a cancel. Any of them answers some notification, which J3
    // judges.
    let after: Vec<(String, bool)> = window(after_send)
        .filter(|&i| msgs[i].0 >= written_at)
        .filter_map(|i| {
            let (at, m) = &msgs[i];
            idless_reply(at, m).map(|what| (what, m.value.to_string().contains("cancelRequest")))
        })
        .collect();
    ctx.report("C1", c1.0, c1.1);

    if let Some((id, what)) = offending {
        let whose = if id == r { "the finished request" } else { "an id never sent" };
        ctx.fail("C2", format!("after a $/cancelRequest for {whose} ({id}), the server sent {what} with that id"));
    } else if c1_probe.is_some() {
        ctx.report("C2", Verdict::Error, "not judged: the server stopped answering (see C1)");
    } else if let Some(e) = c2_unsent {
        ctx.report("C2", Verdict::Error, format!("lspcheck couldn't send a $/cancelRequest: {e}"));
    } else if let Some((e, asked, waited)) = c2_problem {
        let why = format!(
            "the server stopped answering after cancelling an unknown or finished request ({})",
            describe_call_error(&e, t)
        );
        // Only a frame lspcheck missed while waiting could have been the answer.
        match (e, ctx.s.incomplete_within(asked, waited)) {
            (CallError::Write(_), _) => ctx.report("C2", Verdict::Error, why),
            (CallError::Wait(_), Some(missed)) => {
                ctx.report("C2", Verdict::Error, format!("inconclusive, because {missed}: {why}"))
            }
            (CallError::Wait(_), None) => ctx.fail("C2", why),
        }
    } else if let Some(why) = &incomplete_c2 {
        ctx.report("C2", Verdict::Error, inconclusive(why, "a reply to one of the cancels"));
    } else {
        let what = if first.is_some() { "an id never sent and a finished request" } else { "an id never sent" };
        ctx.pass_with("C2", format!("cancelling {what} drew no reply, and the server kept serving"));
    }

    // C3.
    if let Some((what, _)) = after.iter().find(|(_, named)| *named) {
        ctx.fail("C3", format!("{what}, answering a $/cancelRequest notification"));
    } else if let Some((what, _)) = after.first() {
        let more = if after.len() > 1 { format!(" (and {} more)", after.len() - 1) } else { String::new() };
        ctx.report(
            "C3",
            Verdict::Error,
            format!(
                "{what}{more} after the first cancel; nothing in it says whether it answers `initialized` or a cancel \
                 (either way it answers a notification, see J3)"
            ),
        );
    } else if let Some(why) = &stopped {
        ctx.report("C3", Verdict::Error, format!("not judged: {why}, so a reply could still come"));
    } else if let Some(why) = &incomplete_c3 {
        ctx.report("C3", Verdict::Error, inconclusive(why, "a reply to a $/cancelRequest"));
    } else {
        ctx.pass_with("C3", "no response answered a $/cancelRequest notification");
    }
}

/// How a response without an id (no `id` member, or `id: null`) looks, if
/// `m` is one. A malformed message that carries an id is J5's business.
fn idless_reply(at: &Duration, m: &Message) -> Option<String> {
    match &m.kind {
        Kind::Response { id: Id::Null, outcome } => {
            Some(format!("at {} ms, the server sent {} with id null", at.as_millis(), describe_outcome(outcome)))
        }
        Kind::Invalid(_)
            if (m.value.get("result").is_some() || m.value.get("error").is_some())
                && m.value.get("id").is_none_or(Value::is_null) =>
        {
            let text = session::preview(m.value.to_string().as_bytes());
            Some(format!("at {} ms, the server sent a response without an id: {text}", at.as_millis()))
        }
        _ => None,
    }
}

/// Shuts the server down as a client would and returns when lspcheck stopped
/// waiting for replies to what came before: when it sent `exit`, or gave up
/// on `shutdown`. Everything read by then has been taken in.
async fn leave(ctx: &mut Ctx<'_>) -> Duration {
    let shut = ctx.s.call("shutdown", None, ctx.cfg.request_timeout).await.is_ok();
    let end = ctx.s.elapsed();
    if shut && ctx.s.notify("exit", None).await.is_ok() {
        let _ = ctx.s.wait_exit(Instant::now() + ctx.cfg.exit_timeout).await;
    }
    ctx.s.take_in();
    end
}

/// The method whose notifications carry diagnostics.
const PUBLISH_DIAGNOSTICS: &str = "textDocument/publishDiagnostics";
/// How long lspcheck waits for diagnostics labelled with the version it sent.
/// Absence fails nothing, so the wait only buys evidence.
const DIAGNOSTICS_WAIT: Duration = Duration::from_secs(3);
/// The version document A is opened with. A distinctive base catches a server
/// that labels diagnostics with a counter of its own.
const DOC_A_VERSION: i64 = 41;

/// A request lspcheck sent about a document.
struct Asked {
    id: Id,
    method: &'static str,
    params: Value,
    /// When lspcheck began writing it.
    at: Duration,
}

/// Initialize params for the documents scenario: the usual ones, plus the
/// client capabilities for the requests it may send.
fn documents_initialize_params(root: &Path) -> Value {
    let mut params = initialize_params(root);
    let doc = &mut params["capabilities"]["textDocument"];
    for (name, caps) in [
        ("foldingRange", json!({"lineFoldingOnly": false})),
        ("selectionRange", json!({})),
        ("documentHighlight", json!({})),
        ("documentLink", json!({})),
        ("codeLens", json!({})),
        ("diagnostic", json!({"relatedDocumentSupport": false})),
        ("inlayHint", json!({})),
        ("formatting", json!({})),
    ] {
        doc[name] = caps;
    }
    params
}

/// The encoding positions are counted in: the server's `positionEncoding`, or
/// UTF-16 when it chose none. One lspcheck didn't offer is L2's failure, and
/// leaves nothing D1 could count in.
fn negotiated_encoding(reply: &Message) -> Result<Encoding, String> {
    let Kind::Response { outcome: Outcome::Result(result), .. } = &reply.kind else { unreachable!() };
    match result.pointer("/capabilities/positionEncoding") {
        None => Ok(Encoding::Utf16),
        Some(Value::String(name)) if OFFERED_ENCODINGS.contains(&name.as_str()) => {
            Ok(Encoding::from_name(name).expect("every offered encoding has a name lspcheck knows"))
        }
        Some(other) => {
            Err(format!("not judged: the server chose positionEncoding {other}, which lspcheck didn't offer (see L2)"))
        }
    }
}

/// Whether a server capability is advertised: `true`, or an options object.
fn advertises(caps: &Value, name: &str) -> bool {
    matches!(caps.get(name), Some(Value::Bool(true) | Value::Object(_)))
}

/// The advertised requests about a document at `uri`, with their params.
/// Every position sent is counted in the negotiated encoding, of `text`.
fn document_requests(caps: &Value, uri: &str, text: &str, encoding: Encoding) -> Vec<(&'static str, Value)> {
    let pos = |p: Position| json!({"line": p.line, "character": p.character});
    let doc = json!({"uri": uri});
    let lines = text::split_lines(text);
    let (word, _) = documents::span_in(text, 0, "word", encoding);
    // `word` is ASCII, so two units in is inside it in every encoding.
    let inside = Position { line: 0, character: word.character + 2 };
    let l0_end = Position { line: 0, character: text::line_length(lines[0], encoding) };
    // `name` is on line 2 of version 41 and line 3 of version 42.
    let name_line = lines.iter().position(|l| l.contains("name")).expect("the document has a name");
    let (_, name_end) = documents::span_in(text, name_line, "name", encoding);
    // The empty line after the final newline ends the document.
    let whole = json!({"start": {"line": 0, "character": 0}, "end": {"line": lines.len() - 1, "character": 0}});
    [
        ("documentSymbolProvider", "textDocument/documentSymbol", json!({"textDocument": doc})),
        ("foldingRangeProvider", "textDocument/foldingRange", json!({"textDocument": doc})),
        ("documentLinkProvider", "textDocument/documentLink", json!({"textDocument": doc})),
        ("codeLensProvider", "textDocument/codeLens", json!({"textDocument": doc})),
        ("inlayHintProvider", "textDocument/inlayHint", json!({"textDocument": doc, "range": whole})),
        (
            "documentFormattingProvider",
            "textDocument/formatting",
            json!({"textDocument": doc, "options": {"tabSize": 4, "insertSpaces": true}}),
        ),
        ("diagnosticProvider", "textDocument/diagnostic", json!({"textDocument": doc})),
        ("hoverProvider", "textDocument/hover", json!({"textDocument": doc, "position": pos(word)})),
        (
            "documentHighlightProvider",
            "textDocument/documentHighlight",
            json!({"textDocument": doc, "position": pos(word)}),
        ),
        (
            "selectionRangeProvider",
            "textDocument/selectionRange",
            json!({"textDocument": doc, "positions": [pos(word), pos(inside), pos(l0_end), pos(name_end)]}),
        ),
    ]
    .into_iter()
    .filter(|(cap, ..)| advertises(caps, cap))
    .map(|(_, method, params)| (method, params))
    .collect()
}

/// Sends one request and waits for its response, all by one `--timeout`
/// deadline. Errs when lspcheck couldn't send it.
async fn ask(
    ctx: &mut Ctx<'_>,
    method: &'static str,
    params: Value,
) -> Result<(Asked, Result<(), WaitError>), WriteError> {
    let deadline = Instant::now() + ctx.cfg.request_timeout;
    let id = ctx.s.fresh_id();
    let bytes = frame(&message::request(&id, method, Some(params.clone())));
    ctx.s.begin_request(&id, deadline).await?;
    let at = ctx.s.elapsed();
    if let Err(e) = ctx.s.write_split(&bytes, &[], Duration::ZERO, deadline).await {
        ctx.s.abort_request(&id);
        return Err(e);
    }
    let answer = ctx.s.wait_response(&id, deadline).await.map(|_| ());
    Ok((Asked { id, method, params, at }, answer))
}

/// Whether a message is a `publishDiagnostics` for `uri` labelled `version`.
fn labelled_diagnostics(m: &Message, uri: &str, version: i64) -> bool {
    m.method() == Some(PUBLISH_DIAGNOSTICS)
        && m.value.pointer("/params/uri").and_then(Value::as_str).is_some_and(|u| documents::same_uri(u, uri))
        && m.value.pointer("/params/version").and_then(documents::version_number) == Some(version)
}

/// Reads until diagnostics for `uri` labelled `version` have been read (from
/// message `from` on), or for [`DIAGNOSTICS_WAIT`], answering the server's
/// requests meanwhile.
async fn await_diagnostics(ctx: &mut Ctx<'_>, uri: &str, version: i64, from: usize) {
    let deadline = Instant::now() + DIAGNOSTICS_WAIT;
    let mut checked = from;
    loop {
        if ctx.s.messages[checked..].iter().any(|(_, m)| labelled_diagnostics(m, uri, version)) {
            return;
        }
        checked = ctx.s.messages.len();
        if matches!(ctx.s.recv(deadline).await, Recv::Closed | Recv::Timeout) {
            return;
        }
    }
}

/// Sends the advertised requests one at a time, so a reply always refers to
/// the version current when it was asked. No reply, or an error, is simply
/// no evidence: the rest are not sent, and the note says so.
async fn ask_all(
    ctx: &mut Ctx<'_>,
    requests: Vec<(&'static str, Value)>,
    asked: &mut Vec<Asked>,
    notes: &mut Vec<String>,
) {
    for (method, params) in requests {
        match ask(ctx, method, params).await {
            Ok((a, answer)) => {
                asked.push(a);
                if let Err(e) = answer {
                    let why = describe_call_error(&CallError::Wait(e), ctx.cfg.request_timeout);
                    notes.push(format!("{method} went unanswered ({why}), so no more requests were sent"));
                    break;
                }
            }
            Err(e) => {
                notes.push(format!("lspcheck couldn't send {method} ({e}), so no more requests were sent"));
                break;
            }
        }
    }
}

/// Whether a message is a `client/registerCapability` registering the sync
/// notifications D3 depends on.
fn sync_registration(m: &Message) -> bool {
    m.method() == Some("client/registerCapability")
        && (m.value.pointer("/params/registerOptions/textDocumentDidChange").is_some()
            || m.value.pointer("/params/registerOptions/textDocumentDidOpen").is_some())
}

/// The version document A is changed to.
const DOC_A2_VERSION: i64 = 42;

/// Documents: lspcheck opens document A, asks the server's advertised
/// requests about it one at a time, and waits a while for diagnostics. Every
/// position the server sends about it must fall on a character boundary of
/// the text it refers to, in the negotiated encoding (D1), every `version`
/// its diagnostics carry for the document must be one lspcheck sent for it
/// before the notification was read (D2), and after the `didChange` lspcheck
/// sends, positions must refer to the changed text (D3).
///
/// A position or a version can arrive at any time (diagnostics especially),
/// so all three verdicts are given at the end, over everything read before
/// lspcheck sent `exit`.
async fn documents(ctx: &mut Ctx<'_>) {
    let params = documents_initialize_params(&ctx.root);
    let reply = match ctx.initialize_with(params).await {
        Ok(reply) => reply,
        Err(InitFailure::Violation(why) | InitFailure::Absent(why) | InitFailure::Blocked(why)) => {
            let detail = format!("initialize didn't succeed: {why}");
            for id in ["D1", "D2", "D3"] {
                ctx.report(id, Verdict::Error, detail.clone());
            }
            return;
        }
    };
    if !opens_documents(&reply) {
        let detail = "the server doesn't ask for didOpen (textDocumentSync), so lspcheck opens no document";
        for id in ["D1", "D2", "D3"] {
            ctx.report(id, Verdict::Skip, detail);
        }
        leave(ctx).await;
        return;
    }
    let encoding = match negotiated_encoding(&reply) {
        Ok(e) => e,
        Err(why) => {
            for id in ["D1", "D2", "D3"] {
                ctx.report(id, Verdict::Error, why.clone());
            }
            leave(ctx).await;
            return;
        }
    };
    let Kind::Response { outcome: Outcome::Result(result), .. } = &reply.kind else { unreachable!() };
    let caps = result.get("capabilities").cloned().unwrap_or(Value::Null);
    let sync = documents::sync_kind(result);

    // Document A, saved in the workspace as an editor would open it.
    let path = ctx.root.join("doc.txt");
    let text = documents::doc_a();
    if let Err(e) = std::fs::write(&path, &text) {
        ctx.s.note(format!("couldn't write {}: {e}", path.display()));
    }
    ctx.s.record_workspace(ctx.root.clone(), workspace_files(&ctx.root));
    let uri = file_uri(&path);
    let from = ctx.s.messages.len();
    let opened_at = ctx.s.elapsed();
    let doc = json!({"uri": uri, "languageId": "plaintext", "version": DOC_A_VERSION, "text": text});
    if let Err(e) = ctx.s.notify("textDocument/didOpen", Some(json!({"textDocument": doc}))).await {
        let detail = format!("lspcheck couldn't send didOpen: {e}");
        for id in ["D1", "D2", "D3"] {
            ctx.report(id, Verdict::Error, detail.clone());
        }
        leave(ctx).await;
        return;
    }
    let mut docs = [OpenDoc {
        uri: uri.clone(),
        versions: vec![Version { at: opened_at, number: DOC_A_VERSION, text: text.clone() }],
    }];

    let mut asked = Vec::new();
    let mut notes = Vec::new();
    ask_all(ctx, document_requests(&caps, &uri, &text, encoding), &mut asked, &mut notes).await;
    await_diagnostics(ctx, &uri, DOC_A_VERSION, from).await;

    // The change to version 42, the advertised way. A sync kind of None
    // sends no `didChange`, and D3 skips.
    let mut change: Option<Change> = None;
    let mut registered = false;
    if sync != SyncKind::None {
        // A dynamic registration of the sync notifications, read before the
        // change was written: lspcheck declares none but answers it with
        // success, so the server may expect another sync kind. D3 is an
        // error, and the change is still sent, so D1 and D2 keep their
        // evidence.
        registered = ctx.s.messages.iter().any(|(_, m)| sync_registration(m));
        let (changes, after) = documents::scenario_change(&text, encoding, sync);
        let change_at = ctx.s.elapsed();
        let content: Vec<Value> = changes
            .iter()
            .map(|c| match c {
                ContentChange::Full(t) => json!({"text": t}),
                ContentChange::Text { range, text } => json!({
                    "range": {
                        "start": {"line": range.start.line, "character": range.start.character},
                        "end": {"line": range.end.line, "character": range.end.character},
                    },
                    "text": text,
                }),
            })
            .collect();
        let params = json!({
            "textDocument": {"uri": uri, "version": DOC_A2_VERSION},
            "contentChanges": content,
        });
        if let Err(e) = ctx.s.notify("textDocument/didChange", Some(params)).await {
            let detail = format!("lspcheck couldn't send didChange: {e}");
            for id in ["D1", "D2", "D3"] {
                ctx.report(id, Verdict::Error, detail.clone());
            }
            leave(ctx).await;
            return;
        }
        change = Some(Change {
            at: change_at,
            before: text.clone(),
            after: after.clone(),
            alternatives: documents::alternatives(&text, encoding, sync),
        });
        docs[0].versions.push(Version { at: change_at, number: DOC_A2_VERSION, text: after });
        // The same requests at version 42, with positions of the changed
        // text, one at a time as before.
        ask_all(ctx, document_requests(&caps, &uri, &docs[0].versions[1].text, encoding), &mut asked, &mut notes).await;
        await_diagnostics(ctx, &uri, DOC_A2_VERSION, from).await;
    }
    let end = leave(ctx).await;

    // Everything read before `exit` was sent: diagnostics, and the replies
    // to lspcheck's requests, however late.
    let msgs = &ctx.s.messages;
    let mut sources = Vec::new();
    let mut published = Vec::new();
    for (at, m) in msgs.iter().filter(|(at, _)| *at < end) {
        if m.method() == Some(PUBLISH_DIAGNOSTICS) && matches!(m.kind, Kind::Notification { .. }) {
            let params = m.value.get("params").cloned().unwrap_or(Value::Null);
            if let Some(uri) = params.get("uri").and_then(Value::as_str) {
                // The raw value: `null` or a non-integer version isn't D2's
                // to judge, but it isn't an integer version either.
                published.push(Published { uri: uri.into(), version: params.get("version").cloned(), read_at: *at });
            }
            sources.push(Source {
                method: PUBLISH_DIAGNOSTICS.into(),
                found: text::positions(PUBLISH_DIAGNOSTICS, &params, None),
                asked_at: None,
                read_at: *at,
                label: params.get("version").and_then(documents::version_number),
            });
        }
    }
    // Every response to the request, not only the first: a duplicate is J2's
    // finding, but positions in it are still about the document.
    for a in &asked {
        let answers = msgs.iter().filter(|(at, m)| *at >= a.at && *at < end && m.response_id() == Some(&a.id));
        for (at, m) in answers {
            let Kind::Response { outcome: Outcome::Result(result), .. } = &m.kind else { continue };
            sources.push(Source {
                method: a.method.into(),
                found: text::positions(a.method, &a.params, Some(result)),
                asked_at: Some(a.at),
                read_at: *at,
                label: None,
            });
        }
    }
    let judged = documents::judge_positions(encoding, &docs, &sources);
    let incomplete = ctx.s.incomplete_within(opened_at, end);
    let (verdict, mut detail) = documents::judge_d1(encoding, &judged, incomplete.as_deref());
    if !notes.is_empty() {
        detail = format!("{detail}; {}", notes.join("; "));
    }
    ctx.report("D1", verdict, detail);
    let (verdict, mut detail) = documents::judge_d2(&docs, &published, incomplete.as_deref());
    if !notes.is_empty() {
        detail = format!("{detail}; {}", notes.join("; "));
    }
    ctx.report("D2", verdict, detail);
    match change {
        // The sync kind is None or absent: the client must not send
        // `didChange`, so there is no change to judge.
        None => ctx.report(
            "D3",
            Verdict::Skip,
            "the server doesn't take changes (textDocumentSync's change is 0 or absent)",
        ),
        // A dynamic registration of the sync notifications, read before the
        // change was written: the server may expect another sync kind.
        Some(_) if registered => ctx.report(
            "D3",
            Verdict::Error,
            "the server registered textDocument/didChange or didOpen dynamically; lspcheck declares no dynamic registration, so the server may expect another sync kind",
        ),
        Some(change) => {
            // D1's judgements of the positions read before the change was
            // written, and of those that can only refer to the changed text:
            // a reply to a request sent after the change, or diagnostics
            // labelled with its version. A late reply to an earlier request,
            // or unlabelled diagnostics read after the change, may refer to
            // the text before it; a position invalid there is D1's finding,
            // not evidence about applying the change.
            let before = documents::judge_positions_where(encoding, &docs, &sources, |s| s.read_at < change.at);
            let after = documents::judge_positions_where(encoding, &docs, &sources, |s| {
                documents::only_after(&docs[0], s, change.at)
            });
            let incomplete_before = ctx.s.incomplete_within(opened_at, change.at);
            let incomplete_change = ctx.s.incomplete_within(change.at, end);
            let (verdict, mut detail) = documents::judge_d3(
                encoding,
                &before,
                &after,
                &change,
                incomplete_before.as_deref(),
                incomplete_change.as_deref(),
            );
            if !notes.is_empty() {
                detail = format!("{detail}; {}", notes.join("; "));
            }
            ctx.report("D3", verdict, detail);
        }
    }
}

/// End of input without exit, as when the editor goes away.
async fn end_of_input(ctx: &mut Ctx<'_>) {
    if let Err(InitFailure::Violation(why) | InitFailure::Absent(why) | InitFailure::Blocked(why)) =
        ctx.initialize().await
    {
        return ctx.report("L8", Verdict::Error, format!("initialize didn't succeed: {why}"));
    }
    if !ctx.s.is_alive() {
        return ctx.report("L8", Verdict::NotExercised, "the server exited before its input ended");
    }
    ctx.s.close_stdin();
    let wait = ctx.cfg.exit_timeout;
    match ctx.s.wait_exit(Instant::now() + wait).await {
        Some(_) => ctx.pass("L8"),
        None => ctx.fail("L8", format!("still running {} s after its stdin closed", wait.as_secs())),
    }
}

/// exit without shutdown.
async fn exit_without_shutdown(ctx: &mut Ctx<'_>) {
    match ctx.initialize().await {
        Ok(_) => {}
        Err(InitFailure::Violation(why) | InitFailure::Absent(why) | InitFailure::Blocked(why)) => {
            return ctx.report("L7", Verdict::Error, format!("initialize didn't succeed: {why}"));
        }
    }
    if let Err(e) = ctx.s.notify("exit", None).await {
        return ctx.report("L7", Verdict::Error, format!("could not send exit: {e}"));
    }
    match ctx.s.wait_exit(Instant::now() + ctx.cfg.exit_timeout).await {
        Some(status) if status.code() == Some(1) => ctx.pass("L7"),
        Some(status) => ctx.fail("L7", format!("exited with {}", session::describe_status(status))),
        None => ctx.fail("L7", format!("still running {} s after exit", ctx.cfg.exit_timeout.as_secs())),
    }
}

/// A request every server answers: an unknown method gets MethodNotFound
/// (J4). The framing checks ask only whether a response arrived.
const PROBE: &str = "lspcheck/probe";
/// The pause between the pieces of a split write, so that a server reading
/// promptly sees each piece on its own.
const SPLIT_PAUSE: Duration = Duration::from_millis(10);
/// Text with 2-, 3- and 4-byte UTF-8 characters and a combining mark.
const MULTIBYTE: &str = "é ñ ü · € 中文 · 😀 𝄞 · e\u{301}";

/// How one framing variant went.
enum Probe {
    /// Every request it carried was answered.
    Answered,
    /// A response that should have come didn't.
    Missing(String),
    /// lspcheck couldn't deliver it.
    Unsent(String),
}

fn frame(value: &Value) -> Vec<u8> {
    transport::encode(&serde_json::to_vec(value).expect("JSON values serialize"))
}

/// Writes `bytes`, which hold the requests `ids`, as separate writes cut at
/// `cuts`, and waits for every response, all by one deadline.
async fn exchange(ctx: &mut Ctx<'_>, ids: &[Id], bytes: &[u8], cuts: &[usize]) -> Probe {
    let t = ctx.cfg.request_timeout;
    let deadline = Instant::now() + t;
    let started = ctx.s.messages.len();
    for (i, id) in ids.iter().enumerate() {
        if let Err(e) = ctx.s.begin_request(id, deadline).await {
            ids[..i].iter().for_each(|id| ctx.s.abort_request(id));
            return Probe::Unsent(e.to_string());
        }
    }
    if let Err(e) = ctx.s.write_split(bytes, cuts, SPLIT_PAUSE, deadline).await {
        ids.iter().for_each(|id| ctx.s.abort_request(id));
        return Probe::Unsent(e.to_string());
    }
    let (mut missing, mut why) = (0, None);
    for id in ids {
        if let Err(e) = ctx.s.wait_response(id, deadline).await {
            missing += 1;
            why.get_or_insert(e);
        }
    }
    let Some(e) = why else { return Probe::Answered };
    let what = match e {
        WaitError::Timeout => format!("no response within {} s", t.as_secs()),
        WaitError::Closed => "the server closed its stdout without responding".into(),
    };
    let mut detail =
        if ids.len() > 1 { format!("{missing} of {} requests went unanswered: {what}", ids.len()) } else { what };
    // A server that couldn't read a frame often says so with an error whose id is null.
    if let Some(Kind::Response { outcome, .. }) = ctx.s.messages[started..]
        .iter()
        .map(|(_, m)| &m.kind)
        .find(|k| matches!(k, Kind::Response { id: Id::Null, .. }))
    {
        detail = format!("{detail}; the server sent {} with id null", describe_outcome(outcome));
    }
    Probe::Missing(detail)
}

/// Whether the server still answers a plain request after a variant that
/// didn't pass; if not, why the rest of the scenario can't be judged.
async fn back_in_step(ctx: &mut Ctx<'_>, after: &str) -> Option<String> {
    if !ctx.s.is_alive() {
        return Some(format!("the server exited after {after}"));
    }
    let t = ctx.cfg.request_timeout;
    let result = ctx.s.call(PROBE, Some(json!({})), t).await;
    // A closed pipe usually means the server is exiting; give that a moment
    // to be seen, so the reason given is the real one.
    if matches!(result, Err(CallError::Write(WriteError::Closed) | CallError::Wait(WaitError::Closed)))
        && ctx.s.wait_exit(Instant::now() + ctx.cfg.exit_timeout).await.is_some()
    {
        return Some(format!("the server exited after {after}"));
    }
    match result {
        Ok(_) => None,
        Err(e @ CallError::Wait(_)) => Some(format!(
            "after {after}, a plain request went unanswered ({}), so the input may be out of step",
            describe_call_error(&e, t)
        )),
        Err(e) => Some(format!("after {after}, lspcheck couldn't send a plain request ({e})")),
    }
}

/// Reports a framing variant. Returns why the stream can no longer be
/// trusted, if it can't.
async fn settle(ctx: &mut Ctx<'_>, id: &str, what: &str, outcome: Probe) -> Option<String> {
    match outcome {
        Probe::Answered => {
            ctx.pass(id);
            return None;
        }
        Probe::Missing(why) => ctx.fail_absent(id, format!("{what}: {why}")),
        Probe::Unsent(why) => ctx.report(id, Verdict::Error, format!("{what}: {why}")),
    }
    back_in_step(ctx, &format!("{id} didn't pass")).await
}

/// Variations on how the client frames its input. A plain request answered
/// first shows the server answers probes at all; each variant is then judged
/// only while the stream is known to be in step.
async fn framing(ctx: &mut Ctx<'_>) {
    const CHECKS: [&str; 5] = ["F4", "F6", "F7", "F3", "F5"];
    let opens_documents = match ctx.initialize().await {
        Ok(reply) => opens_documents(&reply),
        Err(InitFailure::Violation(why) | InitFailure::Absent(why) | InitFailure::Blocked(why)) => {
            for id in CHECKS {
                ctx.report(id, Verdict::Error, format!("initialize didn't succeed: {why}"));
            }
            return;
        }
    };
    let t = ctx.cfg.request_timeout;
    if let Err(e) = ctx.s.call(PROBE, Some(json!({})), t).await {
        let (verdict, why) = match &e {
            CallError::Write(_) => (Verdict::Error, format!("lspcheck couldn't send a plain request: {e}")),
            CallError::Wait(_) => {
                let why = format!(
                    "a plain request sent whole went unanswered ({}), so the variants have nothing to compare with",
                    describe_call_error(&e, t)
                );
                match ctx.s.incomplete() {
                    Some(limit) => (Verdict::Error, format!("inconclusive, because {limit}: {why}")),
                    None => (Verdict::NotExercised, why),
                }
            }
        };
        for id in CHECKS {
            ctx.report(id, verdict, why.clone());
        }
        return;
    }

    let probe = |ctx: &mut Ctx<'_>, params: Value| {
        let id = ctx.s.fresh_id();
        let body = serde_json::to_vec(&message::request(&id, PROBE, Some(params))).expect("JSON values serialize");
        (id, body)
    };
    let mut broken: Option<String> = None;
    let skip = |ctx: &mut Ctx<'_>, id: &str, broken: &Option<String>| match broken {
        Some(why) => {
            ctx.report(id, Verdict::Error, format!("not judged: {why}"));
            true
        }
        None => false,
    };

    // F4: two requests and a notification in one write.
    if !skip(ctx, "F4", &broken) {
        let (a, first) = probe(ctx, json!({}));
        let (b, second) = probe(ctx, json!({}));
        let mut bytes = transport::encode(&first);
        bytes.extend(frame(&message::notification("$/setTrace", Some(json!({"value": "off"})))));
        bytes.extend(transport::encode(&second));
        let outcome = exchange(ctx, &[a, b], &bytes, &[]).await;
        broken = settle(ctx, "F4", "two requests and a notification written in one write", outcome).await;
    }

    // F6 and F7: a Content-Type header, in the current and the legacy spelling.
    for (check, charset) in [("F6", "utf-8"), ("F7", "utf8")] {
        if skip(ctx, check, &broken) {
            continue;
        }
        let value = format!("application/vscode-jsonrpc; charset={charset}");
        let (id, body) = probe(ctx, json!({}));
        let bytes = transport::encode_with_type(&body, &value);
        let outcome = exchange(ctx, &[id], &bytes, &[]).await;
        broken = settle(ctx, check, &format!("a request with Content-Type: {value}"), outcome).await;
    }

    // F3, first part: every header byte in its own write, the body cut at seeded points.
    let mut split_read = false;
    if !skip(ctx, "F3", &broken) {
        let (id, body) = probe(ctx, json!({"text": "written in pieces"}));
        let bytes = transport::encode(&body);
        let header = bytes.len() - body.len();
        let seed = session::random_u64();
        let mut body_cuts: Vec<usize> =
            (0..3).map(|i| header + 1 + ((seed >> (i * 16)) as usize % (body.len() - 1))).collect();
        body_cuts.sort_unstable();
        body_cuts.dedup();
        let cuts: Vec<usize> = (1..=header).chain(body_cuts.iter().copied()).collect();
        let what = format!(
            "a request written in {} pieces (each header byte alone, then the body cut at bytes {body_cuts:?})",
            cuts.len() + 1
        );
        ctx.s.note(format!("F3: {what}"));
        match exchange(ctx, &[id], &bytes, &cuts).await {
            Probe::Answered => split_read = true,
            outcome => broken = settle(ctx, "F3", &what, outcome).await,
        }
    }

    // F5: a document with multibyte characters, if the server asks for
    // didOpen, then a request that also has some.
    let mut f5_passed = false;
    if !skip(ctx, "F5", &broken) {
        let opened = if opens_documents {
            let path = ctx.root.join("lspcheck-utf8.txt");
            let text = format!("ASCII line\n{MULTIBYTE}\nlast line\n");
            let _ = std::fs::write(&path, &text);
            ctx.s.record_workspace(ctx.root.clone(), workspace_files(&ctx.root));
            let doc = json!({"uri": file_uri(&path), "languageId": "plaintext", "version": 1, "text": text});
            ctx.s.notify("textDocument/didOpen", Some(json!({"textDocument": doc}))).await.map_err(|e| e.to_string())
        } else {
            Ok(())
        };
        let outcome = match opened {
            Err(e) => Probe::Unsent(format!("didOpen: {e}")),
            Ok(()) => {
                let (id, body) = probe(ctx, json!({"text": MULTIBYTE}));
                exchange(ctx, &[id], &transport::encode(&body), &[]).await
            }
        };
        f5_passed = matches!(outcome, Probe::Answered);
        if f5_passed && !opens_documents {
            ctx.pass_with("F5", "the server doesn't ask for didOpen, so only a request with such characters was sent");
        } else {
            let what = if opens_documents {
                "a didOpen whose text has 2-, 3- and 4-byte UTF-8 characters, then a request with some too"
            } else {
                "a request with 2-, 3- and 4-byte UTF-8 characters (no didOpen: the server doesn't ask for it)"
            };
            broken = settle(ctx, "F5", what, outcome).await;
        }
    }

    // Output with non-ASCII text for F2: a response must echo this string
    // id. Sent only once the server is known to read such input.
    if f5_passed {
        let id = Id::Str(format!("lspcheck-é😀-{}", session::random_u64() % 1_000_000_000));
        let deadline = Instant::now() + t;
        let answered = ctx.s.request_with_id(&id, PROBE, Some(json!({"text": MULTIBYTE}))).await.is_ok()
            && ctx.s.wait_response(&id, deadline).await.is_ok();
        if !answered {
            broken = back_in_step(ctx, "a request with a non-ASCII string id went unanswered").await;
        }
    }

    // F3, second part: writes that end inside multibyte characters. If F5
    // passed, the stream is in step and multibyte input is read correctly.
    if split_read && !f5_passed {
        ctx.pass_with("F3", "the part that splits multibyte characters wasn't run, because F5 didn't pass");
    } else if let (true, Some(why)) = (split_read, &broken) {
        ctx.pass_with("F3", format!("the part that splits multibyte characters wasn't run: {why}"));
    } else if split_read {
        let (id, body) = probe(ctx, json!({"text": MULTIBYTE}));
        let bytes = transport::encode(&body);
        let header = bytes.len() - body.len();
        let text = String::from_utf8_lossy(&body).into_owned();
        // Inside a 2-, a 3- and a 4-byte character (twice).
        let cuts: Vec<usize> = [("é", 1), ("€", 2), ("😀", 1), ("😀", 3)]
            .iter()
            .filter_map(|(c, k)| text.find(c).map(|at| header + at + k))
            .collect();
        let what = format!("a request with multibyte characters written in pieces cut inside them, at {cuts:?}");
        ctx.s.note(format!("F3: {what}"));
        let outcome = exchange(ctx, &[id], &bytes, &cuts).await;
        broken = settle(ctx, "F3", &what, outcome).await;
    }

    // Leave politely so the server isn't killed mid-thought.
    if broken.is_none() && ctx.s.call("shutdown", None, t).await.is_ok() {
        let _ = ctx.s.notify("exit", None).await;
        let _ = ctx.s.wait_exit(Instant::now() + ctx.cfg.exit_timeout).await;
    }
}

/// Whether the server asks for didOpen and didClose: a sync kind other than
/// None, or `openClose: true`. Without it the client mustn't send them.
fn opens_documents(reply: &Message) -> bool {
    let Kind::Response { outcome: Outcome::Result(result), .. } = &reply.kind else { return false };
    match result.pointer("/capabilities/textDocumentSync") {
        Some(Value::Number(kind)) => kind.as_i64().is_some_and(|k| k > 0),
        Some(Value::Object(options)) => options.get("openClose") == Some(&Value::Bool(true)),
        _ => false,
    }
}

const OFFERED_ENCODINGS: [&str; 3] = ["utf-8", "utf-32", "utf-16"];

fn judge_l2(ctx: &mut Ctx<'_>, reply: &Message) {
    let Kind::Response { outcome: Outcome::Result(result), .. } = &reply.kind else { unreachable!() };
    let Some(caps) = result.get("capabilities") else {
        return ctx.fail("L2", "the result has no capabilities");
    };
    if !caps.is_object() {
        return ctx.fail("L2", format!("capabilities is {caps}, not an object"));
    }
    match caps.get("positionEncoding") {
        None => ctx.pass("L2"),
        Some(Value::String(enc)) if OFFERED_ENCODINGS.contains(&enc.as_str()) => ctx.pass("L2"),
        Some(other) => {
            ctx.fail("L2", format!("positionEncoding {other} is not one of the offered {OFFERED_ENCODINGS:?}"))
        }
    }
}

/// The scenarios whose server started, with what they observed.
fn observed(runs: &[ScenarioRun]) -> impl Iterator<Item = (&'static str, &Observations)> {
    runs.iter().filter_map(|r| r.observed.as_ref().map(|o| (r.name, o)))
}

/// A verdict for a cross-cutting check from per-scenario failures.
/// `uses_frames`: whether skipped frames could hide a failure of this check.
fn cross(id: &str, runs: &[ScenarioRun], failures: Vec<String>, pass_detail: &str, uses_frames: bool) -> CheckResult {
    if !failures.is_empty() {
        return result("all", id, Verdict::Fail, failures.join("; "));
    }
    if observed(runs).next().is_none() {
        return result("all", id, Verdict::Error, "no scenario could start the server");
    }
    if uses_frames && let Some((name, (_, why))) = observed(runs).find_map(|(n, o)| o.limit.as_ref().map(|l| (n, l))) {
        return result("all", id, Verdict::Error, format!("inconclusive: in {name}, {why}"));
    }
    result("all", id, Verdict::Pass, pass_detail)
}

fn judge_f1(runs: &[ScenarioRun]) -> CheckResult {
    let failures = observed(runs)
        .filter_map(|(name, o)| {
            let (at, p) = o.problems.first()?;
            let more = o.problems.len() - 1;
            let tail = if more > 0 { format!(" (and {more} more)") } else { String::new() };
            Some(format!("in {name} at {} ms: {p}{tail}", at.as_millis()))
        })
        .collect();
    cross("F1", runs, failures, "", true)
}

fn judge_f2(runs: &[ScenarioRun]) -> CheckResult {
    let failures = observed(runs)
        .filter_map(|(name, o)| {
            let (at, what) = o.miscounted.first()?;
            let more = o.miscounted.len() - 1;
            let tail = if more > 0 { format!(" (and {more} more)") } else { String::new() };
            Some(format!("in {name} at {} ms: {what}{tail}", at.as_millis()))
        })
        .collect();
    let r = cross("F2", runs, failures, "", true);
    if r.verdict == Verdict::Pass
        && let Some((name, o)) = observed(runs).find(|(_, o)| o.unchecked > 0)
    {
        return result(
            "all",
            "F2",
            Verdict::Error,
            format!(
                "inconclusive: in {name}, lspcheck couldn't finish checking {} unreadable frame(s) within its limits",
                o.unchecked
            ),
        );
    }
    if r.verdict == Verdict::Pass && observed(runs).all(|(_, o)| o.non_ascii.is_none()) {
        return result(
            "all",
            "F2",
            Verdict::NotExercised,
            "no frame the server sent had non-ASCII text in its body (escaping it as \\u is lawful)",
        );
    }
    r
}

fn judge_f8(runs: &[ScenarioRun]) -> CheckResult {
    let failures = observed(runs)
        .filter_map(|(name, o)| {
            let (at, value) = o
                .content_types
                .iter()
                .find(|(_, v)| transport::charset(v).is_some_and(|c| c != "utf-8" && c != "utf8"))?;
            Some(format!("in {name} at {} ms: Content-Type {value:?}", at.as_millis()))
        })
        .collect();
    cross("F8", runs, failures, "", true)
}

/// `r`, or an error when it would pass but a frame lspcheck couldn't read
/// (see F1) might have been a response or message the check judges.
fn unless_unreadable(runs: &[ScenarioRun], r: CheckResult) -> CheckResult {
    if r.verdict != Verdict::Pass {
        return r;
    }
    let unread = observed(runs).find_map(|(name, o)| {
        o.problems.iter().find(|(_, p)| !matches!(p, Problem::Garbage(_))).map(|(at, _)| (name, *at))
    });
    match unread {
        Some((name, at)) => CheckResult {
            verdict: Verdict::Error,
            detail: format!("inconclusive: in {name} at {} ms, a frame couldn't be read (see F1)", at.as_millis()),
            ..r
        },
        None => r,
    }
}

fn judge_j1(runs: &[ScenarioRun]) -> CheckResult {
    let failures = observed(runs)
        .filter_map(|(name, o)| {
            let bad: Vec<&Unmatched> = o.unmatched.iter().filter(|u| u.loose.is_some()).collect();
            let first = bad.first()?;
            let more = bad.len() - 1;
            let tail = if more > 0 { format!(" (and {more} more)") } else { String::new() };
            let sent = first.loose.as_ref().expect("filtered for a loosely equal id");
            Some(format!(
                "in {name} at {} ms: a response id {} where lspcheck sent {sent}{tail}",
                first.at.as_millis(),
                first.id
            ))
        })
        .collect();
    let r = cross("J1", runs, failures, "", true);
    if r.verdict != Verdict::Pass {
        return r;
    }
    // A matching value proves little unless both JSON types were seen: a
    // server that always answers with a number would pass every numeric id.
    let answered: Vec<&Id> = observed(runs).flat_map(|(_, o)| o.answered.iter().map(|(_, id)| id)).collect();
    let numbers = answered.iter().any(|id| matches!(id, Id::Num { .. }));
    let strings = answered.iter().any(|id| matches!(id, Id::Str(_)));
    if numbers && strings {
        return r;
    }
    let missing = match (numbers, strings) {
        (false, false) => "a numeric or a string",
        (false, true) => "a numeric",
        _ => "a string",
    };
    result("all", "J1", Verdict::NotExercised, format!("no request with {missing} id was answered with exactly its id"))
}

/// Whether an unmatched response breaks J2, rather than being lawful (a
/// null id) or J1's finding (an id only loosely equal to one sent).
fn j2_blames(u: &Unmatched) -> bool {
    u.loose.is_none() && u.id != Id::Null && !u.aborted && (u.duplicate || u.early || u.unsent)
}

fn describe_unmatched(u: &Unmatched) -> String {
    if u.duplicate {
        format!("a second response for id {}, whose request was already answered", u.id)
    } else if u.early {
        format!("a response id {} read before its request was sent", u.id)
    } else {
        format!("a response id {}, which lspcheck never sent", u.id)
    }
}

fn judge_j2(runs: &[ScenarioRun]) -> CheckResult {
    let failures = observed(runs)
        .filter_map(|(name, o)| {
            let bad: Vec<&Unmatched> = o.unmatched.iter().filter(|u| j2_blames(u)).collect();
            let first = bad.first()?;
            let more = bad.len() - 1;
            let tail = if more > 0 { format!(" (and {more} more)") } else { String::new() };
            Some(format!("in {name} at {} ms: {}{tail}", first.at.as_millis(), describe_unmatched(first)))
        })
        .collect();
    let r = cross("J2", runs, failures, "", true);
    if r.verdict == Verdict::Pass && observed(runs).all(|(_, o)| o.answered.is_empty() && o.unmatched.is_empty()) {
        return result("all", "J2", Verdict::NotExercised, "the server sent no response lspcheck could judge");
    }
    r
}

/// The scenarios whose input is all well-formed, so a response without an id
/// answers some notification. `framing` is left out because its variants can
/// make a server misread the input (a null-id ParseError is then lawful), and
/// `pre-initialize` because L1 judges what happens there.
const J3_SCENARIOS: &[&str] = &[
    "lifecycle",
    "json-rpc",
    "after-initialize",
    "cancellation",
    "documents",
    "end-of-input",
    "exit-without-shutdown",
];

/// J3 across the scenarios in [`J3_SCENARIOS`]: no response without an id
/// (`id` missing or null) is read after `initialized`, to the end of the
/// session (a reply to `exit` answers a notification too), or until a write
/// of lspcheck's stopped partway through a frame: the server may answer that
/// malformed input with a null-id error. (Such a write also makes the
/// session's evidence incomplete, so a pass becomes an error.)
fn judge_j3(runs: &[ScenarioRun]) -> CheckResult {
    let mut failures = Vec::new();
    let mut incomplete: Option<(&'static str, String)> = None;
    let mut exercise: Option<(Verdict, String)> = None;
    for (name, o) in observed(runs).filter(|(name, _)| J3_SCENARIOS.contains(name)) {
        if name == "json-rpc" {
            exercise = o.j3_exercise.clone();
        }
        let Some(from) = o.initialized_at else { continue };
        let to = o.abandoned_at.unwrap_or(Duration::MAX);
        if let Some((at, what)) = o.messages.iter().find_map(|(at, m)| {
            if *at < from || *at >= to {
                return None;
            }
            reply_to_notification(m).map(|what| (*at, what))
        }) {
            failures.push(format!("in {name} at {} ms, a notification was answered: {what}", at.as_millis()));
        }
        if incomplete.is_none()
            && let Some(why) = &o.j3_incomplete
        {
            incomplete = Some((name, why.clone()));
        }
    }
    if !failures.is_empty() {
        return result("all", "J3", Verdict::Fail, failures.join("; "));
    }
    match exercise {
        Some((Verdict::Error, why)) => return result("all", "J3", Verdict::Error, why),
        Some((Verdict::Fail, why)) => return result("all", "J3", Verdict::Fail, why),
        Some((Verdict::Pass, _)) => {}
        Some(_) => unreachable!("check_j3 returns a pass, failure or error"),
        None => {
            return result(
                "all",
                "J3",
                Verdict::Error,
                "the json-rpc scenario didn't send its unknown notifications, so J3 wasn't exercised",
            );
        }
    }
    if let Some((name, why)) = incomplete {
        return result("all", "J3", Verdict::Error, format!("inconclusive: in {name}, {why}"));
    }
    result("all", "J3", Verdict::Pass, "no response answered a notification")
}

/// Why a message's `params`, if it has one, isn't a structured value.
fn params_problem(m: &Message) -> Option<String> {
    let params = m.value.get("params")?;
    if params.is_object() || params.is_array() {
        return None;
    }
    Some(format!(
        "params is {}, not an object or an array: {}",
        params,
        session::preview(m.value.to_string().as_bytes())
    ))
}

/// Why a message isn't a well-formed JSON-RPC 2.0 envelope, if it isn't.
fn envelope_problem(m: &Message) -> Option<String> {
    match &m.kind {
        Kind::Invalid(why) => Some(format!("an invalid message: {why}")),
        Kind::Response { outcome: Outcome::Malformed(why), .. } => Some(format!("a response that {why}")),
        Kind::Response { outcome: Outcome::Error { code: None, error, .. }, .. } => {
            Some(format!("an error whose code isn't an integer: {}", session::preview(error.to_string().as_bytes())))
        }
        Kind::Response { outcome: Outcome::Error { code: Some(code), .. }, .. } if i32::try_from(*code).is_err() => {
            Some(format!("an error code {code}, outside LSP's integer range"))
        }
        Kind::Response { outcome: Outcome::Error { message: None, error, .. }, .. } => {
            Some(format!("an error whose message isn't a string: {}", session::preview(error.to_string().as_bytes())))
        }
        // A response's id is a string, a number or null. Numbers must be whole
        // and in LSP's integer range (signed 32-bit): `2.0` is an integer id,
        // `2.5` and `2147483648` aren't.
        Kind::Response { id: Id::Other(raw), .. } => {
            Some(format!("a response with id {raw}, which is neither a string, a number nor null"))
        }
        Kind::Response { id: id @ Id::Num { raw, .. }, .. } if !id.is_lsp_integer() => {
            Some(format!("a response with id {raw}, which isn't an integer in LSP's range"))
        }
        Kind::Request { id, .. } => match id {
            Id::Null => Some("a request with id null".into()),
            Id::Other(raw) => Some(format!("a request with id {raw}, which is neither a string nor a number")),
            Id::Num { raw, .. } if !id.is_lsp_integer() => {
                Some(format!("a request with id {raw}, which isn't an integer in LSP's range"))
            }
            _ => params_problem(m),
        },
        Kind::Notification { .. } => params_problem(m),
        _ => None,
    }
}

fn judge_j5(runs: &[ScenarioRun]) -> CheckResult {
    let failures = observed(runs)
        .filter_map(|(name, o)| {
            let bad: Vec<(Duration, String)> =
                o.messages.iter().filter_map(|(at, m)| envelope_problem(m).map(|why| (*at, why))).collect();
            let (at, why) = bad.first()?;
            let more = bad.len() - 1;
            let tail = if more > 0 { format!(" (and {more} more)") } else { String::new() };
            Some(format!("in {name} at {} ms: {why}{tail}", at.as_millis()))
        })
        .collect();
    let r = cross("J5", runs, failures, "", true);
    if r.verdict == Verdict::Pass && observed(runs).all(|(_, o)| o.messages.is_empty()) {
        return result("all", "J5", Verdict::NotExercised, "the server sent no readable message");
    }
    r
}

fn judge_l9(runs: &[ScenarioRun]) -> CheckResult {
    let all = observed(runs).collect::<Vec<_>>();
    if all.iter().any(|(_, o)| o.coverage == Coverage::None) {
        return result("all", "L9", Verdict::NotExercised, "lspcheck can't track descendants on this platform yet");
    }
    let failures: Vec<String> = all
        .iter()
        .filter(|(_, o)| !o.killed && !o.leftovers.is_empty())
        .map(|(name, o)| {
            let list: Vec<String> = o.leftovers.iter().map(ProcInfo::to_string).collect();
            format!("in {name}, still running after the server exited: {}", list.join(", "))
        })
        .collect();
    // A leak seen in one scenario stands even if another's search failed.
    if failures.is_empty()
        && let Some((name, o)) = all.iter().find(|(_, o)| o.scan_error.is_some())
    {
        let why = o.scan_error.as_deref().unwrap_or("");
        return result("all", "L9", Verdict::Error, format!("in {name}, the process search failed: {why}"));
    }
    if failures.is_empty() && !all.is_empty() && all.iter().all(|(_, o)| o.killed) {
        return result(
            "all",
            "L9",
            Verdict::NotExercised,
            "lspcheck had to stop the server in every scenario, so its cleanup wasn't tested",
        );
    }
    let mut detail = String::new();
    if all.iter().any(|(_, o)| o.coverage == Coverage::GroupOnly) {
        detail =
            "searched the server's process group only; a descendant that called setsid wouldn't be seen here".into();
    }
    let not_stopped: Vec<String> = all.iter().flat_map(|(_, o)| &o.not_stopped).map(ProcInfo::to_string).collect();
    if !not_stopped.is_empty() {
        detail = format!(
            "{detail}{}lspcheck couldn't stop {}",
            if detail.is_empty() { "" } else { "; " },
            not_stopped.join(", ")
        );
    }
    let mut r = cross("L9", runs, failures, &detail, false);
    if r.verdict == Verdict::Fail && !detail.is_empty() {
        r.detail = format!("{}; {detail}", r.detail);
    }
    r
}

fn describe_outcome(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Result(v) => format!("result {}", session::preview(v.to_string().as_bytes())),
        Outcome::Error { code: Some(code), message, .. } => {
            format!("error {code}{}", message.as_deref().map(|m| format!(" ({m})")).unwrap_or_default())
        }
        Outcome::Error { error, .. } => format!("an error without an integer code: {error}"),
        Outcome::Malformed(why) => format!("a malformed response that {why}"),
    }
}

fn describe_call_error(e: &CallError, timeout: Duration) -> String {
    match e {
        CallError::Wait(WaitError::Timeout) => {
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

/// The files in a workspace, as (relative name, text) pairs, for the
/// workspace line of a transcript.
fn workspace_files(root: &Path) -> Vec<(String, String)> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file()
                && let Ok(text) = std::fs::read_to_string(&path)
            {
                files.push((entry.file_name().to_string_lossy().into_owned(), text));
            }
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
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
        assert_eq!(file_uri(Path::new("/tmp/a b/é")), "file:///tmp/a%20b/%C3%A9");
        assert_eq!(file_uri(Path::new("C:\\x y")), "file:///C:/x%20y");
    }

    fn observed_run(name: &'static str, leftovers: Vec<ProcInfo>, scan_error: Option<&str>) -> ScenarioRun {
        ScenarioRun {
            name,
            results: Vec::new(),
            observed: Some(Observations {
                problems: Vec::new(),
                content_types: Vec::new(),
                non_ascii: None,
                miscounted: Vec::new(),
                unchecked: 0,
                killed: false,
                leftovers,
                coverage: Coverage::GroupAndEscaped,
                scan_error: scan_error.map(str::to_string),
                not_stopped: Vec::new(),
                limit: None,
                messages: Vec::new(),
                initialized_at: None,
                j3_incomplete: None,
                abandoned_at: None,
                j3_exercise: None,
                unmatched: Vec::new(),
                answered: Vec::new(),
            }),
            transcript: None,
        }
    }

    #[test]
    fn a_leak_stands_when_another_scenarios_search_failed() {
        let leak = ProcInfo { pid: 42, ppid: 1, pgid: 42, name: "helper".into() };
        let runs =
            [observed_run("lifecycle", vec![leak], None), observed_run("pre-initialize", vec![], Some("no /proc"))];
        let r = judge_l9(&runs);
        assert_eq!(r.verdict, Verdict::Fail, "{}", r.detail);
        assert!(r.detail.contains("helper"), "{}", r.detail);
        // Without the leak, the failed search makes the result unknown.
        let runs = [observed_run("lifecycle", vec![], None), observed_run("pre-initialize", vec![], Some("no /proc"))];
        assert_eq!(judge_l9(&runs).verdict, Verdict::Error);
    }

    #[test]
    fn j3_doesnt_blame_a_parse_error_for_lspchecks_own_truncated_output() {
        // After a write of lspcheck's stopped partway through a frame, a
        // null-id ParseError lawfully answers that malformed input.
        let parse_error =
            Message::parse(br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}"#).unwrap();
        let run = |abandoned_at: Option<Duration>| {
            let mut r = observed_run("lifecycle", vec![], None);
            let o = r.observed.as_mut().unwrap();
            o.initialized_at = Some(Duration::from_millis(10));
            o.abandoned_at = abandoned_at;
            o.messages = vec![(Duration::from_millis(500), parse_error.clone())];
            let mut exercise = observed_run("json-rpc", vec![], None);
            exercise.observed.as_mut().unwrap().j3_exercise = Some((Verdict::Pass, String::new()));
            [r, exercise]
        };
        assert_eq!(judge_j3(&run(None)).verdict, Verdict::Fail);
        assert_ne!(judge_j3(&run(Some(Duration::from_millis(100)))).verdict, Verdict::Fail);
        // A reply read before the truncated write began still counts.
        assert_eq!(judge_j3(&run(Some(Duration::from_millis(600)))).verdict, Verdict::Fail);
    }

    #[test]
    fn j5_recognises_bad_envelopes() {
        let bad = |s: &str| envelope_problem(&Message::parse(s.as_bytes()).unwrap());
        // Both or neither of result and error.
        assert!(bad(r#"{"jsonrpc":"2.0","id":1,"result":null,"error":{}}"#).is_some());
        assert!(bad(r#"{"jsonrpc":"2.0","id":1}"#).is_some());
        // A missing or wrong jsonrpc field.
        assert!(bad(r#"{"id":1,"result":null}"#).is_some());
        assert!(bad(r#"{"jsonrpc":"1.0","id":1,"result":null}"#).is_some());
        // An error code that isn't an integer, or a message that isn't a string.
        assert!(bad(r#"{"jsonrpc":"2.0","id":1,"error":{"code":"-32601","message":"x"}}"#).is_some());
        assert!(bad(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":7}}"#).is_some());
        // Request ids that JSON-RPC doesn't allow.
        assert!(bad(r#"{"jsonrpc":"2.0","id":null,"method":"m"}"#).is_some());
        assert!(bad(r#"{"jsonrpc":"2.0","id":1.5,"method":"m"}"#).is_some());
        assert!(bad(r#"{"jsonrpc":"2.0","id":true,"method":"m"}"#).is_some());
        // Response ids: a string, a whole number or null. `2.0` is a whole
        // number spelled with a fraction, so it stays lawful.
        assert!(bad(r#"{"jsonrpc":"2.0","id":true,"result":null}"#).is_some());
        assert!(bad(r#"{"jsonrpc":"2.0","id":[1],"result":null}"#).is_some());
        assert!(bad(r#"{"jsonrpc":"2.0","id":2.5,"result":null}"#).is_some());
        assert!(bad(r#"{"jsonrpc":"2.0","id":2.0,"result":null}"#).is_none());
        // params, when present, is an object or an array.
        assert!(bad(r#"{"jsonrpc":"2.0","id":1,"method":"m","params":3}"#).is_some());
        assert!(bad(r#"{"jsonrpc":"2.0","method":"m","params":"x"}"#).is_some());
        assert!(bad(r#"{"jsonrpc":"2.0","method":"m","params":null}"#).is_some());
        // Well-formed envelopes.
        assert!(bad(r#"{"jsonrpc":"2.0","id":1,"method":"m"}"#).is_none());
        assert!(bad(r#"{"jsonrpc":"2.0","id":1,"method":"m","params":{}}"#).is_none());
        assert!(bad(r#"{"jsonrpc":"2.0","id":1,"method":"m","params":[]}"#).is_none());
        assert!(bad(r#"{"jsonrpc":"2.0","method":"m"}"#).is_none());
        assert!(bad(r#"{"jsonrpc":"2.0","id":1,"result":null}"#).is_none());
        assert!(bad(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"x"}}"#).is_none());
    }

    #[test]
    fn j2_blames_only_unexplained_unmatched_responses() {
        let u = |id: Id| Unmatched {
            at: Duration::ZERO,
            id,
            duplicate: false,
            unsent: false,
            early: false,
            aborted: false,
            loose: None,
        };
        let mut unsent = u(Id::int(1));
        unsent.unsent = true;
        assert!(j2_blames(&unsent));
        let mut duplicate = u(Id::int(1));
        duplicate.duplicate = true;
        assert!(j2_blames(&duplicate));
        let mut early = u(Id::int(1));
        early.early = true;
        assert!(j2_blames(&early));
        // A response to a write that failed, a null id, and J1's loose ids are
        // lawful as far as J2 is concerned.
        let mut aborted = u(Id::int(1));
        aborted.aborted = true;
        assert!(!j2_blames(&aborted));
        let mut null = u(Id::Null);
        null.unsent = true;
        assert!(!j2_blames(&null));
        let mut loose = u(Id::int(1));
        loose.unsent = true;
        loose.loose = Some(Id::Str("1".into()));
        assert!(!j2_blames(&loose));
    }

    #[test]
    fn every_check_is_reported_exactly_once() {
        let mut seen: Vec<&str> = CROSS_CUTTING.to_vec();
        for s in SCENARIOS {
            for id in s.checks {
                assert!(!seen.contains(id), "{id} is reported twice");
                seen.push(id);
            }
        }
        for id in &seen {
            info(id);
        }
        assert_eq!(seen.len(), CATALOGUE.len(), "every catalogued check is reported");
    }
}
