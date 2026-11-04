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

use crate::message::{self, Id, Kind, Message, Outcome, codes};
use crate::process::{Coverage, ProcInfo, ServerCommand};
use crate::session::{self, CallError, Finish, Options, Problem, Session, WaitError, WriteError};
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
    check!(
        "J4",
        JsonRpc,
        Must,
        "An unknown request gets MethodNotFound (-32601), including a $/ request",
        "dollarRequests"
    ),
    check!("L1", Lifecycle, Should, "A request before initialize gets ServerNotInitialized (-32002)", "initialize"),
    check!("L2", Lifecycle, Must, "initialize returns capabilities and an offered position encoding", "initialize"),
    check!("L4", Lifecycle, Must, "shutdown returns null and the server keeps running until exit", "shutdown"),
    check!("L6", Lifecycle, Should, "exit after shutdown ends the process with status 0", "exit"),
    check!("L7", Lifecycle, Should, "exit without shutdown ends the process with status 1", "exit"),
    check!("L9", Lifecycle, Policy, "No descendant process outlives the server", "exit"),
];

/// Checks judged from every scenario's observations.
pub const CROSS_CUTTING: &[&str] = &["F1", "F2", "F8", "L9"];

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
    Scenario { name: "pre-initialize", checks: &["L1"] },
    Scenario { name: "exit-without-shutdown", checks: &["L7"] },
    Scenario { name: "framing", checks: &["F4", "F6", "F7", "F3", "F5"] },
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
    let cross: Vec<CheckResult> =
        [judge_f1(&runs), judge_f2(&runs), judge_f8(&runs), judge_l9(&runs)].into_iter().collect();
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
    let mut ctx = Ctx { s: session, cfg, root, results: Vec::new(), scenario: name };
    match name {
        "lifecycle" => lifecycle(&mut ctx).await,
        "pre-initialize" => pre_initialize(&mut ctx).await,
        "exit-without-shutdown" => exit_without_shutdown(&mut ctx).await,
        "framing" => framing(&mut ctx).await,
        _ => unreachable!("unknown scenario {name}"),
    }
    let Ctx { s, mut results, .. } = ctx;
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
    Ok(ScenarioRun { name, results, observed: Some(observations(finish)), transcript })
}

fn observations(f: Finish) -> Observations {
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
        let t = self.cfg.startup_timeout;
        let reply = match self.s.call("initialize", Some(initialize_params(&self.root)), t).await {
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

/// A request before initialize.
async fn pre_initialize(ctx: &mut Ctx<'_>) {
    let t = ctx.cfg.request_timeout;
    match ctx.s.call("workspace/symbol", Some(json!({"query": ""})), t).await {
        Ok(m) => match &m.kind {
            Kind::Response { outcome: Outcome::Error { code: Some(codes::SERVER_NOT_INITIALIZED), .. }, .. } => {
                ctx.pass("L1")
            }
            Kind::Response { outcome, .. } => {
                ctx.fail("L1", format!("got {} instead of ServerNotInitialized", describe_outcome(outcome)))
            }
            _ => unreachable!(),
        },
        Err(e) => ctx.fail_call("L1", describe_call_error(&e, t), &e),
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
