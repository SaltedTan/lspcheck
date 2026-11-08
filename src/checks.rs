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
use crate::session::{
    self, CallError, Finish, Options, Problem, Recv, ServerRequest, Session, Unmatched, WaitError, WriteError,
};
use crate::text::{self, ContentChange, Encoding, Position};
use crate::transcript;
use crate::transport;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
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
    CapabilitiesAndRobustness,
}

impl Area {
    /// The area's name in reports.
    pub fn label(self) -> &'static str {
        match self {
            Area::Framing => "Framing",
            Area::JsonRpc => "JSON-RPC",
            Area::Lifecycle => "Lifecycle",
            Area::Cancellation => "Cancellation",
            Area::Documents => "Documents",
        }
    }
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
        "J7",
        JsonRpc,
        Must,
        "The server's own requests don't reuse an unanswered id, and it keeps serving after they are answered",
        "requestMessage"
    ),
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
    check!(
        "D4",
        Documents,
        Policy,
        "A request about a document that isn't open gets a response, and the server keeps serving",
        "textDocumentSync"
    ),
    check!("D5", Documents, Must, "Lines are split at \\n, \\r\\n and \\r", "textDocumentSync"),
    check!(
        "D6",
        Documents,
        Must,
        "A selectionRange result has one entry per position, each range contains its position, and each parent contains its child",
        "textDocument_selectionRange"
    ),
    check!(
        "R2",
        CapabilitiesAndRobustness,
        Policy,
        "A request with parameters of the wrong types gets an error response (InvalidParams preferred), and the server keeps serving",
        "responseMessage"
    ),
    check!(
        "R4",
        CapabilitiesAndRobustness,
        Policy,
        "Unusual but valid request ids round-trip exactly: 0, negative, the 32-bit extremes, an empty string, a long string, a non-ASCII string",
        "requestMessage"
    ),
];

/// Checks judged from every scenario's observations.
pub const CROSS_CUTTING: &[&str] = &["F1", "F2", "F8", "J1", "J2", "J3", "J5", "J7", "L9"];

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
    /// sent what should have been answered, and when it stopped waiting.
    /// Evidence lspcheck didn't see in between could explain it. A failure
    /// seen directly has none.
    #[serde(skip)]
    pub absent_at: Option<(Duration, Duration)>,
    /// For a verdict reached during the session on the assumption that
    /// lspcheck missed nothing in a window: what it would have been
    /// otherwise. Checked again when the session has ended and everything
    /// the server sent has been read, since a frame unfinished at the time
    /// may turn out to have been cut short.
    #[serde(skip)]
    pub unless_missed: Option<Recheck>,
}

/// A verdict's alternative if lspcheck turns out to have missed something
/// between `from` and `to`. [`MISSED`] in `detail` stands for the reason.
#[derive(Debug, Clone)]
pub struct Recheck {
    pub from: Duration,
    pub to: Duration,
    pub output_only: bool,
    pub verdict: Verdict,
    pub detail: String,
}

/// Where a [`Recheck`]'s detail gives the reason something was missed.
const MISSED: &str = "\u{1}missed\u{1}";

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
    /// Requests lspcheck sent whole, with when each was registered.
    pub sent: Vec<(Duration, Id)>,
    /// The server's requests, in the order they were read.
    pub server_requests: Vec<ServerRequest>,
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
    Scenario { name: "odd-ids", checks: &["R4"] },
    Scenario { name: "pre-initialize", checks: &["L1"] },
    Scenario { name: "exit-without-shutdown", checks: &["L7"] },
    Scenario { name: "framing", checks: &["F4", "F6", "F7", "F3", "F5"] },
    Scenario { name: "after-initialize", checks: &["L3", "L5"] },
    Scenario { name: "cancellation", checks: &["C1", "C2", "C3"] },
    Scenario { name: "documents", checks: &["D1", "D2", "D3", "D4", "D5", "D6"] },
    Scenario { name: "wrong-params", checks: &["R2"] },
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
        unless_unreadable(&runs, judge_j7(&runs)),
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
        "odd-ids" => odd_ids(&mut ctx).await,
        "pre-initialize" => pre_initialize(&mut ctx).await,
        "exit-without-shutdown" => exit_without_shutdown(&mut ctx).await,
        "framing" => framing(&mut ctx).await,
        "after-initialize" => after_initialize(&mut ctx).await,
        "cancellation" => cancellation(&mut ctx).await,
        "documents" => documents(&mut ctx).await,
        "wrong-params" => wrong_params(&mut ctx).await,
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
    // evidence lspcheck didn't see while it waited: frames it skipped or
    // couldn't read after it sent what should have been answered (a log line
    // loses nothing, so it doesn't count), or, from any time before it
    // stopped waiting, reading that stopped or its own side of the
    // conversation failing. A failure seen directly, such as a wrong result,
    // stands, and so does one whose wait had nothing go unseen.
    for r in results.iter_mut().filter(|r| r.verdict == Verdict::Fail) {
        let Some((since, waited)) = r.absent_at else { continue };
        if let Some(why) = finish.incomplete_within(since, waited) {
            r.verdict = Verdict::Error;
            r.detail = format!("inconclusive, because {why}: {}", r.detail);
        }
    }
    // A verdict reached during the session that assumed nothing was missed
    // in its window, judged again now that everything read is known.
    for r in &mut results {
        let Some(again) = r.unless_missed.take() else { continue };
        let missed = if again.output_only {
            finish.missed_output_within(again.from, again.to)
        } else {
            finish.incomplete_within(again.from, again.to)
        };
        if let Some(why) = missed {
            r.verdict = again.verdict;
            r.detail = again.detail.replace(MISSED, &why);
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
        sent: f.sent,
        server_requests: f.server_requests,
    }
}

fn result(scenario: &'static str, id: &str, verdict: Verdict, detail: impl Into<String>) -> CheckResult {
    CheckResult { id: info(id).id, verdict, detail: detail.into(), scenario, absent_at: None, unless_missed: None }
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

    /// Reports a verdict that depends on lspcheck having missed nothing
    /// between `from` and `to`. `judge` gives the verdict and detail, given
    /// why something may have been missed if it may have. What lspcheck finds
    /// only later that it missed in that window is judged again when the
    /// session ends.
    fn report_judged(
        &mut self,
        id: &str,
        from: Duration,
        to: Duration,
        judge: impl Fn(Option<&str>) -> (Verdict, String),
    ) {
        let missed = self.s.incomplete_within(from, to);
        let (verdict, detail) = judge(missed.as_deref());
        self.report(id, verdict, detail);
        if missed.is_none() {
            let (verdict, detail) = judge(Some(MISSED));
            self.unless_missed(from, to, verdict, detail);
        }
    }

    /// What the result just reported becomes if lspcheck turns out to have
    /// missed something between `from` and `to`; `detail` gives the reason
    /// as [`MISSED`]. Nothing, if that is the same verdict.
    fn unless_missed(&mut self, from: Duration, to: Duration, verdict: Verdict, detail: impl Into<String>) {
        let r = self.results.last_mut().expect("just reported");
        if r.verdict != verdict {
            r.unless_missed = Some(Recheck { from, to, output_only: false, verdict, detail: detail.into() });
        }
    }

    fn pass_with(&mut self, id: &str, detail: impl Into<String>) {
        self.report(id, Verdict::Pass, detail);
    }

    fn fail(&mut self, id: &str, detail: impl Into<String>) {
        self.report(id, Verdict::Fail, detail);
    }

    /// A failure because something expected never arrived, in reply to what
    /// lspcheck sent from `since` on.
    fn fail_absent(&mut self, id: &str, since: Duration, detail: impl Into<String>) {
        let waited = self.s.elapsed();
        self.fail(id, detail);
        self.results.last_mut().expect("just reported").absent_at = Some((since, waited));
    }

    /// A failure because a reply lspcheck waited for from `since` to `waited`
    /// never arrived, judged before the session ended: what lspcheck only
    /// later finds it missed in that time still makes it inconclusive.
    fn fail_absent_between(&mut self, id: &str, since: Duration, waited: Duration, detail: impl Into<String>) {
        self.fail(id, detail);
        self.results.last_mut().expect("just reported").absent_at = Some((since, waited));
    }

    /// The verdict for a call sent from `since` on that got no response: a
    /// failure when the request was delivered, and an error when lspcheck
    /// couldn't deliver it.
    fn fail_call(&mut self, id: &str, since: Duration, detail: impl Into<String>, e: &CallError) {
        match e {
            CallError::Wait(_) => self.fail_absent(id, since, detail),
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
    let since = ctx.s.elapsed();
    let reply = match ctx.initialize().await {
        Ok(reply) => reply,
        Err(InitFailure::Violation(why)) => return ctx.fail("L2", why),
        Err(InitFailure::Absent(why)) => return ctx.fail_absent("L2", since, why),
        Err(InitFailure::Blocked(why)) => return ctx.report("L2", Verdict::Error, why),
    };
    judge_l2(ctx, &reply);

    let t = ctx.cfg.request_timeout;
    let mut problems = Vec::new();
    let (mut seen_wrong, mut unsent) = (false, false);
    // When the first request that went unanswered was sent.
    let mut since = None;
    for method in ["lspcheck/unknownRequest", "$/lspcheck/unknownRequest"] {
        let sent = ctx.s.elapsed();
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
                since.get_or_insert(sent);
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
        let since = since.expect("a request went unanswered");
        ctx.fail_absent("J4", since, problems.join("; "));
    }

    let since = ctx.s.elapsed();
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
        Err(e) => return ctx.fail_call("L4", since, format!("shutdown: {}", describe_call_error(&e, t)), &e),
    }

    if !ctx.s.is_alive() {
        return ctx.report("L6", Verdict::NotExercised, "the server exited before exit was sent (see L4)");
    }
    if let Err(e) = ctx.s.notify("exit", None).await {
        return ctx.report("L6", Verdict::Error, format!("could not send exit: {e}"));
    }
    match ctx.s.serve_until_exit(Instant::now() + ctx.cfg.exit_timeout).await {
        Some(status) if status.code() == Some(0) => ctx.pass("L6"),
        Some(status) => ctx.fail("L6", format!("exited with {}", session::describe_status(status))),
        None => still_running(ctx, "L6"),
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
        let _ = ctx.s.serve_until_exit(Instant::now() + ctx.cfg.exit_timeout).await;
    }
}

/// Requests whose ids cover the less common values allowed by LSP 3.17.
fn odd_request_ids() -> Vec<Id> {
    vec![
        Id::int(0),
        Id::int(-1),
        Id::int(i32::MAX.into()),
        Id::int(i32::MIN.into()),
        Id::Str(String::new()),
        Id::Str("a".repeat(1000)),
        Id::Str("é中😀𝄞".into()),
        Id::Str("7".into()),
    ]
}

fn describe_odd_id(id: &Id) -> String {
    match id {
        Id::Str(value) if value.chars().count() > 40 => {
            let prefix: String = value.chars().take(20).collect();
            format!("\"{prefix}…\" ({} characters)", value.chars().count())
        }
        _ => id.to_string(),
    }
}

/// One-at-a-time probes for valid request ids outside the usual positive range.
async fn odd_ids(ctx: &mut Ctx<'_>) {
    let ids = odd_request_ids();
    for id in &ids {
        // Keep initialize's random id and every later generated id clear of
        // the values this scenario deliberately sends.
        ctx.s.reserve_id(id.clone());
    }
    if let Err(InitFailure::Violation(why) | InitFailure::Absent(why) | InitFailure::Blocked(why)) =
        ctx.initialize().await
    {
        return ctx.report("R4", Verdict::Error, format!("initialize didn't succeed: {why}"));
    }

    let t = ctx.cfg.request_timeout;
    let started = ctx.s.elapsed();
    let mut missing = Vec::new();
    let mut undelivered = Vec::new();
    let mut first_missing = None;
    let mut stopped = None;
    for (index, id) in ids.iter().enumerate() {
        let sent = ctx.s.elapsed();
        match ctx.s.call_with_id(id, PROBE, Some(json!({})), t).await {
            Ok(_) => {}
            Err(CallError::Wait(error)) => {
                missing.push(describe_odd_id(id));
                first_missing.get_or_insert(sent);
                if error == WaitError::Closed {
                    stopped = match ctx.s.wait_exit(Instant::now() + ctx.cfg.exit_timeout).await {
                        Some(status) => Some(format!("the server exited with {}", session::describe_status(status))),
                        None if !ctx.s.is_alive() => Some("the server exited".into()),
                        None => Some("the server closed its stdout".into()),
                    };
                    undelivered.extend(ids[index + 1..].iter().map(describe_odd_id));
                    break;
                }
            }
            Err(CallError::Write(error)) => {
                undelivered.extend(ids[index..].iter().map(describe_odd_id));
                stopped = ctx
                    .s
                    .wait_exit(Instant::now() + ctx.cfg.exit_timeout)
                    .await
                    .map(|status| format!("the server exited with {}", session::describe_status(status)));
                if stopped.is_none() && !ctx.s.is_alive() {
                    stopped = Some("the server exited".into());
                }
                if missing.is_empty() {
                    // This request was not delivered, so it cannot establish a
                    // protocol failure even if an earlier response was slow.
                    ctx.report(
                        "R4",
                        Verdict::Error,
                        format!("lspcheck couldn't deliver request id {}: {error}", describe_odd_id(id)),
                    );
                    return;
                }
                break;
            }
        }
    }

    let unexpected = ctx.s.unmatched_ids(started);
    if !missing.is_empty() {
        let mut detail = format!("delivered request id(s) {} got no response with the exact id", missing.join(", "));
        if !unexpected.is_empty() {
            let ids = unexpected.iter().map(describe_odd_id).collect::<Vec<_>>().join(", ");
            detail.push_str(&format!("; response id(s) {ids} arrived meanwhile (likely id mangling)"));
        }
        if let Some(why) = stopped {
            detail.push_str(&format!("; {why}"));
        }
        if !undelivered.is_empty() {
            detail.push_str(&format!("; couldn't send the remaining id(s) {}", undelivered.join(", ")));
        }
        return ctx.fail_absent("R4", first_missing.expect("a request went unanswered"), detail);
    }
    if !undelivered.is_empty() {
        return ctx.report(
            "R4",
            Verdict::Error,
            format!("lspcheck couldn't deliver request id(s) {}", undelivered.join(", ")),
        );
    }
    ctx.pass_with("R4", format!("all {} requests got responses with their exact ids", ids.len()));
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
    for (i, id) in ids.iter().enumerate() {
        if let Err(e) = ctx.s.begin_request(id, deadline).await {
            ids[..i].iter().for_each(|id| ctx.s.abort_request(id));
            return ctx.report("J6", Verdict::Error, format!("lspcheck couldn't register a request: {e}"));
        }
    }
    // The requests written whole, and why the rest couldn't be.
    let mut delivered = ids.len();
    let mut send_failure = None;
    for (i, body) in bodies.iter().enumerate() {
        // The shared deadline, not a fresh write timeout: a write that can't
        // finish by then is an error like any other, and the time left to
        // wait for the replies mustn't shrink with every write.
        if let Err(e) = ctx.s.write_split(body, &[], Duration::ZERO, deadline).await {
            // The server may have seen part of the failed write, so those ids
            // are aborted; the earlier, whole ones stay outstanding.
            ids[i..].iter().for_each(|id| ctx.s.abort_request(id));
            delivered = i;
            send_failure = Some(SendFailure {
                why: if i == 0 {
                    format!("lspcheck couldn't send a request: {e}")
                } else {
                    format!("lspcheck couldn't send the rest: {e}")
                },
                stdin_closed: matches!(e, WriteError::Closed),
            });
            break;
        }
    }

    // The delivered requests are still owed a reply: one that goes
    // unanswered is a failure seen, whatever happened to the rest (a server
    // that exits on a string id may close its stdin before the next write).
    // That holds only when the server closed its stdin: a write that timed
    // out used up the deadline, so replies to the delivered requests may be
    // on their way, or read and not yet looked at (`j6_outcome` makes that
    // an error, so there's nothing to wait for).
    let mut missing = Vec::new();
    let mut closed = false;
    let judge_delivered = send_failure.as_ref().is_none_or(|f| f.stdin_closed);
    if send_failure.is_some() && judge_delivered {
        // Replies already read count even if the deadline has passed.
        ctx.s.take_in();
    }
    for id in ids[..delivered].iter().filter(|_| judge_delivered) {
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
    let unasked = ctx.s.unmatched_ids(started);
    match j6_outcome(delivered, &missing, closed, send_failure.as_ref(), &unasked) {
        J6Outcome::Pass => ctx.pass("J6"),
        J6Outcome::Error(why) => ctx.report("J6", Verdict::Error, why),
        J6Outcome::Absent(detail) => ctx.fail_absent("J6", started, detail),
    }
}

/// J6's verdict once the waits are over.
#[derive(Debug, PartialEq)]
enum J6Outcome {
    Pass,
    Error(String),
    /// Replies that didn't come: a failure, unless missed evidence could have
    /// held them (`fail_absent` decides).
    Absent(String),
}

/// Why some of J6's requests couldn't be written.
struct SendFailure {
    why: String,
    /// The server had closed its stdin (rather than the write timing out or
    /// failing otherwise).
    stdin_closed: bool,
}

/// J6's verdict from what was delivered and answered. A delivered request
/// that went unanswered is a failure seen, even when the rest couldn't be
/// sent because the server closed its stdin: a server that exits on a string
/// id may do that before lspcheck's next write, or after it, and the verdict
/// mustn't depend on which. A write that timed out or failed otherwise makes
/// J6 an error whatever the replies (the deadline was spent writing, so the
/// replies weren't waited for), as does a request lspcheck couldn't send when
/// every delivered one was answered.
fn j6_outcome(
    delivered: usize,
    missing: &[Id],
    closed: bool,
    send_failure: Option<&SendFailure>,
    unasked: &[Id],
) -> J6Outcome {
    if let Some(f) = send_failure
        && (missing.is_empty() || !f.stdin_closed)
    {
        let why = &f.why;
        return J6Outcome::Error(if delivered == 0 {
            why.clone()
        } else if !f.stdin_closed {
            format!("{why}; the replies to the {delivered} request(s) delivered before it weren't waited for")
        } else {
            format!("{why}; the {delivered} request(s) delivered before it were answered")
        });
    }
    if missing.is_empty() {
        return J6Outcome::Pass;
    }
    let list: Vec<String> = missing.iter().map(Id::to_string).collect();
    let mut detail = format!(
        "{} of {} concurrent requests delivered were not answered with their id: {}",
        missing.len(),
        delivered,
        list.join(", ")
    );
    if closed {
        detail.push_str(" (the server closed its stdout)");
    }
    if let Some(f) = send_failure {
        detail.push_str(&format!("; {}", f.why));
    }
    if !unasked.is_empty() {
        let list: Vec<String> = unasked.iter().map(Id::to_string).collect();
        detail.push_str(&format!("; meanwhile responses with unmatched ids arrived: {}", list.join(", ")));
    }
    J6Outcome::Absent(detail)
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
    let since = ctx.s.elapsed();
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
                ctx.fail_call("L1", since, describe_call_error(e, t), e);
                if let Some(r) = ctx.results.last_mut().filter(|r| r.absent_at.is_some()) {
                    r.absent_at = Some((since, waited));
                }
            }
        },
    }
    // Leave politely so the server isn't killed mid-thought.
    if init.is_ok() {
        let _ = ctx.s.call("shutdown", None, t).await;
        let _ = ctx.s.notify("exit", None).await;
        let _ = ctx.s.serve_until_exit(Instant::now() + ctx.cfg.exit_timeout).await;
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
    let since = ctx.s.elapsed();
    match ctx.s.call("initialize", Some(initialize_params(&ctx.root)), t).await {
        Ok(m) => match &m.kind {
            Kind::Response { outcome: Outcome::Error { code: Some(_), .. }, .. } => {
                if !answers {
                    ctx.pass_with(
                        "L3",
                        "whether it keeps serving wasn't checked: it didn't answer a plain request before either",
                    );
                } else {
                    let since = ctx.s.elapsed();
                    match ctx.s.call(PROBE, Some(json!({})), t).await {
                        Ok(_) => ctx.pass("L3"),
                        Err(e) => {
                            stopped = Some("the server stopped answering after the second initialize (see L3)");
                            let why = format!(
                                "after the error for the second initialize, a plain request went unanswered ({})",
                                describe_call_error(&e, t)
                            );
                            ctx.fail_call("L3", since, why, &e);
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
            ctx.fail_call(
                "L3",
                since,
                format!("a second initialize went unanswered ({})", describe_call_error(&e, t)),
                &e,
            );
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
    let since = ctx.s.elapsed();
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
            since,
            format!("a request after shutdown went unanswered ({})", describe_call_error(&e, t)),
            &e,
        ),
    }
    if ctx.s.notify("exit", None).await.is_ok() {
        let _ = ctx.s.serve_until_exit(Instant::now() + ctx.cfg.exit_timeout).await;
    }
}

/// The capabilities object from a successful initialize result.
fn initialize_capabilities(reply: &Message) -> Option<&Value> {
    let Kind::Response { outcome: Outcome::Result(result), .. } = &reply.kind else { return None };
    result.get("capabilities")
}

/// Whether the initialize result advertises `workspaceSymbolProvider`.
fn advertises_symbol(reply: &Message) -> bool {
    initialize_capabilities(reply).is_some_and(|caps| advertises(caps, "workspaceSymbolProvider"))
}

/// Tries one deliberately wrongly typed request for the first supported
/// capability, then checks that the server still answers a plain request.
async fn wrong_params(ctx: &mut Ctx<'_>) {
    const LOOKED_FOR: &str = "hoverProvider, definitionProvider, documentSymbolProvider and workspaceSymbolProvider";
    let reply = match ctx.initialize().await {
        Ok(reply) => reply,
        Err(InitFailure::Violation(why) | InitFailure::Absent(why) | InitFailure::Blocked(why)) => {
            return ctx.report("R2", Verdict::Error, format!("initialize didn't succeed: {why}"));
        }
    };
    let Some(caps) = initialize_capabilities(&reply) else {
        return ctx.report(
            "R2",
            Verdict::NotExercised,
            format!("initialize had no capabilities object; looked for {LOOKED_FOR}"),
        );
    };
    let Some((capability, method)) = [
        ("hoverProvider", "textDocument/hover"),
        ("definitionProvider", "textDocument/definition"),
        ("documentSymbolProvider", "textDocument/documentSymbol"),
        ("workspaceSymbolProvider", "workspace/symbol"),
    ]
    .into_iter()
    .find(|(capability, _)| advertises(caps, capability)) else {
        return ctx.report("R2", Verdict::NotExercised, format!("none of {LOOKED_FOR} was advertised"));
    };
    let params = match method {
        "textDocument/hover" | "textDocument/definition" => {
            json!({"textDocument": 42, "position": "start"})
        }
        "textDocument/documentSymbol" => json!({"textDocument": 42}),
        "workspace/symbol" => json!({"query": 42}),
        _ => unreachable!("only the advertised R2 methods are selected"),
    };
    let t = ctx.cfg.request_timeout;
    let sent = ctx.s.elapsed();
    let response = match ctx.s.call(method, Some(params), t).await {
        Ok(response) => response,
        Err(error) => {
            return ctx.fail_call(
                "R2",
                sent,
                format!("{method} with wrongly typed params: {}", describe_call_error(&error, t)),
                &error,
            );
        }
    };
    let error = match &response.kind {
        Kind::Response { outcome: Outcome::Error { code, .. }, .. } => match code {
            Some(codes::INVALID_PARAMS) => format!("InvalidParams ({})", codes::INVALID_PARAMS),
            Some(code) => format!("error {code}, not InvalidParams (which is preferred)"),
            None => "error without an integer code".into(),
        },
        Kind::Response { outcome: Outcome::Result(value), .. } => {
            return ctx.fail(
                "R2",
                format!(
                    "{method} returned a result for parameters of the wrong types: {}",
                    session::preview(value.to_string().as_bytes())
                ),
            );
        }
        Kind::Response { outcome: Outcome::Malformed(why), .. } => {
            return ctx.fail("R2", format!("{method} returned a malformed response instead of an error: {why}"));
        }
        _ => unreachable!("call waits for a response"),
    };

    match back_in_step_result(ctx, "the wrongly typed request").await {
        Ok(()) => {
            ctx.pass_with("R2", format!("{method} ({capability}) returned {error}; a plain request was answered"))
        }
        Err(StepFailure::Exited(why)) => ctx.fail("R2", format!("{method} returned {error}; {why}")),
        Err(StepFailure::Unanswered { detail, since }) => {
            ctx.fail_absent("R2", since, format!("{method} returned {error}; {detail}"));
        }
        Err(StepFailure::Unsent(why)) => ctx.report("R2", Verdict::Error, format!("{method} returned {error}; {why}")),
    }
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

    // C1. A missing reply is judged again when the session ends, in case a
    // frame then unfinished turns out to have swallowed it.
    let mut c1_absent = None;
    // A pass that assumed nothing was missed: rechecked at the end.
    let mut c1_again = None;
    let c1 = match (&answer, first) {
        (Err(wait), _) => {
            let e = CallError::Wait(*wait);
            let why = format!("{which}: no response after $/cancelRequest ({})", describe_call_error(&e, t));
            match ctx.s.incomplete_within(written_at, r_waited) {
                Some(missed) => (Verdict::Error, format!("inconclusive, because {missed}: {why}")),
                None => {
                    c1_absent = Some((written_at, r_waited));
                    (Verdict::Fail, why)
                }
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
                c1_again = Some(inconclusive(MISSED, &format!("a second response to {which}")));
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
    match c1_absent {
        Some((since, waited)) => ctx.fail_absent_between("C1", since, waited, c1.1),
        None => ctx.report("C1", c1.0, c1.1),
    }
    if let Some(detail) = c1_again {
        ctx.unless_missed(written_at, r_cancelled_at.unwrap_or(end), Verdict::Error, detail);
    }

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
            (CallError::Wait(_), None) => ctx.fail_absent_between("C2", asked, waited, why),
        }
    } else if let Some(why) = &incomplete_c2 {
        ctx.report("C2", Verdict::Error, inconclusive(why, "a reply to one of the cancels"));
    } else {
        let what = if first.is_some() { "an id never sent and a finished request" } else { "an id never sent" };
        ctx.pass_with("C2", format!("cancelling {what} drew no reply, and the server kept serving"));
        if let Some(from) = c2_started {
            ctx.unless_missed(from, end, Verdict::Error, inconclusive(MISSED, "a reply to one of the cancels"));
        }
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
        ctx.unless_missed(written_at, end, Verdict::Error, inconclusive(MISSED, "a reply to a $/cancelRequest"));
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
        let _ = ctx.s.serve_until_exit(Instant::now() + ctx.cfg.exit_timeout).await;
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
    // `word` is on line 0 of document A and line 2 of document B; find it.
    let word_line = lines.iter().position(|l| l.contains("word")).expect("the document has a word");
    let (word, _) = documents::span_in(text, word_line, "word", encoding);
    // `word` is ASCII, so two units in is inside it in every encoding.
    let inside = Position { line: word_line, character: word.character + 2 };
    let l0_end = Position { line: word_line, character: text::line_length(lines[word_line], encoding) };
    // `name` is on line 2 of version 41, line 3 of version 42, and line 4 of B.
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
) -> Result<(Asked, Result<Message, WaitError>), WriteError> {
    let deadline = Instant::now() + ctx.cfg.request_timeout;
    let id = ctx.s.fresh_id();
    let bytes = frame(&message::request(&id, method, Some(params.clone())));
    ctx.s.begin_request(&id, deadline).await?;
    let at = ctx.s.elapsed();
    if let Err(e) = ctx.s.write_split(&bytes, &[], Duration::ZERO, deadline).await {
        ctx.s.abort_request(&id);
        return Err(e);
    }
    let answer = ctx.s.wait_response(&id, deadline).await;
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
/// no evidence: the rest are not sent, and the note says so. Returns why
/// the server stopped answering, when a request went unanswered.
async fn ask_all(
    ctx: &mut Ctx<'_>,
    requests: Vec<(&'static str, Value)>,
    asked: &mut Vec<Asked>,
    notes: &mut Vec<String>,
    undelivered: &mut Vec<(&'static str, String)>,
) -> Option<String> {
    let methods: Vec<&'static str> = requests.iter().map(|(m, _)| *m).collect();
    for (i, (method, params)) in requests.into_iter().enumerate() {
        match ask(ctx, method, params).await {
            Ok((a, answer)) => {
                asked.push(a);
                if let Err(e) = answer {
                    let closed = matches!(e, WaitError::Closed);
                    let why = describe_call_error(&CallError::Wait(e), ctx.cfg.request_timeout);
                    notes.push(format!("{method} went unanswered ({why}), so no more requests were sent"));
                    // The server's output closed: neither this request nor the
                    // rest of the batch could be answered, as if undelivered.
                    if closed {
                        undelivered.extend(methods[i..].iter().map(|m| (*m, format!("{m} was cut off ({why})"))));
                    }
                    return Some(format!("{method} went unanswered ({why})"));
                }
            }
            Err(e) => {
                notes.push(format!("lspcheck couldn't send {method} ({e}), so no more requests were sent"));
                // This request and the rest of the batch never reached the
                // server: lspcheck couldn't deliver them.
                undelivered.extend(methods[i..].iter().map(|m| (*m, format!("lspcheck couldn't send {m} ({e})"))));
                break;
            }
        }
    }
    None
}

/// Whether a message is a `client/registerCapability` registering the sync
/// notifications D3 depends on: a registration whose `method` is
/// `textDocument/didChange` or `textDocument/didOpen`.
fn sync_registration(m: &Message) -> bool {
    m.method() == Some("client/registerCapability")
        && m.value.pointer("/params/registrations").and_then(Value::as_array).is_some_and(|regs| {
            regs.iter().any(|r| {
                matches!(
                    r.get("method").and_then(Value::as_str),
                    Some("textDocument/didChange") | Some("textDocument/didOpen")
                )
            })
        })
}

/// The version document A is changed to.
const DOC_A2_VERSION: i64 = 42;

/// Document B's version, sent in its `didOpen`.
const DOC_B_VERSION: i64 = 7;

/// Documents: lspcheck opens document A, asks the server's advertised
/// requests about it one at a time, and waits a while for diagnostics. Every
/// position the server sends about it must fall on a character boundary of
/// the text it refers to, in the negotiated encoding (D1), every `version`
/// its diagnostics carry for the document must be one lspcheck sent for it
/// before the notification was read (D2), and after the `didChange` lspcheck
/// sends, positions must refer to the changed text (D3). Finally it closes
/// the document and asks about documents that aren't open (D4).
///
/// A position or a version can arrive at any time (diagnostics especially),
/// so the D1 to D3 verdicts are given at the end, over everything read before
/// lspcheck sent `exit`.
async fn documents(ctx: &mut Ctx<'_>) {
    let params = documents_initialize_params(&ctx.root);
    let reply = match ctx.initialize_with(params).await {
        Ok(reply) => reply,
        Err(InitFailure::Violation(why) | InitFailure::Absent(why) | InitFailure::Blocked(why)) => {
            let detail = format!("initialize didn't succeed: {why}");
            for id in ["D1", "D2", "D3", "D4"] {
                ctx.report(id, Verdict::Error, detail.clone());
            }
            return;
        }
    };
    let Kind::Response { outcome: Outcome::Result(result), .. } = &reply.kind else { unreachable!() };
    let caps = result.get("capabilities").cloned().unwrap_or(Value::Null);
    if !opens_documents(&reply) {
        let detail = "the server doesn't ask for didOpen (textDocumentSync), so lspcheck opens no document";
        for id in ["D1", "D2", "D3", "D5", "D6"] {
            ctx.report(id, Verdict::Skip, detail);
        }
        // D4 still runs: doc.txt on disk, never opened.
        let path = ctx.root.join("doc.txt");
        let _ = std::fs::write(&path, documents::doc_a());
        ctx.s.record_workspace(ctx.root.clone(), workspace_files(&ctx.root));
        let uri = file_uri(&path);
        d4(ctx, &caps, false, &uri, None).await;
        leave(ctx).await;
        return;
    }
    let encoding = match negotiated_encoding(&reply) {
        Ok(e) => e,
        Err(why) => {
            for id in ["D1", "D2", "D3", "D5", "D6"] {
                ctx.report(id, Verdict::Error, why.clone());
            }
            // D4 doesn't need the encoding, and runs with the document
            // never opened, as when the server doesn't ask for didOpen.
            let path = ctx.root.join("doc.txt");
            let _ = std::fs::write(&path, documents::doc_a());
            ctx.s.record_workspace(ctx.root.clone(), workspace_files(&ctx.root));
            let uri = file_uri(&path);
            d4(ctx, &caps, false, &uri, None).await;
            leave(ctx).await;
            return;
        }
    };
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
        for id in ["D1", "D2", "D3", "D5", "D6"] {
            ctx.report(id, Verdict::Error, detail.clone());
        }
        leave(ctx).await;
        return;
    }
    let mut docs = vec![OpenDoc {
        uri: uri.clone(),
        versions: vec![Version { at: opened_at, number: DOC_A_VERSION, text: text.clone() }],
    }];

    let mut asked = Vec::new();
    let mut notes = Vec::new();
    // The document requests lspcheck couldn't deliver, or sent after the
    // server's output closed: no answer could come (D6 can't pass then).
    let mut undelivered: Vec<(&'static str, String)> = Vec::new();
    // Why the server stopped answering before D4, if it did.
    let mut stalled =
        ask_all(ctx, document_requests(&caps, &uri, &text, encoding), &mut asked, &mut notes, &mut undelivered).await;
    await_diagnostics(ctx, &uri, DOC_A_VERSION, from).await;

    // The change to version 42, the advertised way. A sync kind of None
    // sends no `didChange`, and D3 skips.
    let mut change: Option<Change> = None;
    let mut registered = false;
    // Set when the `didChange` couldn't be written: D3 is an error, and the
    // version-42 requests aren't sent, but D1 and D2 are still judged over
    // everything read, as on the normal path.
    let mut change_failed: Option<String> = None;
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
        match ctx.s.notify("textDocument/didChange", Some(params)).await {
            Ok(()) => {
                change = Some(Change {
                    at: change_at,
                    before: text.clone(),
                    after: after.clone(),
                    alternatives: documents::alternatives(&text, encoding, sync),
                });
                docs[0].versions.push(Version { at: change_at, number: DOC_A2_VERSION, text: after });
                // The same requests at version 42, with positions of the
                // changed text, one at a time as before.
                let stopped = ask_all(
                    ctx,
                    document_requests(&caps, &uri, &docs[0].versions[1].text, encoding),
                    &mut asked,
                    &mut notes,
                    &mut undelivered,
                )
                .await;
                // The latest requests say whether the server is answering
                // now: an earlier timeout it recovered from doesn't count.
                stalled = stopped;
                await_diagnostics(ctx, &uri, DOC_A2_VERSION, from).await;
            }
            Err(e) => {
                // The change didn't reach the server: D3 is an error, and the
                // version-42 requests aren't sent. D1 and D2 are still judged
                // over everything read, as on the normal path.
                change_failed = Some(format!("lspcheck couldn't send didChange: {e}"));
            }
        }
    }
    // D5: open document B, with its mix of line endings, after the version-42
    // requests and diagnostics wait, before D4's `didClose`. B is a second
    // opened document, so D1 and D2 judge its positions and publishes too; D3
    // concerns only A. B is closed before D4 runs, so D4's requests concern
    // only A and `missing.txt`, as before.
    let b_path = ctx.root.join("crlf.txt");
    let b_text = documents::doc_b();
    if let Err(e) = std::fs::write(&b_path, &b_text) {
        ctx.s.note(format!("couldn't write {}: {e}", b_path.display()));
    }
    ctx.s.record_workspace(ctx.root.clone(), workspace_files(&ctx.root));
    let b_uri = file_uri(&b_path);
    let b_from = ctx.s.messages.len();
    let b_opened_at = ctx.s.elapsed();
    let b_doc = json!({
        "uri": b_uri,
        "languageId": "plaintext",
        "version": DOC_B_VERSION,
        "text": b_text.clone(),
    });
    let mut b_opened = false;
    if let Err(e) = ctx.s.notify("textDocument/didOpen", Some(json!({"textDocument": b_doc}))).await {
        ctx.report("D5", Verdict::Error, format!("lspcheck couldn't send didOpen for the line-ending document: {e}"));
    } else {
        b_opened = true;
        docs.push(OpenDoc {
            uri: b_uri.clone(),
            versions: vec![Version { at: b_opened_at, number: DOC_B_VERSION, text: b_text.clone() }],
        });
        // The advertised requests about B, with positions on B2 and B4. A
        // stall here doesn't affect D4, which concerns only A.
        // B's requests are the latest before D4's, so they say whether the
        // server is answering when D4 starts (when any were asked).
        let asked_before = asked.len();
        let b_stalled =
            ask_all(ctx, document_requests(&caps, &b_uri, &b_text, encoding), &mut asked, &mut notes, &mut undelivered)
                .await;
        if asked.len() > asked_before {
            stalled = b_stalled;
        }
        await_diagnostics(ctx, &b_uri, DOC_B_VERSION, b_from).await;
    }
    // Close B before D4 runs, so D4's requests concern only A and missing.txt.
    // When lspcheck began closing B: from then on the server may answer
    // about it from disk, so nothing about B read later is judged. Set even
    // if the write fails partway.
    let b_closed_at = if b_opened {
        let began = ctx.s.elapsed();
        let _ = ctx.s.notify("textDocument/didClose", Some(json!({"textDocument": {"uri": b_uri}}))).await;
        Some(began)
    } else {
        None
    };
    // D4: the document is closed, so a request about it, one about a URI
    // that was never opened and doesn't exist, and a plain request, each
    // get a response, and the server keeps serving.
    let closed_at = d4(ctx, &caps, true, &uri, stalled.as_deref()).await;
    let end = leave(ctx).await;
    // D1 to D3 judge what was read while the document was open. Once
    // `didClose` is sent the server may lawfully read the file from disk,
    // whose text lspcheck doesn't track, so later positions aren't judged.
    let end = closed_at.map_or(end, |at| at.min(end));

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
    // What is about B and was read after B's close isn't judged, as for A.
    if let Some(at) = b_closed_at {
        documents::forget_after_close(&mut sources, &mut published, &b_uri, at);
    }
    let about_b = |s: &Source| s.found.iter().any(|f| documents::same_uri(&f.uri, &b_uri));
    let judged = documents::judge_positions(encoding, &docs, &sources);
    // Each verdict is judged by what lspcheck missed in its window, now and
    // again when the session ends (`report_judged`).
    let with_notes = |(verdict, detail): (Verdict, String)| match notes.is_empty() {
        true => (verdict, detail),
        false => (verdict, format!("{detail}; {}", notes.join("; "))),
    };
    ctx.report_judged("D1", opened_at, end, |why| with_notes(documents::judge_d1(encoding, &judged, why)));
    ctx.report_judged("D2", opened_at, end, |why| with_notes(documents::judge_d2(&docs, &published, why)));
    if let Some(detail) = change_failed {
        // The `didChange` didn't reach the server: D3 is an error. D1 and D2
        // were still judged over everything read, above.
        ctx.report("D3", Verdict::Error, detail);
    } else {
        match change {
            // The sync kind is None or absent: the client must not send
            // `didChange`, so there is no change to judge.
            None => ctx.report(
                "D3",
                Verdict::Skip,
                "the server doesn't take changes (textDocumentSync's change is 0 or absent)",
            ),
            // A dynamic registration of the sync notifications, read before
            // the change was written: the server may expect another sync
            // kind.
            Some(_) if registered => ctx.report(
                "D3",
                Verdict::Error,
                "the server registered textDocument/didChange or didOpen dynamically; lspcheck declares no dynamic registration, so the server may expect another sync kind",
            ),
            Some(change) => {
                // D1's judgements of the positions read before the change was
                // written, and of those that can only refer to the changed
                // text: a reply to a request sent after the change, or
                // diagnostics labelled with its version. A late reply to an
                // earlier request, or unlabelled diagnostics read after the
                // change, may refer to the text before it; a position invalid
                // there is D1's finding, not evidence about applying the
                // change.
                // Document A only: B is never changed.
                let after = documents::judge_positions_where(encoding, &docs[..1], &sources, |s| {
                    documents::only_after(&docs[0], s, change.at)
                });
                ctx.report_judged("D3", change.at, end, |why| {
                    with_notes(documents::judge_d3(encoding, &after, &change, why))
                });
            }
        }
    }
    // D5: judged over the positions read while B was open, before its
    // `didClose` if sent. B's text is never changed, so there is no
    // after-change judgement as in D3.
    if b_opened {
        let b_end = b_closed_at.map_or(end, |at| at.min(end));
        // Document B only, while it was open.
        let b_docs: Vec<OpenDoc> = docs.iter().filter(|d| documents::same_uri(&d.uri, &b_uri)).cloned().collect();
        let judged_b =
            documents::judge_positions_where(encoding, &b_docs, &sources, |s| about_b(s) && s.read_at < b_end);
        ctx.report_judged("D5", b_opened_at, b_end, |why| {
            with_notes(documents::judge_d5(encoding, &b_text, &judged_b, why))
        });
    }
    // D6: selectionRange replies about open documents, judged against the
    // request's own positions, in the text current when the request was
    // sent. The same windows D1 uses: before A's `didClose`, and for B
    // before B's. Skipped when the server advertises no
    // selectionRangeProvider, so no request is sent.
    if !advertises(&caps, "selectionRangeProvider") {
        ctx.report(
            "D6",
            Verdict::Skip,
            "the server advertises no selectionRangeProvider, so lspcheck sends no selectionRange request",
        );
    } else {
        let mut selection: Vec<documents::SelectionReply> = Vec::new();
        // A fresh borrow, so the earlier `msgs` borrow doesn't live across
        // the mutable borrows above.
        let d6_msgs = &ctx.s.messages;
        for a in asked.iter().filter(|a| a.method == "textDocument/selectionRange") {
            let req_uri = a.params.pointer("/textDocument/uri").and_then(Value::as_str);
            let Some(doc) = docs.iter().find(|d| req_uri.is_some_and(|u| documents::same_uri(&d.uri, u))) else {
                continue;
            };
            // The same window D1 uses for this document: before its close.
            let window_end = if b_opened && req_uri.is_some_and(|u| documents::same_uri(u, &b_uri)) {
                b_closed_at.map_or(end, |at| at.min(end))
            } else {
                end
            };
            // The version current when the request was sent, used only to
            // clamp.
            let Some(version) = doc.versions.iter().rfind(|v| v.at <= a.at) else { continue };
            let positions = documents::request_positions(&a.params);
            for (_at, m) in
                d6_msgs.iter().filter(|(at, m)| *at >= a.at && *at < window_end && m.response_id() == Some(&a.id))
            {
                let Kind::Response { outcome: Outcome::Result(result), .. } = &m.kind else { continue };
                if let Some(r) =
                    documents::judge_selection_reply(&version.text, encoding, version.number, &positions, result)
                {
                    selection.push(r);
                }
            }
        }
        // A selectionRange request lspcheck couldn't deliver leaves D6
        // without evidence it should have had: an error, unless a failure
        // was seen.
        let undelivered_selection =
            undelivered.iter().find(|(m, _)| *m == "textDocument/selectionRange").map(|(_, why)| why.clone());
        ctx.report_judged("D6", opened_at, end, |why| {
            with_notes(documents::judge_d6(&selection, why.or(undelivered_selection.as_deref())))
        });
    }
}

/// D4's request: its method, and how to build its params for a URI.
type D4Request = (&'static str, fn(&str) -> Value);

/// D4's request about a document: the first advertised of `documentSymbol`,
/// `hover` at (0,0), `foldingRange`, `documentHighlight` at (0,0) and
/// `selectionRange` at (0,0). (0,0) is a boundary in every encoding, so no
/// text model is needed.
fn d4_request(caps: &Value) -> Option<D4Request> {
    if advertises(caps, "documentSymbolProvider") {
        return Some(("textDocument/documentSymbol", |uri| json!({"textDocument": {"uri": uri}})));
    }
    if advertises(caps, "hoverProvider") {
        return Some((
            "textDocument/hover",
            |uri| json!({"textDocument": {"uri": uri}, "position": {"line": 0, "character": 0}}),
        ));
    }
    if advertises(caps, "foldingRangeProvider") {
        return Some(("textDocument/foldingRange", |uri| json!({"textDocument": {"uri": uri}})));
    }
    if advertises(caps, "documentHighlightProvider") {
        return Some((
            "textDocument/documentHighlight",
            |uri| json!({"textDocument": {"uri": uri}, "position": {"line": 0, "character": 0}}),
        ));
    }
    if advertises(caps, "selectionRangeProvider") {
        return Some((
            "textDocument/selectionRange",
            |uri| json!({"textDocument": {"uri": uri}, "positions": [{"line": 0, "character": 0}]}),
        ));
    }
    None
}

/// How a D4 response looks: the result (null, empty, or non-empty) or the
/// error code. The content is never judged.
fn describe_d4_outcome(m: &Message) -> String {
    let Kind::Response { outcome, .. } = &m.kind else { unreachable!("wait_response returns a response") };
    match outcome {
        Outcome::Result(Value::Null) => "result null".into(),
        Outcome::Result(Value::Array(a)) if a.is_empty() => "result (empty)".into(),
        Outcome::Result(Value::Object(o)) if o.is_empty() => "result (empty)".into(),
        Outcome::Result(_) => "result (non-empty)".into(),
        Outcome::Error { code: Some(c), .. } => format!("error {c}"),
        Outcome::Error { .. } => "an error without an integer code".into(),
        Outcome::Malformed(why) => format!("a malformed response that {why}"),
    }
}

/// D4: one advertised request about a document that isn't open (closed, or
/// never opened when `closed` is false), one about a URI that was never
/// opened and doesn't exist, and a plain request, each sent alone and
/// awaited by `--timeout`. Any result or error is lawful, including one read
/// from disk; what is judged is that a response comes, and that the server
/// keeps serving. The requests are not judged by D1 to D3: the document is
/// not open, so a reply may refer to the file on disk, which has no version
/// in lspcheck's model. `stalled` says why the server had already stopped
/// answering earlier in the scenario: then an unanswered D4 request says
/// nothing about documents that aren't open, so it is an error, not a
/// failure (a server that answers them anyway still passes).
async fn d4(ctx: &mut Ctx<'_>, caps: &Value, closed: bool, uri: &str, stalled: Option<&str>) -> Option<Duration> {
    let mut stalled = stalled;
    if !ctx.s.is_alive() {
        ctx.report("D4", Verdict::Error, "not judged: the server exited before the document was closed");
        return None;
    }
    let Some((method, params_for)) = d4_request(caps) else {
        ctx.report(
            "D4",
            Verdict::Skip,
            "the server advertises no request about a document (none of documentSymbolProvider, hoverProvider, \
             foldingRangeProvider, documentHighlightProvider, selectionRangeProvider)",
        );
        return None;
    };
    // When lspcheck began closing the document: from then on, the server may
    // answer from disk. Set even if the write fails partway.
    let closed_at = closed.then(|| ctx.s.elapsed());
    if closed && let Err(e) = ctx.s.notify("textDocument/didClose", Some(json!({"textDocument": {"uri": uri}}))).await {
        ctx.report("D4", Verdict::Error, format!("lspcheck couldn't send didClose: {e}"));
        return closed_at;
    }
    let t = ctx.cfg.request_timeout;
    let missing = file_uri(&ctx.root.join("missing.txt"));
    let closed_label = if closed { "the closed document" } else { "the document on disk (never opened)" };
    let mut outcomes = Vec::new();
    for (label, doc_uri) in
        [(closed_label, Some(uri)), ("the never-opened document", Some(missing.as_str())), ("the plain request", None)]
    {
        let (m, params) = match doc_uri {
            Some(u) => (method, params_for(u)),
            None => (PROBE, json!({})),
        };
        let since = ctx.s.elapsed();
        match ask(ctx, m, params).await {
            Ok((_, Ok(reply))) => {
                outcomes.push(format!("{label} got {}", describe_d4_outcome(&reply)));
                // It is answering again, whatever happened before.
                stalled = None;
            }
            Ok((_, Err(e))) => {
                let e = CallError::Wait(e);
                let what = format!("{label} went unanswered ({})", describe_call_error(&e, t));
                if let Some(why) = stalled {
                    ctx.report(
                        "D4",
                        Verdict::Error,
                        format!("not judged: {what}, but the server had already stopped answering ({why})"),
                    );
                    return closed_at;
                }
                ctx.fail_call("D4", since, what, &e);
                return closed_at;
            }
            Err(e) => {
                ctx.report("D4", Verdict::Error, format!("{label}: lspcheck couldn't send it ({e})"));
                return closed_at;
            }
        }
    }
    ctx.pass_with("D4", format!("all three answered: {}", outcomes.join("; ")));
    closed_at
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

/// A server still running after `exit`, though lspcheck answered its requests
/// while it waited. If lspcheck missed something the server sent, that may
/// have been a request the server is still waiting on. An answer lspcheck
/// couldn't write doesn't excuse it: a server waiting for an answer would be
/// reading its input.
fn still_running(ctx: &mut Ctx<'_>, id: &'static str) {
    let waited = ctx.cfg.exit_timeout.as_secs();
    let to = ctx.s.elapsed();
    ctx.fail(id, format!("still running {waited} s after exit"));
    let r = ctx.results.last_mut().expect("just reported");
    r.unless_missed = Some(Recheck {
        from: Duration::ZERO,
        to,
        output_only: true,
        verdict: Verdict::Error,
        detail: format!(
            "inconclusive: still running {waited} s after exit, but {MISSED}, so it may be waiting on lspcheck"
        ),
    });
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
    match ctx.s.serve_until_exit(Instant::now() + ctx.cfg.exit_timeout).await {
        Some(status) if status.code() == Some(1) => ctx.pass("L7"),
        Some(status) => ctx.fail("L7", format!("exited with {}", session::describe_status(status))),
        None => still_running(ctx, "L7"),
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
    /// A response that should have come didn't: why, and when lspcheck began
    /// sending what should have been answered.
    Missing(String, Duration),
    /// lspcheck couldn't deliver it.
    Unsent(String),
}

fn frame(value: &Value) -> Vec<u8> {
    transport::encode(&serde_json::to_vec(value).expect("JSON values serialize"))
}

/// Writes `bytes`, which hold the requests `ids`, as separate writes cut at
/// `cuts`, and waits for every response, all by one deadline.
async fn exchange(ctx: &mut Ctx<'_>, ids: &[Id], bytes: &[u8], cuts: &[usize]) -> Probe {
    let since = ctx.s.elapsed();
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
    Probe::Missing(detail, since)
}

enum StepFailure {
    Exited(String),
    Unanswered { detail: String, since: Duration },
    Unsent(String),
}

/// Whether the server still answers a plain request after a variant that
/// didn't pass; if not, why the rest of the scenario can't be judged.
async fn back_in_step_result(ctx: &mut Ctx<'_>, after: &str) -> Result<(), StepFailure> {
    if !ctx.s.is_alive() {
        return Err(StepFailure::Exited(format!("the server exited after {after}")));
    }
    let t = ctx.cfg.request_timeout;
    let since = ctx.s.elapsed();
    let result = ctx.s.call(PROBE, Some(json!({})), t).await;
    // A closed pipe usually means the server is exiting; give that a moment
    // to be seen, so the reason given is the real one.
    if matches!(result, Err(CallError::Write(WriteError::Closed) | CallError::Wait(WaitError::Closed)))
        && let Some(status) = ctx.s.wait_exit(Instant::now() + ctx.cfg.exit_timeout).await
    {
        return Err(StepFailure::Exited(format!(
            "the server exited after {after} ({})",
            session::describe_status(status)
        )));
    }
    match result {
        Ok(_) => Ok(()),
        Err(e @ CallError::Wait(_)) => Err(StepFailure::Unanswered {
            detail: format!(
                "after {after}, a plain request went unanswered ({}), so the input may be out of step",
                describe_call_error(&e, t)
            ),
            since,
        }),
        Err(e) => Err(StepFailure::Unsent(format!("after {after}, lspcheck couldn't send a plain request ({e})"))),
    }
}

async fn back_in_step(ctx: &mut Ctx<'_>, after: &str) -> Option<String> {
    match back_in_step_result(ctx, after).await {
        Ok(()) => None,
        Err(StepFailure::Exited(why) | StepFailure::Unsent(why)) => Some(why),
        Err(StepFailure::Unanswered { detail, .. }) => Some(detail),
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
        Probe::Missing(why, since) => ctx.fail_absent(id, since, format!("{what}: {why}")),
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
        let waited = ctx.s.elapsed();
        for id in CHECKS {
            ctx.report(id, verdict, why.clone());
            // Not exercised because the reply never came: unless lspcheck
            // finds at the end that it missed something that could have held it.
            ctx.unless_missed(Duration::ZERO, waited, Verdict::Error, format!("inconclusive, because {MISSED}: {why}"));
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
        let _ = ctx.s.serve_until_exit(Instant::now() + ctx.cfg.exit_timeout).await;
    }
}

/// Whether the server asks for didOpen and didClose: a sync kind other than
/// None, or `openClose: true`. Without it the client mustn't send them. A
/// numeric `textDocumentSync` is read as an integer however it is spelled.
fn opens_documents(reply: &Message) -> bool {
    let Kind::Response { outcome: Outcome::Result(result), .. } = &reply.kind else { return false };
    match result.pointer("/capabilities/textDocumentSync") {
        Some(Value::Number(kind)) => crate::message::integer_value(kind).is_some_and(|k| k > 0),
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

/// The first time the server reused the id of one of its own requests while
/// that request was still unanswered: before lspcheck began writing its
/// answer, or when lspcheck never answered it in full. For a completed answer,
/// the server may have read it any time after writing began, before reusing
/// the id, which JSON-RPC allows.
fn reused_server_id(requests: &[ServerRequest]) -> Option<(&ServerRequest, &ServerRequest)> {
    let mut by_id: HashMap<&Id, (Option<&ServerRequest>, Option<&ServerRequest>)> = HashMap::new();
    for later in requests {
        let (unanswered, latest) = by_id.entry(&later.id).or_default();
        if let Some(earlier) = unanswered.or(latest.filter(|r| r.answer_began.is_none_or(|began| later.at < began))) {
            return Some((earlier, later));
        }
        if later.answered_at.is_none() {
            *unanswered = Some(later);
        }
        if latest.is_none_or(|r| r.answer_began < later.answer_began) {
            *latest = Some(later);
        }
    }
    None
}

/// How the server went on after lspcheck answered one of its requests:
/// `Some(true)` when it answered a request lspcheck sent after the answer,
/// `Some(false)` when lspcheck sent requests after it and none was answered,
/// `None` when lspcheck sent none.
fn served_after(o: &Observations, answered_at: Duration) -> Option<bool> {
    let mut later = o.sent.iter().filter(|(sent, _)| *sent >= answered_at).peekable();
    later.peek()?;
    Some(later.any(|(_, id)| o.answered.iter().any(|(_, a)| a == id)))
}

fn judge_j7(runs: &[ScenarioRun]) -> CheckResult {
    let failures = observed(runs)
        .filter_map(|(name, o)| {
            let (earlier, later) = reused_server_id(&o.server_requests)?;
            let when = match earlier.answered_at {
                Some(_) => "before lspcheck had begun answering it",
                None => "which lspcheck never answered in full",
            };
            Some(format!(
                "in {name} at {} ms: the server's {} request reused id {} of its {} request read at {} ms, {when}",
                later.at.as_millis(),
                later.method,
                later.id,
                earlier.method,
                earlier.at.as_millis(),
            ))
        })
        .collect();
    let r = cross("J7", runs, failures, "", true);
    if r.verdict != Verdict::Pass {
        return r;
    }
    let total: usize = observed(runs).map(|(_, o)| o.server_requests.len()).sum();
    if total == 0 {
        return result("all", "J7", Verdict::NotExercised, "the server sent lspcheck no requests");
    }
    // The server going quiet after an answer can't be pinned on the answer:
    // something else lspcheck sent may have stopped it. Only an answer the
    // server is seen to have gone on from counts, as a pass.
    let (mut served, mut silent) = (0, None);
    for (name, o) in observed(runs) {
        for r in &o.server_requests {
            match r.answered_at.and_then(|at| served_after(o, at)) {
                Some(true) => served += 1,
                Some(false) => {
                    silent.get_or_insert((name, r));
                }
                None => {}
            }
        }
    }
    match (served, silent) {
        (0, Some((name, r))) => result(
            "all",
            "J7",
            Verdict::Error,
            format!(
                "inconclusive: in {name}, after lspcheck answered the server's {} request (read at {} ms), the \
                 server answered none of lspcheck's later requests (see the checks that sent them), and no other \
                 answer was followed by one it answered",
                r.method,
                r.at.as_millis()
            ),
        ),
        (0, None) => result(
            "all",
            "J7",
            Verdict::NotExercised,
            format!(
                "the server sent {total} request(s), but lspcheck sent no request of its own after answering one, \
                 so whether the server keeps serving after an answer wasn't seen"
            ),
        ),
        (served, _) => result(
            "all",
            "J7",
            Verdict::Pass,
            format!(
                "{total} server request(s), no unanswered id reused; the server answered lspcheck's later requests \
                 after {served} of lspcheck's answers"
            ),
        ),
    }
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
    fn j6_fails_on_a_delivered_request_left_unanswered_even_if_the_rest_couldnt_be_sent() {
        let lost = Id::Str("123456".into());
        let failure = |why: &str, stdin_closed| SendFailure { why: why.into(), stdin_closed };
        // The server exited on the first string id: that request was
        // delivered whole and never answered, and the last couldn't be
        // written. A failure, as when the last write landed first.
        let why = "lspcheck couldn't send the rest: the server's stdin is closed";
        let exited = failure(why, true);
        let J6Outcome::Absent(detail) = j6_outcome(3, std::slice::from_ref(&lost), true, Some(&exited), &[]) else {
            panic!("expected an absence");
        };
        assert!(detail.starts_with("1 of 3 concurrent requests delivered were not answered"), "{detail}");
        assert!(detail.contains(why), "{detail}");
        // Every delivered request answered, but one couldn't be sent: no
        // evidence about it, so an error.
        let J6Outcome::Error(detail) = j6_outcome(3, &[], false, Some(&exited), &[]) else {
            panic!("expected an error");
        };
        assert!(detail.ends_with("the 3 request(s) delivered before it were answered"), "{detail}");
        // Nothing delivered: the plain error.
        let first = "lspcheck couldn't send a request: timed out";
        assert_eq!(j6_outcome(0, &[], false, Some(&failure(first, false)), &[]), J6Outcome::Error(first.into()));
        // All four delivered and answered.
        assert_eq!(j6_outcome(4, &[], false, None, &[]), J6Outcome::Pass);
    }

    #[test]
    fn j6_is_an_error_when_a_write_times_out_after_some_were_delivered() {
        // The server stopped reading after three requests; the fourth write
        // used up the deadline, so the three replies weren't waited for (they
        // may have been read and not yet looked at). Not a failure, whatever
        // was missing when the waiting stopped.
        let timed_out = SendFailure { why: "lspcheck couldn't send the rest: timed out".into(), stdin_closed: false };
        let lost = [Id::Str("1".into()), Id::Str("lspcheck-2".into())];
        for missing in [&[][..], &lost[..]] {
            let J6Outcome::Error(detail) = j6_outcome(3, missing, false, Some(&timed_out), &[]) else {
                panic!("expected an error with {missing:?} missing");
            };
            assert!(
                detail.ends_with("the replies to the 3 request(s) delivered before it weren't waited for"),
                "{detail}"
            );
        }
    }

    #[test]
    fn file_uris_encode_reserved_bytes() {
        assert_eq!(file_uri(Path::new("/tmp/a b/é")), "file:///tmp/a%20b/%C3%A9");
        assert_eq!(file_uri(Path::new("C:\\x y")), "file:///C:/x%20y");
    }

    #[test]
    fn odd_id_details_truncate_long_strings() {
        let detail = describe_odd_id(&Id::Str("x".repeat(1000)));
        assert_eq!(detail, format!("\"{}…\" (1000 characters)", "x".repeat(20)));
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
                sent: Vec::new(),
                server_requests: Vec::new(),
            }),
            transcript: None,
        }
    }

    fn server_request(at: u64, id: &str, began: Option<u64>) -> ServerRequest {
        ServerRequest {
            at: Duration::from_millis(at),
            id: Id::Str(id.into()),
            method: "window/workDoneProgress/create".into(),
            answer_began: began.map(Duration::from_millis),
            answered_at: began.map(|b| Duration::from_millis(b + 1)),
        }
    }

    /// A run in which the server sent `requests`, and lspcheck sent request
    /// "a" at 50 ms, answered when `answered`.
    fn j7_run(requests: Vec<ServerRequest>, answered: bool) -> [ScenarioRun; 1] {
        let mut run = observed_run("lifecycle", vec![], None);
        let o = run.observed.as_mut().unwrap();
        o.server_requests = requests;
        o.sent = vec![(Duration::from_millis(50), Id::Str("a".into()))];
        if answered {
            o.answered = vec![(Duration::from_millis(60), Id::Str("a".into()))];
        }
        [run]
    }

    #[test]
    fn j7_fails_on_reuse_before_an_answer_begins_or_after_an_incomplete_answer() {
        // Read before the first answer began: the server couldn't have had it.
        let r = judge_j7(&j7_run(vec![server_request(10, "1", Some(20)), server_request(15, "1", Some(25))], true));
        assert_eq!(r.verdict, Verdict::Fail, "{}", r.detail);
        // Never answered: still outstanding.
        let r = judge_j7(&j7_run(vec![server_request(10, "1", None), server_request(30, "1", None)], true));
        assert_eq!(r.verdict, Verdict::Fail, "{}", r.detail);
        assert!(r.detail.contains("never answered"), "{}", r.detail);
        let mut incomplete = server_request(10, "1", Some(20));
        incomplete.answered_at = None;
        let r = judge_j7(&j7_run(vec![incomplete, server_request(30, "1", Some(35))], true));
        assert_eq!(r.verdict, Verdict::Fail, "{}", r.detail);
        assert!(r.detail.contains("never answered in full"), "{}", r.detail);
        // Read after the answer began: the server may have read it first.
        let r = judge_j7(&j7_run(vec![server_request(10, "1", Some(20)), server_request(20, "1", Some(25))], true));
        assert_eq!(r.verdict, Verdict::Pass, "{}", r.detail);
        // Different ids at the same time are fine.
        let r = judge_j7(&j7_run(vec![server_request(10, "1", Some(20)), server_request(10, "2", Some(25))], true));
        assert_eq!(r.verdict, Verdict::Pass, "{}", r.detail);
    }

    #[test]
    fn j7_reuse_matches_exact_id_values_in_read_order() {
        let mut requests = vec![
            server_request(10, "1", Some(20)),
            server_request(30, "1", Some(40)),
            server_request(35, "2", None),
            server_request(36, "1", Some(50)),
            server_request(37, "2", None),
        ];
        let (earlier, later) = reused_server_id(&requests).unwrap();
        assert!(std::ptr::eq(earlier, &requests[1]));
        assert!(std::ptr::eq(later, &requests[3]));

        requests[0].id = Id::int(1);
        requests[1].id = Id::Str("1".into());
        assert!(reused_server_id(&requests[..2]).is_none());
        requests[0].answered_at = None;
        requests[1].id = Id::from_value(&serde_json::from_str::<Value>("1.0").unwrap());
        let (earlier, later) = reused_server_id(&requests[..2]).unwrap();
        assert!(std::ptr::eq(earlier, &requests[0]));
        assert!(std::ptr::eq(later, &requests[1]));
    }

    #[test]
    fn j7_handles_a_large_burst_of_distinct_request_ids() {
        let mut requests: Vec<_> = (0..100_000).map(|i| server_request(i, &i.to_string(), None)).collect();
        assert!(reused_server_id(&requests).is_none());
        requests.push(server_request(100_000, "99999", None));
        let (earlier, later) = reused_server_id(&requests).unwrap();
        assert!(std::ptr::eq(earlier, &requests[99_999]));
        assert!(std::ptr::eq(later, &requests[100_000]));
    }

    #[test]
    fn j7_passes_only_on_service_seen_after_an_answer() {
        assert_eq!(judge_j7(&j7_run(vec![], true)).verdict, Verdict::NotExercised);
        // Answered after lspcheck's request was sent: nothing seen since.
        let r = judge_j7(&j7_run(vec![server_request(55, "1", Some(56))], true));
        assert_eq!(r.verdict, Verdict::NotExercised, "{}", r.detail);
        // lspcheck's later request went unanswered: that may not be the answer's doing.
        let r = judge_j7(&j7_run(vec![server_request(10, "1", Some(20))], false));
        assert_eq!(r.verdict, Verdict::Error, "{}", r.detail);
        // Missed output may have held a reused id.
        let mut runs = j7_run(vec![server_request(10, "1", Some(20))], true);
        runs[0].observed.as_mut().unwrap().limit = Some((Duration::from_millis(5), "skipped a frame".into()));
        assert_eq!(judge_j7(&runs).verdict, Verdict::Error);
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
