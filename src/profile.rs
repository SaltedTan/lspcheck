// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! Profiles: an optional TOML file giving lspcheck what a particular server
//! needs to start and do useful work (a command, project files, settings),
//! and what its maintainers accept (descendants that outlive it, checks to
//! skip).
//!
//! Everything is read and validated once, before any server starts, so a
//! bad profile is a usage error rather than a run full of errors. Paths in
//! the profile are relative to the directory of the profile file; the
//! document K1 asks about is named relative to the workspace.

#![cfg_attr(
    windows,
    allow(
        clippy::permissions_set_readonly_false,
        reason = "Windows read-only attributes are not Unix permission bits"
    )
)]

use crate::capabilities::{self, Spot};
use crate::checks;
use crate::mutation::{self, Lexicon, Operator, StringDelimiters};
use serde::Deserialize;
use serde::de::{self, Deserializer, Visitor};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A profile, read and validated.
#[derive(Debug, Clone, Default)]
pub struct Profile {
    /// `[server].command`, its program resolved against the profile's
    /// directory when it is a path with a directory part.
    pub command: Option<Vec<OsString>>,
    pub startup_timeout: Option<Duration>,
    pub request_timeout: Option<Duration>,
    pub exit_timeout: Option<Duration>,
    pub workspace: Workspace,
    /// `[client].initialization_options`, as JSON.
    pub initialization_options: Option<Value>,
    /// `[client].configuration`, as a JSON object.
    pub configuration: Option<Value>,
    /// `[allow].descendants`: process names L9 accepts after exit.
    pub allowed_descendants: Vec<String>,
    /// `[allow].skip`.
    pub skip: Vec<Skip>,
    /// `[document]`: what K1 asks about instead of a document of lspcheck's own.
    pub document: Option<Document>,
    /// `[mutation]`, inert without `--mutate`.
    pub mutation: Option<Mutation>,
}

/// The document K1 opens and asks about: a file the workspace provides.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    /// Its name in the workspace, with `/` separators.
    pub path: String,
    pub language_id: String,
    /// Where position requests are asked; identifier-like tokens when absent.
    pub positions: Option<Vec<Spot>>,
}

/// What a profile puts in each scenario's workspace.
#[derive(Debug, Clone, Default)]
pub struct Workspace {
    /// The directory copied in first (`[workspace].path`), made absolute.
    pub fixture: Option<PathBuf>,
    /// Files written after it (`[workspace].files`): relative names with `/`
    /// separators, and their text.
    pub files: Vec<(String, String)>,
    /// Every file the profile puts in the workspace, fixture and inline,
    /// named as in `files`, so transcripts record them.
    pub names: Vec<String>,
    pub directories: Vec<String>,
}

/// A check the profile skips, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Skip {
    pub id: &'static str,
    pub reason: String,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Raw {
    #[serde(default)]
    server: RawServer,
    #[serde(default)]
    workspace: RawWorkspace,
    #[serde(default)]
    client: RawClient,
    #[serde(default)]
    allow: RawAllow,
    document: Option<RawDocument>,
    mutation: Option<RawMutation>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDocument {
    path: Option<String>,
    language_id: Option<String>,
    positions: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawServer {
    command: Option<Vec<String>>,
    #[serde(default, deserialize_with = "startup_timeout")]
    startup_timeout: Option<Duration>,
    #[serde(default, deserialize_with = "request_timeout")]
    request_timeout: Option<Duration>,
    #[serde(default, deserialize_with = "exit_timeout")]
    exit_timeout: Option<Duration>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawWorkspace {
    path: Option<String>,
    files: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawClient {
    initialization_options: Option<toml::Value>,
    configuration: Option<toml::Table>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawAllow {
    descendants: Option<Vec<String>>,
    skip: Option<Vec<RawSkip>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSkip {
    id: String,
    reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMutation {
    files: Option<Vec<String>>,
    language_id: Option<String>,
    seed: Option<i64>,
    cases: Option<i64>,
    steps: Option<i64>,
    operators: Option<Vec<String>>,
    fragments: Option<Vec<String>>,
    requests: Option<Vec<String>>,
    requests_per_step: Option<i64>,
    max_document_kib: Option<i64>,
    #[serde(default, deserialize_with = "run_timeout")]
    run_timeout: Option<Duration>,
    lexicon: Option<RawLexicon>,
    recovery: Option<RawRecovery>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLexicon {
    line_comments: Option<Vec<String>>,
    block_comments: Option<Vec<(String, String)>>,
    strings: Option<Vec<RawStringDelimiters>>,
    identifier_chars: Option<String>,
    brackets: Option<Vec<String>>,
    keywords: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStringDelimiters {
    open: String,
    close: String,
    escape: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRecovery {
    probes: Option<Vec<String>>,
    #[serde(default, deserialize_with = "settle_timeout")]
    settle_timeout: Option<Duration>,
    #[serde(default, deserialize_with = "recovery_timeout")]
    recovery_timeout: Option<Duration>,
    expect: Option<String>,
    no_errors: Option<bool>,
}

/// Most cases a run may ask for.
pub const MAX_CASES: u64 = 1000;
/// Cases when neither the profile nor `--mutation-cases` gives a number.
pub const DEFAULT_CASES: u64 = 20;
/// Most requests sent after one step.
pub const MAX_REQUESTS_PER_STEP: usize = 16;
/// The largest `max_document_kib`.
pub const MAX_DOCUMENT_KIB: usize = 1024;
/// The longest `run_timeout`.
pub const MAX_RUN_TIMEOUT: Duration = Duration::from_secs(4 * 3600);
/// The longest `settle_timeout`.
pub const MAX_SETTLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// What a case's budget adds to the deadlines it counts.
const CASE_MARGIN: Duration = Duration::from_secs(5);

/// The mutation scenario's settings: `[mutation]`, or for `--mutate` without
/// one, defaults for `[document]`'s file ([design](../docs/source-mutations.md#profile-schema)).
#[derive(Debug, Clone, PartialEq)]
pub struct Mutation {
    /// The documents mutated, in the order given. Empty when there is no
    /// seed file, which makes M1 to M3 `SKIP`.
    pub seeds: Vec<SeedFile>,
    /// The seed of every case's generators; chosen at random when `None`.
    pub seed: Option<u64>,
    pub cases: u64,
    /// What the planner may do: steps, operators, fragments, the document
    /// size limit and the lexicon.
    pub planner: mutation::Config,
    /// The methods a step may ask, in K1's order; all K1's when `None`.
    pub requests: Option<Vec<&'static str>>,
    pub requests_per_step: usize,
    pub run_timeout: Duration,
    pub recovery: Recovery,
}

/// A document the mutation scenario opens and mutates.
#[derive(Debug, Clone, PartialEq)]
pub struct SeedFile {
    /// Its name in the workspace, with `/` separators.
    pub path: String,
    pub language_id: String,
    /// Its clean text.
    pub text: String,
}

/// `[mutation.recovery]`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Recovery {
    /// The probes M3 may use; every advertised one when `None`.
    pub probes: Option<Vec<Probe>>,
    /// The startup timeout when `None`.
    pub settle_timeout: Option<Duration>,
    /// Twice the request timeout when `None`.
    pub recovery_timeout: Option<Duration>,
    pub expect: Expect,
    pub no_errors: bool,
}

/// A recovery probe: a request whose answer depends only on a document's
/// text, or the diagnostics the server publishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    DocumentSymbol,
    FoldingRange,
    Diagnostic,
    PublishDiagnostics,
}

impl Probe {
    pub const ALL: [Probe; 4] = [Self::DocumentSymbol, Self::FoldingRange, Self::Diagnostic, Self::PublishDiagnostics];

    /// The probe's name in profiles: its method.
    pub fn name(self) -> &'static str {
        match self {
            Self::DocumentSymbol => "textDocument/documentSymbol",
            Self::FoldingRange => "textDocument/foldingRange",
            Self::Diagnostic => "textDocument/diagnostic",
            Self::PublishDiagnostics => "publishDiagnostics",
        }
    }

    /// Whether lspcheck asks for it (and so sends a terminal probe), rather
    /// than reading what the server publishes.
    pub fn is_request(self) -> bool {
        self != Self::PublishDiagnostics
    }
}

/// What the profile states about the server's answers after a restoration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Expect {
    /// Nothing: a recovery not seen is `ERROR`.
    #[default]
    Observe,
    /// The answers return to their baseline within `recovery_timeout`.
    Baseline,
}

impl Mutation {
    /// The defaults for these seed files.
    fn defaults(seeds: Vec<SeedFile>) -> Self {
        Self {
            seeds,
            seed: None,
            cases: DEFAULT_CASES,
            planner: mutation::Config::default(),
            requests: None,
            requests_per_step: 4,
            run_timeout: Duration::from_secs(3600),
            recovery: Recovery::default(),
        }
    }

    pub fn settle_timeout(&self, startup_timeout: Duration) -> Duration {
        self.recovery.settle_timeout.unwrap_or(startup_timeout)
    }

    pub fn recovery_timeout(&self, request_timeout: Duration) -> Duration {
        self.recovery.recovery_timeout.unwrap_or(request_timeout.saturating_mul(2))
    }

    /// The most time one case may take: every write and reply of its steps
    /// and of the restoration's steps, each with its own deadline, the
    /// restoration's writes, `recovery_timeout`, and each terminal probe's
    /// write and reply ([Bounds](../docs/source-mutations.md#bounds)).
    pub fn case_budget(&self, request_timeout: Duration) -> Duration {
        let documents = self.seeds.len().max(1) as u32;
        let requested_probes = match &self.recovery.probes {
            Some(probes) => probes.iter().filter(|p| p.is_request()).count(),
            None => Probe::ALL.iter().filter(|p| p.is_request()).count(),
        } as u32;
        let steps = self.planner.steps as u32;
        let per_step = 1 + self.requests_per_step as u32;
        request_timeout
            .saturating_mul((steps + 1) * per_step)
            .saturating_add(request_timeout.saturating_mul(documents))
            .saturating_add(self.recovery_timeout(request_timeout))
            .saturating_add(request_timeout.saturating_mul(documents * requested_probes * 2))
            .saturating_add(CASE_MARGIN)
    }
}

/// A seed for a run that names none: random, below 2^53 so that it survives
/// any JSON reader and fits a TOML integer.
pub fn random_seed() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::hash::RandomState::new().build_hasher();
    hasher
        .write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
    hasher.write_u32(std::process::id());
    hasher.finish() >> 11
}

/// What `--mutate` runs: the profile's `[mutation]`, else defaults for its
/// `[document]`, else nothing to mutate; with `--mutation-seed` and
/// `--mutation-cases` applied, and the case budget checked against the run's.
pub fn mutation_settings(
    profile: &Profile,
    seed: Option<u64>,
    cases: Option<u64>,
    request_timeout: Duration,
) -> Result<Mutation, String> {
    let mut settings = match (&profile.mutation, &profile.document) {
        (Some(section), _) => section.clone(),
        (None, Some(document)) => {
            let text = workspace_text(&profile.workspace, &document.path)?;
            let limit = mutation::Config::default().max_document_bytes;
            if text.len() > limit {
                return Err(format!(
                    "--mutate: document.path {:?} is {} bytes, more than the {} KiB a document may hold without \
                     [mutation].max_document_kib",
                    document.path,
                    text.len(),
                    limit / 1024
                ));
            }
            Mutation::defaults(vec![SeedFile {
                path: document.path.clone(),
                language_id: document.language_id.clone(),
                text,
            }])
        }
        (None, None) => Mutation::defaults(Vec::new()),
    };
    if let Some(seed) = seed {
        settings.seed = Some(seed);
    }
    if let Some(cases) = cases {
        if !(1..=MAX_CASES).contains(&cases) {
            return Err(format!("--mutation-cases must be from 1 to {MAX_CASES}, not {cases}"));
        }
        settings.cases = cases;
    }
    let budget = settings.case_budget(request_timeout);
    if !settings.seeds.is_empty() && budget > settings.run_timeout {
        return Err(format!(
            "--mutate: a case may take up to {} with a request timeout of {}, more than the run's \
             mutation.run_timeout of {}, so no case could start; raise run_timeout (at most 4h), or lower \
             mutation.steps, mutation.requests_per_step or the request timeout",
            seconds(budget),
            seconds(request_timeout),
            seconds(settings.run_timeout)
        ));
    }
    Ok(settings)
}

fn seconds(d: Duration) -> String {
    if d.subsec_millis() == 0 { format!("{} s", d.as_secs()) } else { format!("{:.3} s", d.as_secs_f64()) }
}

/// A whole number from `lo` to `hi` for `key`, or `default` when absent.
fn whole(key: &str, value: Option<i64>, lo: i64, hi: i64, default: i64) -> Result<u64, String> {
    match value {
        None => Ok(default as u64),
        Some(n) if (lo..=hi).contains(&n) => Ok(n as u64),
        Some(n) => Err(format!("{key} must be from {lo} to {hi}, not {n}")),
    }
}

/// Refuses a list naming something twice.
fn distinct<T: PartialEq>(key: &str, items: &[T], name: impl Fn(&T) -> String) -> Result<(), String> {
    for (i, item) in items.iter().enumerate() {
        if items[..i].contains(item) {
            return Err(format!("{key}: {} is listed twice", name(item)));
        }
    }
    Ok(())
}

/// Refuses a lexicon string that is empty or holds a line end.
fn delimiter(key: &str, s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err(format!("{key}: a delimiter is empty"));
    }
    if s.contains(['\n', '\r']) {
        return Err(format!("{key}: {s:?} holds a line end, which is a token of its own"));
    }
    Ok(())
}

/// Validates `[mutation]`.
fn mutation_section(raw: RawMutation, workspace: &Workspace, document: Option<&Document>) -> Result<Mutation, String> {
    let max_document_kib = whole("mutation.max_document_kib", raw.max_document_kib, 1, MAX_DOCUMENT_KIB as i64, 64)?;
    let max_document_bytes = max_document_kib as usize * 1024;
    let language_id = match (raw.language_id, document) {
        (Some(id), _) if id.trim().is_empty() => return Err("mutation.language_id is empty".into()),
        (Some(id), _) => Some(id),
        (None, Some(document)) => Some(document.language_id.clone()),
        (None, None) => None,
    };
    let files = match (raw.files, document) {
        (Some(files), _) => {
            if files.is_empty() {
                return Err("mutation.files is empty; leave it out to mutate [document]'s file".into());
            }
            distinct("mutation.files", &files, |f| format!("{f:?}"))?;
            files
        }
        (None, Some(document)) => vec![document.path.clone()],
        (None, None) => {
            return Err("mutation.files is required without a [document]: name the seed files to mutate".into());
        }
    };
    let Some(language_id) = language_id else {
        return Err("mutation.language_id is required without a [document], such as \"lua\"".into());
    };
    let mut seeds = Vec::new();
    for path in files {
        safe_name(&path).map_err(|e| format!("mutation.files: {e}"))?;
        if !workspace.names.contains(&path) {
            return Err(format!(
                "mutation.files: {path:?} isn't a file the profile's workspace provides ([workspace].path or [workspace].files)"
            ));
        }
        let text = workspace_text(workspace, &path).map_err(|e| e.replacen("document.path", "mutation.files", 1))?;
        if text.len() > max_document_bytes {
            return Err(format!(
                "mutation.files: {path:?} is {} bytes, more than mutation.max_document_kib ({max_document_kib} KiB)",
                text.len()
            ));
        }
        seeds.push(SeedFile { path, language_id: language_id.clone(), text });
    }
    let seed = match raw.seed {
        None => None,
        Some(n) if n >= 0 => Some(n as u64),
        Some(n) => return Err(format!("mutation.seed must be a whole number from 0, not {n}")),
    };
    let cases = whole("mutation.cases", raw.cases, 1, MAX_CASES as i64, DEFAULT_CASES as i64)?;
    let steps = whole("mutation.steps", raw.steps, 1, mutation::MAX_STEPS as i64, mutation::MAX_STEPS as i64)?;
    let operators = match raw.operators {
        None => Operator::defaults(),
        Some(names) => {
            if names.is_empty() {
                return Err("mutation.operators is empty; leave it out for the default operators".into());
            }
            let mut operators = Vec::new();
            for name in &names {
                let Some(operator) = Operator::from_name(name) else {
                    let known: Vec<&str> = Operator::ALL.iter().map(|o| o.name()).collect();
                    return Err(format!(
                        "mutation.operators: {name:?} isn't an operator; they are {}",
                        known.join(", ")
                    ));
                };
                operators.push(operator);
            }
            distinct("mutation.operators", &operators, |o| o.name().to_string())?;
            operators
        }
    };
    let fragments = raw.fragments.unwrap_or_default();
    for fragment in &fragments {
        if fragment.is_empty() {
            return Err("mutation.fragments: a fragment is empty".into());
        }
        if fragment.len() > mutation::INSERT_CAP {
            return Err(format!(
                "mutation.fragments: a fragment of {} bytes is longer than an edit may grow a document ({} bytes)",
                fragment.len(),
                mutation::INSERT_CAP
            ));
        }
    }
    let requests = match raw.requests {
        None => None,
        Some(given) => {
            if given.is_empty() {
                return Err("mutation.requests is empty; leave it out to ask every method K1 asks".into());
            }
            let methods = capabilities::methods();
            let mut requests = Vec::new();
            for name in &given {
                let Some(method) = methods.iter().find(|m| *m == name) else {
                    return Err(format!(
                        "mutation.requests: {name:?} isn't a request K1 asks; they are {}",
                        methods.join(", ")
                    ));
                };
                requests.push(*method);
            }
            distinct("mutation.requests", &requests, |m| m.to_string())?;
            // K1's order, so that the round robin doesn't depend on the profile's.
            Some(methods.into_iter().filter(|m| requests.contains(m)).collect())
        }
    };
    let requests_per_step =
        whole("mutation.requests_per_step", raw.requests_per_step, 1, MAX_REQUESTS_PER_STEP as i64, 4)? as usize;
    let run_timeout = raw.run_timeout.unwrap_or(Duration::from_secs(3600));
    if run_timeout > MAX_RUN_TIMEOUT {
        return Err(format!("mutation.run_timeout is at most \"4h\", not {} s", run_timeout.as_secs()));
    }
    let lexicon = match raw.lexicon {
        None => Lexicon::default(),
        Some(raw) => lexicon(raw)?,
    };
    let recovery = match raw.recovery {
        None => Recovery::default(),
        Some(raw) => recovery(raw)?,
    };
    Ok(Mutation {
        seeds,
        seed,
        cases,
        planner: mutation::Config { steps: steps as usize, operators, fragments, max_document_bytes, lexicon },
        requests,
        requests_per_step,
        run_timeout,
        recovery,
    })
}

/// Validates `[mutation.lexicon]`. A key left out keeps the default
/// lexicon's value.
fn lexicon(raw: RawLexicon) -> Result<Lexicon, String> {
    let mut lexicon = Lexicon::default();
    if let Some(comments) = raw.line_comments {
        for c in &comments {
            delimiter("mutation.lexicon.line_comments", c)?;
        }
        lexicon.line_comments = comments;
    }
    if let Some(comments) = raw.block_comments {
        for (open, close) in &comments {
            delimiter("mutation.lexicon.block_comments", open)?;
            delimiter("mutation.lexicon.block_comments", close)?;
        }
        lexicon.block_comments = comments;
    }
    if let Some(strings) = raw.strings {
        lexicon.strings = Vec::new();
        for s in strings {
            delimiter("mutation.lexicon.strings", &s.open)?;
            delimiter("mutation.lexicon.strings", &s.close)?;
            let escape = match s.escape {
                None => None,
                Some(e) => {
                    let mut chars = e.chars();
                    match (chars.next(), chars.next()) {
                        (Some(c), None) if c != '\n' && c != '\r' => Some(c),
                        _ => {
                            return Err(format!(
                                "mutation.lexicon.strings: escape {e:?} must be one character, not a line end"
                            ));
                        }
                    }
                }
            };
            lexicon.strings.push(StringDelimiters { open: s.open, close: s.close, escape });
        }
    }
    if let Some(chars) = raw.identifier_chars {
        if chars.contains(['\n', '\r']) {
            return Err("mutation.lexicon.identifier_chars: a line end can't be part of an identifier".into());
        }
        lexicon.identifier_chars = chars;
    }
    if let Some(brackets) = raw.brackets {
        lexicon.brackets = Vec::new();
        for pair in &brackets {
            let chars: Vec<char> = pair.chars().collect();
            let [open, close] = chars[..] else {
                return Err(format!(
                    "mutation.lexicon.brackets: {pair:?} must be two characters, the opening and closing bracket"
                ));
            };
            if [open, close].iter().any(|c| matches!(c, '\n' | '\r')) {
                return Err(format!("mutation.lexicon.brackets: {pair:?} holds a line end"));
            }
            lexicon.brackets.push((open, close));
        }
    }
    if let Some(keywords) = raw.keywords {
        for k in &keywords {
            delimiter("mutation.lexicon.keywords", k)?;
        }
        lexicon.keywords = keywords;
    }
    Ok(lexicon)
}

/// Validates `[mutation.recovery]`.
fn recovery(raw: RawRecovery) -> Result<Recovery, String> {
    let probes = match raw.probes {
        None => None,
        Some(names) => {
            if names.is_empty() {
                return Err("mutation.recovery.probes is empty; leave it out for every advertised probe".into());
            }
            let mut probes = Vec::new();
            for name in &names {
                let Some(probe) = Probe::ALL.into_iter().find(|p| p.name() == name) else {
                    let known: Vec<&str> = Probe::ALL.iter().map(|p| p.name()).collect();
                    return Err(format!(
                        "mutation.recovery.probes: {name:?} isn't a recovery probe; they are {}",
                        known.join(", ")
                    ));
                };
                probes.push(probe);
            }
            distinct("mutation.recovery.probes", &probes, |p| p.name().to_string())?;
            Some(probes)
        }
    };
    if let Some(settle) = raw.settle_timeout
        && settle > MAX_SETTLE_TIMEOUT
    {
        return Err(format!("mutation.recovery.settle_timeout is at most \"5m\", not {} s", settle.as_secs()));
    }
    let expect = match raw.expect.as_deref() {
        None | Some("observe") => Expect::Observe,
        Some("baseline") => Expect::Baseline,
        Some(other) => {
            return Err(format!("mutation.recovery.expect must be \"observe\" or \"baseline\", not {other:?}"));
        }
    };
    Ok(Recovery {
        probes,
        settle_timeout: raw.settle_timeout,
        recovery_timeout: raw.recovery_timeout,
        expect,
        no_errors: raw.no_errors.unwrap_or(false),
    })
}

fn startup_timeout<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
    duration(d, "server.startup_timeout")
}

fn request_timeout<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
    duration(d, "server.request_timeout")
}

fn exit_timeout<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
    duration(d, "server.exit_timeout")
}

fn run_timeout<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
    duration(d, "mutation.run_timeout")
}

fn settle_timeout<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
    duration(d, "mutation.recovery.settle_timeout")
}

fn recovery_timeout<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
    duration(d, "mutation.recovery.recovery_timeout")
}

/// A duration string, with the key it belongs to named in any error.
fn duration<'de, D: Deserializer<'de>>(d: D, key: &'static str) -> Result<Option<Duration>, D::Error> {
    struct Expect(&'static str);
    impl Visitor<'_> for Expect {
        type Value = Duration;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "{} as a string \"<n>ms\", \"<n>s\", \"<n>m\" or \"<n>h\", such as \"30s\"", self.0)
        }
        fn visit_str<E: de::Error>(self, s: &str) -> Result<Duration, E> {
            parse_duration(s).ok_or_else(|| {
                E::custom(format!(
                    "{} must be \"<n>ms\", \"<n>s\", \"<n>m\" or \"<n>h\" with a whole number n, such as \"30s\", not {s:?}",
                    self.0
                ))
            })
        }
    }
    d.deserialize_str(Expect(key)).map(Some)
}

/// `"<n>ms"`, `"<n>s"`, `"<n>m"` or `"<n>h"`, n a non-negative integer.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let number = |n: &str| (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())).then(|| n.parse::<u64>().ok())?;
    if let Some(n) = s.strip_suffix("ms") {
        return number(n).map(Duration::from_millis);
    }
    if let Some(n) = s.strip_suffix('s') {
        return number(n).map(Duration::from_secs);
    }
    if let Some(n) = s.strip_suffix('m') {
        return number(n)?.checked_mul(60).map(Duration::from_secs);
    }
    if let Some(n) = s.strip_suffix('h') {
        return number(n)?.checked_mul(3600).map(Duration::from_secs);
    }
    None
}

/// Reads and validates the profile at `path`.
pub fn load(path: &Path) -> Result<Profile, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("can't read profile {}: {e}", path.display()))?;
    let dir = path.parent().unwrap_or(Path::new(""));
    parse(&text, dir).map_err(|e| format!("profile {}: {e}", path.display()))
}

/// Parses and validates a profile's text; relative paths in it are relative
/// to `dir`.
pub fn parse(text: &str, dir: &Path) -> Result<Profile, String> {
    let raw: Raw = toml::from_str(text).map_err(|e| e.to_string().trim_end().to_string())?;
    let mut profile = Profile {
        startup_timeout: raw.server.startup_timeout,
        request_timeout: raw.server.request_timeout,
        exit_timeout: raw.server.exit_timeout,
        ..Profile::default()
    };
    if let Some(command) = raw.server.command {
        let Some(program) = command.first() else {
            return Err("server.command must name a program: it is empty".into());
        };
        // A path with a directory part is relative to the profile, like the
        // profile's other paths; a bare name is looked up on PATH.
        let program = Path::new(program);
        let program = if program.components().count() > 1 && program.is_relative() {
            dir.join(program).into_os_string()
        } else {
            program.as_os_str().to_owned()
        };
        profile.command = Some(std::iter::once(program).chain(command[1..].iter().map(OsString::from)).collect());
    }
    profile.workspace = workspace(raw.workspace, dir)?;
    if let Some(options) = raw.client.initialization_options {
        profile.initialization_options = Some(to_json(&options, "client.initialization_options")?);
    }
    if let Some(settings) = raw.client.configuration {
        profile.configuration = Some(to_json(&toml::Value::Table(settings), "client.configuration")?);
    }
    profile.allowed_descendants = raw.allow.descendants.unwrap_or_default();
    for skip in raw.allow.skip.unwrap_or_default() {
        let Some(check) = checks::CATALOGUE.iter().find(|c| c.id == skip.id) else {
            return Err(format!("allow.skip: {:?} is not a check; `lspcheck list` shows them", skip.id));
        };
        if skip.reason.trim().is_empty() {
            return Err(format!("allow.skip: the entry for {} needs a reason", check.id));
        }
        if profile.skip.iter().any(|s| s.id == check.id) {
            return Err(format!("allow.skip: {} is listed twice", check.id));
        }
        profile.skip.push(Skip { id: check.id, reason: skip.reason });
    }
    if let Some(raw) = raw.document {
        profile.document = Some(document(raw, &profile.workspace)?);
    }
    if let Some(raw) = raw.mutation {
        profile.mutation = Some(mutation_section(raw, &profile.workspace, profile.document.as_ref())?);
    }
    Ok(profile)
}

/// Validates `[document]`: `path` names a UTF-8 text file the workspace
/// provides, `language_id` isn't empty, and each position is in the file.
fn document(raw: RawDocument, workspace: &Workspace) -> Result<Document, String> {
    let Some(path) = raw.path else {
        return Err("document.path is required: name a file in the profile's workspace".into());
    };
    safe_name(&path).map_err(|e| format!("document.path: {e}"))?;
    if !workspace.names.contains(&path) {
        return Err(format!(
            "document.path: {path:?} isn't a file the profile's workspace provides ([workspace].path or [workspace].files)"
        ));
    }
    let text = workspace_text(workspace, &path)?;
    let language_id = match raw.language_id {
        None => return Err("document.language_id is required with document.path, such as \"go\"".into()),
        Some(id) if id.trim().is_empty() => return Err("document.language_id is empty".into()),
        Some(id) => id,
    };
    let positions = match raw.positions {
        None => None,
        Some(given) => {
            if given.is_empty() {
                return Err("document.positions is empty; leave it out to use identifier-like tokens".into());
            }
            if given.len() > capabilities::MAX_POSITIONS {
                return Err(format!(
                    "document.positions has {} positions; at most {} are used",
                    given.len(),
                    capabilities::MAX_POSITIONS
                ));
            }
            let mut spots = Vec::new();
            for s in &given {
                let Some(spot) = Spot::parse(s) else {
                    return Err(format!(
                        "document.positions: {s:?} isn't LINE:COLUMN, two whole numbers from 1, such as \"3:6\""
                    ));
                };
                spot.check(&text).map_err(|e| format!("document.positions: in {path}, {e}"))?;
                spots.push(spot);
            }
            Some(spots)
        }
    };
    Ok(Document { path, language_id, positions })
}

/// The text of a file the workspace provides: an inline file's, else the
/// fixture's, which must be UTF-8.
fn workspace_text(workspace: &Workspace, name: &str) -> Result<String, String> {
    if let Some((_, text)) = workspace.files.iter().find(|(n, _)| n == name) {
        return Ok(text.clone());
    }
    let fixture = workspace.fixture.as_ref().expect("a file not inline is in the fixture");
    let path: PathBuf = std::iter::once(fixture.as_os_str()).chain(name.split('/').map(std::ffi::OsStr::new)).collect();
    let bytes = std::fs::read(&path).map_err(|e| format!("document.path: can't read {}: {e}", path.display()))?;
    String::from_utf8(bytes).map_err(|_| format!("document.path: {name:?} isn't UTF-8 text"))
}

/// A TOML value as JSON. A date or time becomes its TOML spelling; a float
/// JSON can't hold (NaN or infinite) is an error naming `key`.
fn to_json(v: &toml::Value, key: &str) -> Result<Value, String> {
    Ok(match v {
        toml::Value::String(s) => Value::String(s.clone()),
        toml::Value::Integer(n) => Value::from(*n),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(Value::Number)
            .ok_or_else(|| format!("{key}: {f} can't be sent as JSON"))?,
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Datetime(d) => Value::String(d.to_string()),
        toml::Value::Array(items) => {
            Value::Array(items.iter().map(|item| to_json(item, key)).collect::<Result<_, _>>()?)
        }
        toml::Value::Table(table) => Value::Object(
            table
                .iter()
                .map(|(k, item)| Ok((k.clone(), to_json(item, &format!("{key}.{k}"))?)))
                .collect::<Result<_, String>>()?,
        ),
    })
}

/// Validates `[workspace]`: the fixture is a directory holding only regular
/// files and directories, inline names are safe relative paths, and nothing
/// occupies a path reserved for lspcheck or creates a file/directory conflict.
fn workspace(raw: RawWorkspace, dir: &Path) -> Result<Workspace, String> {
    let mut fixture_files = BTreeSet::new();
    let mut fixture_dirs = BTreeSet::new();
    let fixture = match raw.path {
        Some(path) => {
            let full = dir.join(&path);
            let full = std::path::absolute(&full).unwrap_or(full);
            let full: PathBuf = full.components().collect();
            real_directory(&full).map_err(|e| format!("workspace.path: can't use {}: {e}", full.display()))?;
            walk(&full, "", &mut fixture_files, &mut fixture_dirs)?;
            Some(full)
        }
        None => None,
    };
    let inline = raw.files.unwrap_or_default();
    validate_name_case(fixture_files.iter().chain(&fixture_dirs).chain(inline.keys()).map(String::as_str))
        .map_err(|e| format!("workspace: {e}"))?;
    let mut files = Vec::new();
    for (name, text) in inline {
        let parts = safe_name(&name).map_err(|e| format!("workspace.files: {e}"))?;
        if fixture_dirs.contains(&name) {
            return Err(format!("workspace.files: {name:?} is a directory in workspace.path"));
        }
        for i in 1..parts.len() {
            let parent = parts[..i].join("/");
            if fixture_files.contains(&parent) {
                return Err(format!("workspace.files: {name:?} needs a directory where workspace.path has a file"));
            }
            if files.iter().any(|(other, _): &(String, String)| *other == parent) {
                return Err(format!("workspace.files: {name:?} needs a directory where {parent:?} is a file"));
            }
        }
        files.push((name, text));
    }
    // What lspcheck writes into the workspace, or names there as never
    // existing, mustn't come from the profile, nor be a directory of it.
    let mut taken: Vec<(&str, &str)> = Vec::new();
    for name in fixture_files.iter().chain(&fixture_dirs) {
        taken.push((name, "workspace.path"));
    }
    for (name, _) in &files {
        let mut at = 0;
        while let Some(i) = name[at..].find('/') {
            taken.push((&name[..at + i], "workspace.files"));
            at += i + 1;
        }
        taken.push((name, "workspace.files"));
    }
    for (name, key) in taken {
        if checks::OWN_FILES.iter().any(|own| own.eq_ignore_ascii_case(name)) {
            return Err(format!(
                "{key}: {name:?} is a name lspcheck uses in the workspace itself; {} are reserved at its top",
                checks::OWN_FILES.join(", ")
            ));
        }
    }
    let mut names = fixture_files;
    names.extend(files.iter().map(|(name, _)| name.clone()));
    Ok(Workspace {
        fixture,
        files,
        names: names.into_iter().collect(),
        directories: fixture_dirs.into_iter().collect(),
    })
}

pub(crate) fn validate_name_case<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<(), String> {
    let mut spellings: BTreeMap<String, (&str, &str)> = BTreeMap::new();
    for name in names {
        for end in name.match_indices('/').map(|(index, _)| index).chain(std::iter::once(name.len())) {
            let prefix = &name[..end];
            let key = prefix.to_ascii_lowercase();
            if let Some((previous, other)) = spellings.get(&key) {
                if *previous != prefix {
                    return Err(format!(
                        "{other:?} and {name:?} use inconsistent ASCII case for {previous:?} and {prefix:?}"
                    ));
                }
            } else {
                spellings.insert(key, (prefix, name));
            }
        }
    }
    Ok(())
}

/// The parts of a workspace name: a relative path with `/`
/// separators, without empty, `.` or `..` parts.
pub(crate) fn safe_name(name: &str) -> Result<Vec<&str>, String> {
    let bad = |why: &str| Err(format!("{name:?} {why}"));
    if name.is_empty() {
        return bad("is empty");
    }
    if name.contains('\\') {
        return bad("contains a backslash; separate directories with /");
    }
    if name.starts_with('/') {
        return bad("is absolute; names are relative to the workspace");
    }
    let parts: Vec<&str> = name.split('/').collect();
    if parts.iter().any(|p| p.is_empty() || *p == "." || *p == "..") {
        return bad("has an empty, . or .. part");
    }
    for part in &parts {
        if part.chars().any(|c| c.is_control() || ":<>\"|?*".contains(c)) {
            return bad("contains a character forbidden in portable file names");
        }
        if part.ends_with(['.', ' ']) {
            return bad("has a part ending with a dot or space");
        }
        let stem = part.split('.').next().unwrap_or_default().to_ascii_uppercase();
        if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && matches!(stem.as_bytes()[3], b'1'..=b'9'))
        {
            return bad("contains a Windows reserved device name");
        }
    }
    Ok(parts)
}

/// Lists a fixture's files and directories, named relative to its root,
/// refusing anything else: a symbolic link isn't followed, so nothing
/// outside the fixture is copied.
fn walk(dir: &Path, prefix: &str, files: &mut BTreeSet<String>, dirs: &mut BTreeSet<String>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("workspace.path: can't read {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("workspace.path: can't read {}: {e}", dir.display()))?;
        let path = entry.path();
        let Some(file_name) = entry.file_name().to_str().map(str::to_string) else {
            return Err(format!("workspace.path: {} has a name that isn't UTF-8", path.display()));
        };
        let name = format!("{prefix}{file_name}");
        safe_name(&name).map_err(|e| format!("workspace.path: {e}"))?;
        let kind = entry.file_type().map_err(|e| format!("workspace.path: can't inspect {}: {e}", path.display()))?;
        if kind.is_symlink() {
            return Err(format!(
                "workspace.path: {} is a symbolic link; lspcheck copies only regular files and directories",
                path.display()
            ));
        } else if kind.is_dir() {
            walk(&path, &format!("{name}/"), files, dirs)?;
            dirs.insert(name);
        } else if kind.is_file() {
            files.insert(name);
        } else {
            return Err(format!("workspace.path: {} is neither a regular file nor a directory", path.display()));
        }
    }
    Ok(())
}

/// Puts a profile's files into a fresh workspace at `root`: the fixture
/// copied, then the inline files written over it.
pub fn populate(root: &Path, w: &Workspace) -> std::io::Result<()> {
    if let Some(fixture) = &w.fixture {
        copy_dir(fixture, root)?;
    }
    for (name, text) in &w.files {
        let path: PathBuf =
            std::iter::once(root.as_os_str()).chain(name.split('/').map(std::ffi::OsStr::new)).collect();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        #[cfg(windows)]
        if path.try_exists()? {
            let mut permissions = std::fs::metadata(&path)?.permissions();
            permissions.set_readonly(false);
            std::fs::set_permissions(&path, permissions)?;
        }
        if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(e);
        }
        std::fs::write(&path, text)?;
    }
    Ok(())
}

fn real_directory(path: &Path) -> std::io::Result<()> {
    let normalized: PathBuf = path.components().collect();
    let meta = std::fs::symlink_metadata(&normalized)?;
    if meta.file_type().is_symlink() {
        return Err(std::io::Error::other(format!("{} is a symbolic link", path.display())));
    }
    if !meta.is_dir() {
        return Err(std::io::Error::other(format!("{} is not a directory", path.display())));
    }
    Ok(())
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    real_directory(from)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let (source, target) = (entry.path(), to.join(entry.file_name()));
        // Checked again, without following links: the fixture may have
        // changed since the profile was validated.
        let kind = std::fs::symlink_metadata(&source)?.file_type();
        if kind.is_dir() {
            std::fs::create_dir(&target)?;
            copy_dir(&source, &target)?;
        } else if kind.is_file() {
            std::fs::copy(&source, &target)?;
        } else {
            return Err(std::io::Error::other(format!(
                "{} is no longer a regular file or directory",
                source.display()
            )));
        }
    }
    Ok(())
}

/// A timeout: the command-line flag's seconds when given, else the
/// profile's, else the default.
pub fn timeout(flag: Option<u64>, profile: Option<Duration>, default_secs: u64) -> Duration {
    flag.map(Duration::from_secs).or(profile).unwrap_or(Duration::from_secs(default_secs))
}

/// The server's argv: what follows `--` when given, else `[server].command`.
pub fn server_argv(after_dashes: &[OsString], profile: Option<&Profile>) -> Option<Vec<OsString>> {
    if !after_dashes.is_empty() {
        return Some(after_dashes.to_vec());
    }
    profile.and_then(|p| p.command.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_here(text: &str) -> Result<Profile, String> {
        parse(text, Path::new("/profiles"))
    }

    #[test]
    fn an_empty_profile_is_valid_and_changes_nothing() {
        let p = parse_here("").unwrap();
        assert!(p.command.is_none() && p.startup_timeout.is_none() && p.configuration.is_none());
        assert!(p.workspace.fixture.is_none() && p.workspace.files.is_empty() && p.skip.is_empty());
    }

    #[test]
    fn durations_are_whole_numbers_of_ms_s_or_m() {
        assert_eq!(parse_duration("0s"), Some(Duration::ZERO));
        assert_eq!(parse_duration("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("2m"), Some(Duration::from_secs(120)));
        assert_eq!(parse_duration("4h"), Some(Duration::from_secs(4 * 3600)));
        for bad in ["", "30", "s", "-1s", "1.5s", " 1s", "1 s", "1d", "1sec", "+3s", "99999999999999999999s", "h"] {
            assert_eq!(parse_duration(bad), None, "{bad:?}");
        }
        let p = parse_here("[server]\nstartup_timeout = \"45s\"\nrequest_timeout = \"500ms\"\nexit_timeout = \"1m\"\n")
            .unwrap();
        assert_eq!(p.startup_timeout, Some(Duration::from_secs(45)));
        assert_eq!(p.request_timeout, Some(Duration::from_millis(500)));
        assert_eq!(p.exit_timeout, Some(Duration::from_secs(60)));
    }

    #[test]
    fn a_bad_duration_names_its_key() {
        for value in ["30", "\"30\"", "\"1d\"", "\"-5s\"", "true"] {
            let e = parse_here(&format!("[server]\nrequest_timeout = {value}\n")).unwrap_err();
            assert!(e.contains("server.request_timeout"), "{value}: {e}");
            assert!(e.contains("line 2"), "{value}: {e}");
        }
    }

    #[test]
    fn unknown_sections_and_keys_are_errors_naming_them() {
        let e = parse_here("[servr]\ncommand = [\"x\"]\n").unwrap_err();
        assert!(e.contains("servr") && e.contains("line 1"), "{e}");
        let e = parse_here("[server]\ncomand = [\"x\"]\n").unwrap_err();
        assert!(e.contains("comand"), "{e}");
        let e = parse_here("[[allow.skip]]\nid = \"L7\"\nreason = \"r\"\nwhy = \"x\"\n").unwrap_err();
        assert!(e.contains("why"), "{e}");
        let e = parse_here("[document]\npath = \"a\"\nlanguage = \"go\"\n").unwrap_err();
        assert!(e.contains("language") && e.contains("line 3"), "{e}");
        // Client capabilities aren't accepted yet.
        assert!(parse_here("[client]\ncapabilities = \"default\"\n").unwrap_err().contains("capabilities"));
    }

    #[test]
    fn wrong_types_are_errors_showing_the_key() {
        for (text, key) in [
            ("[server]\ncommand = \"gopls\"\n", "command"),
            ("[workspace]\nfiles = { \"go.mod\" = 3 }\n", "go.mod"),
            ("[client]\nconfiguration = [1]\n", "configuration"),
            ("[allow]\ndescendants = \"helper\"\n", "descendants"),
            ("server = 1\n", "server"),
        ] {
            let e = parse_here(text).unwrap_err();
            assert!(e.contains(key) && e.contains("line"), "{text}: {e}");
        }
    }

    #[test]
    fn unsafe_file_names_are_refused() {
        for name in ["", "/etc/passwd", "C:/x", "a/../b", "../x", "./x", "a/./b", "a//b", "a/", "a\\b"] {
            let text = format!("[workspace]\nfiles = {{ {name:?} = \"x\" }}\n");
            let e = parse_here(&text).unwrap_err();
            assert!(e.starts_with("workspace.files"), "{name:?}: {e}");
        }
        let p = parse_here("[workspace]\nfiles = { \"go.mod\" = \"module x\\n\", \"src/a b/c.go\" = \"\" }\n").unwrap();
        assert_eq!(p.workspace.names, ["go.mod", "src/a b/c.go"]);
    }

    #[test]
    fn profile_regression_names_are_portable_in_every_component() {
        let mut forbidden: Vec<String> = ":<>\"|?*\\".chars().map(|c| format!("file{c}name")).collect();
        forbidden.extend((0..=31).chain(127..=159).map(|c| format!("file{}name", char::from_u32(c).unwrap())));
        forbidden.extend(["file.", "file ", "CON", "prn", "Aux.txt", "nUl.tar.gz"].map(str::to_string));
        for prefix in ["COM", "LPT"] {
            for n in 1..=9 {
                forbidden.push(format!("{prefix}{n}"));
                forbidden.push(format!("{}{n}.txt", prefix.to_lowercase()));
            }
        }
        for name in forbidden {
            for path in [name.clone(), format!("src/{name}"), format!("{name}/file.txt")] {
                assert!(safe_name(&path).is_err(), "{path:?}");
            }
        }
        for name in ["src/a b/c.go", "COM0", "LPT10.txt", "CONSOLE", "src/lib.rs"] {
            assert!(safe_name(name).is_ok(), "{name:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn profile_regression_fixture_names_are_also_portable() {
        for name in ["missing.txt:payload", "NUL.txt", "file."] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join(name), "x").unwrap();
            let error = parse("[workspace]\npath = \".\"\n", dir.path()).unwrap_err();
            assert!(error.starts_with("workspace.path") && error.contains(name), "{error}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn profile_regression_fixture_roots_are_rechecked_before_copying() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("fixture");
        let target = dir.path().join("target");
        let root = dir.path().join("workspace");
        std::fs::create_dir(&fixture).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::write(fixture.join("payload.txt"), "keep").unwrap();
        for spelling in ["fixture", "fixture/", "fixture/."] {
            let profile = parse(&format!("[workspace]\npath = {spelling:?}\n"), dir.path()).unwrap();
            std::fs::rename(&fixture, &target).unwrap();
            std::os::unix::fs::symlink(&target, &fixture).unwrap();
            let error = populate(&root, &profile.workspace).unwrap_err().to_string();
            assert!(error.contains("fixture") && error.contains("symbolic link"), "{error}");
            assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
            assert_eq!(std::fs::read_to_string(target.join("payload.txt")).unwrap(), "keep");
            std::fs::remove_file(&fixture).unwrap();
            std::fs::rename(&target, &fixture).unwrap();
        }
    }

    #[test]
    fn profile_case_collisions_in_inline_paths_are_rejected() {
        for (left, right) in [
            ("Settings.json", "settings.json"),
            ("src/Settings.json", "src/settings.json"),
            ("src/one.txt", "SRC/two.txt"),
            ("a/x.txt", "A"),
            ("A", "a/x.txt"),
            ("src/Nested/one.txt", "src/nested/two.txt"),
        ] {
            let error = parse_here(&format!("[workspace]\nfiles = {{ {left:?} = \"one\", {right:?} = \"two\" }}\n"))
                .unwrap_err();
            assert!(error.contains(left) && error.contains(right) && error.contains("ASCII case"), "{error}");
        }
        assert!(
            parse_here("[workspace]\nfiles = { \"left/Config.txt\" = \"one\", \"right/config.txt\" = \"two\" }\n")
                .is_ok()
        );
    }

    #[test]
    fn profile_case_collisions_between_fixture_and_inline_paths_are_rejected() {
        for (fixture_name, inline_name, directory) in [
            ("src/Settings.json", "src/settings.json", false),
            ("src/Settings.json", "SRC/other.json", false),
            ("src", "SRC/child.txt", false),
            ("src", "SRC", true),
            ("src/empty", "src/EMPTY", true),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("fixture").join(fixture_name);
            if directory {
                std::fs::create_dir_all(&path).unwrap();
            } else {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, "original").unwrap();
            }
            let error = parse(
                &format!("[workspace]\npath = \"fixture\"\nfiles = {{ {inline_name:?} = \"override\" }}\n"),
                dir.path(),
            )
            .unwrap_err();
            assert!(
                error.contains(fixture_name) && error.contains(inline_name) && error.contains("ASCII case"),
                "{error}"
            );
        }
    }

    #[test]
    fn lspchecks_own_files_are_reserved() {
        for name in [
            "doc.txt",
            "DOC.TXT",
            "crlf.txt",
            "lspcheck-utf8.txt",
            "missing.txt",
            "doc.txt/inner",
            "lspcheck-large.txt",
            "LSPCHECK-LARGE.TXT",
            "lspcheck-large.txt/inner",
            "lspcheck-k1.txt",
        ] {
            let text = format!("[workspace]\nfiles = {{ {name:?} = \"x\" }}\n");
            let e = parse_here(&text).unwrap_err();
            assert!(e.contains("reserved"), "{name:?}: {e}");
        }
        assert!(parse_here("[workspace]\nfiles = { \"sub/doc.txt\" = \"x\" }\n").is_ok());
        for name in ["crlf.txt", "lspcheck-large.txt", "LSPCHECK-LARGE.TXT", "lspcheck-large.txt/inner"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "x").unwrap();
            let e = parse("[workspace]\npath = \".\"\n", dir.path()).unwrap_err();
            assert!(e.starts_with("workspace.path") && e.contains("reserved"), "{e}");
        }
    }

    #[test]
    fn inline_files_dont_collide_with_each_other_or_the_fixture() {
        let e = parse_here("[workspace]\nfiles = { \"a\" = \"x\", \"a/b\" = \"y\" }\n").unwrap_err();
        assert!(e.contains("\"a/b\""), "{e}");
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("go.mod"), "x").unwrap();
        let text = |files: &str| format!("[workspace]\npath = \".\"\nfiles = {{ {files} }}\n");
        assert!(parse(&text("\"src\" = \"x\""), dir.path()).unwrap_err().contains("directory in workspace.path"));
        assert!(parse(&text("\"go.mod/x\" = \"x\""), dir.path()).unwrap_err().contains("has a file"));
        // Overwriting a copied file is how inline files refine a fixture.
        let p = parse(&text("\"go.mod\" = \"y\", \"src/main.go\" = \"z\""), dir.path()).unwrap();
        assert_eq!(p.workspace.names, ["go.mod", "src/main.go"]);
    }

    #[test]
    fn the_fixture_must_be_an_existing_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(parse("[workspace]\npath = \"nope\"\n", dir.path()).unwrap_err().starts_with("workspace.path"));
        std::fs::write(dir.path().join("file"), "x").unwrap();
        assert!(parse("[workspace]\npath = \"file\"\n", dir.path()).unwrap_err().contains("not a directory"));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_in_the_fixture_is_refused_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("fixture");
        std::fs::create_dir_all(fixture.join("sub")).unwrap();
        std::os::unix::fs::symlink("/etc", fixture.join("sub/escape")).unwrap();
        let e = parse("[workspace]\npath = \"fixture\"\n", dir.path()).unwrap_err();
        assert!(e.contains("sub/escape") && e.contains("symbolic link"), "{e}");
    }

    #[test]
    fn populate_copies_the_fixture_then_writes_inline_files_over_it() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("fixture");
        std::fs::create_dir_all(fixture.join("a/b")).unwrap();
        std::fs::write(fixture.join("a/b/c.txt"), "deep").unwrap();
        std::fs::write(fixture.join("top.txt"), "old").unwrap();
        let p = parse(
            "[workspace]\npath = \"fixture\"\nfiles = { \"top.txt\" = \"new\", \"x/y.txt\" = \"made\" }\n",
            dir.path(),
        )
        .unwrap();
        assert_eq!(p.workspace.names, ["a/b/c.txt", "top.txt", "x/y.txt"]);
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        populate(&root, &p.workspace).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("a/b/c.txt")).unwrap(), "deep");
        assert_eq!(std::fs::read_to_string(root.join("top.txt")).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(root.join("x/y.txt")).unwrap(), "made");
    }

    #[cfg(unix)]
    #[test]
    fn profile_readonly_fixture_overrides_use_default_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("fixture");
        std::fs::create_dir_all(fixture.join("src")).unwrap();
        for name in ["settings.json", "src/settings.json", "unchanged.txt"] {
            let path = fixture.join(name);
            std::fs::write(&path, "original").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        }
        let profile = parse(
            "[workspace]\npath = \"fixture\"\nfiles = { \"settings.json\" = \"new top\", \"src/settings.json\" = \"new nested\", \"added.txt\" = \"fresh\" }\n",
            dir.path(),
        ).unwrap();
        let root = dir.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        let reference = root.join("reference.txt");
        std::fs::write(&reference, "ordinary creation").unwrap();
        let default_mode = std::fs::metadata(&reference).unwrap().permissions().mode() & 0o777;
        populate(&root, &profile.workspace).unwrap();
        for (name, text) in [("settings.json", "new top"), ("src/settings.json", "new nested"), ("added.txt", "fresh")]
        {
            let path = root.join(name);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, default_mode);
        }
        assert_eq!(std::fs::read_to_string(root.join("unchanged.txt")).unwrap(), "original");
        assert_eq!(std::fs::metadata(root.join("unchanged.txt")).unwrap().permissions().mode() & 0o777, 0o444);
        for name in ["settings.json", "src/settings.json", "unchanged.txt"] {
            let path = fixture.join(name);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o444);
        }
    }

    #[cfg(windows)]
    #[test]
    fn profile_readonly_fixture_attributes_allow_inline_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("fixture");
        std::fs::create_dir(&fixture).unwrap();
        let source = fixture.join("settings.json");
        std::fs::write(&source, "original").unwrap();
        let mut permissions = std::fs::metadata(&source).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&source, permissions.clone()).unwrap();
        let profile =
            parse("[workspace]\npath = \"fixture\"\nfiles = { \"settings.json\" = \"inline\" }\n", dir.path()).unwrap();
        let root = dir.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        populate(&root, &profile.workspace).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("settings.json")).unwrap(), "inline");
        assert!(!std::fs::metadata(root.join("settings.json")).unwrap().permissions().readonly());
        assert!(std::fs::metadata(&source).unwrap().permissions().readonly());
        assert_eq!(std::fs::read_to_string(&source).unwrap(), "original");
        permissions.set_readonly(false);
        std::fs::set_permissions(&source, permissions).unwrap();
    }

    #[test]
    fn a_document_names_a_workspace_file_with_its_language_and_positions() {
        let p = parse_here(
            "[workspace]\nfiles = { \"src/main.go\" = \"package main\\n\\nfunc main() {}\\n\" }\n\
             [document]\npath = \"src/main.go\"\nlanguage_id = \"go\"\npositions = [\"1:9\", \"3:6\", \"4:1\"]\n",
        )
        .unwrap();
        let spot = |line, column| Spot { line, column };
        assert_eq!(
            p.document,
            Some(Document {
                path: "src/main.go".into(),
                language_id: "go".into(),
                positions: Some(vec![spot(0, 8), spot(2, 5), spot(3, 0)]),
            })
        );
        // Positions are optional.
        let p = parse_here(
            "[workspace]\nfiles = { \"a.go\" = \"x\" }\n[document]\npath = \"a.go\"\nlanguage_id = \"go\"\n",
        );
        assert_eq!(p.unwrap().document.unwrap().positions, None);
        assert_eq!(parse_here("").unwrap().document, None);
    }

    #[test]
    fn a_fixture_file_can_be_the_document_and_must_be_utf8() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("fixture/pkg")).unwrap();
        std::fs::write(dir.path().join("fixture/pkg/a.py"), "def f():\n    pass\n").unwrap();
        std::fs::write(dir.path().join("fixture/blob.bin"), [0xff, 0xfe, 0x00]).unwrap();
        let text = |path: &str| {
            format!(
                "[workspace]\npath = \"fixture\"\n[document]\npath = {path:?}\nlanguage_id = \"python\"\npositions = [\"2:5\"]\n"
            )
        };
        assert!(parse(&text("pkg/a.py"), dir.path()).is_ok());
        let e = parse(&text("blob.bin"), dir.path()).unwrap_err();
        assert!(e.starts_with("document.path") && e.contains("isn't UTF-8"), "{e}");
        // A directory isn't a file.
        let e = parse(&text("pkg"), dir.path()).unwrap_err();
        assert!(e.starts_with("document.path") && e.contains("isn't a file"), "{e}");
        // An inline file replaces the fixture's text, which positions must fit.
        let inline = "[workspace]\npath = \"fixture\"\nfiles = { \"pkg/a.py\" = \"x\" }\n\
                      [document]\npath = \"pkg/a.py\"\nlanguage_id = \"python\"\npositions = [\"2:5\"]\n";
        let e = parse(inline, dir.path()).unwrap_err();
        assert!(e.contains("in pkg/a.py, 2:5 is past the end"), "{e}");
    }

    #[test]
    fn a_bad_document_is_an_error_naming_the_key() {
        let with = |document: &str| {
            parse_here(&format!(
                "[workspace]\nfiles = {{ \"main.go\" = \"package main\\nfunc é() {{}}\\n\" }}\n[document]\n{document}"
            ))
            .unwrap_err()
        };
        for (document, want) in [
            ("language_id = \"go\"\n", "document.path is required"),
            ("path = \"other.go\"\nlanguage_id = \"go\"\n", "isn't a file the profile's workspace provides"),
            ("path = \"../main.go\"\nlanguage_id = \"go\"\n", "document.path: \"../main.go\""),
            ("path = \"main.go\"\n", "document.language_id is required"),
            ("path = \"main.go\"\nlanguage_id = \" \"\n", "document.language_id is empty"),
            ("path = \"main.go\"\nlanguage_id = \"go\"\npositions = []\n", "document.positions is empty"),
            ("path = \"main.go\"\nlanguage_id = \"go\"\npositions = [\"3\"]\n", "\"3\" isn't LINE:COLUMN"),
            ("path = \"main.go\"\nlanguage_id = \"go\"\npositions = [\"0:1\"]\n", "\"0:1\" isn't LINE:COLUMN"),
            (
                "path = \"main.go\"\nlanguage_id = \"go\"\npositions = [\"4:1\"]\n",
                "4:1 is past the end: the document has 3 line(s)",
            ),
            // Columns count characters: line 2 has 11, so 12 is its end.
            (
                "path = \"main.go\"\nlanguage_id = \"go\"\npositions = [\"2:12\", \"2:13\"]\n",
                "columns run from 1 to 12",
            ),
            (
                "path = \"main.go\"\nlanguage_id = \"go\"\npositions = [\"1:1\", \"1:1\", \"1:1\", \"1:1\", \"1:1\", \"1:1\"]\n",
                "at most 5",
            ),
            ("path = \"main.go\"\nlanguage_id = \"go\"\nposition = [\"1:1\"]\n", "position"),
            ("path = \"main.go\"\nlanguage_id = \"go\"\npositions = [1]\n", "positions"),
        ] {
            let e = with(document);
            assert!(e.contains(want), "{document}: {e}");
        }
    }

    #[test]
    fn client_values_become_json() {
        let p = parse_here(
            "[client]\ninitialization_options = { hints = true, depth = 3, ratio = 0.5, when = 1979-05-27 }\n\
             [client.configuration.gopls]\nstaticcheck = false\nflags = [\"-x\"]\n",
        )
        .unwrap();
        assert_eq!(
            p.initialization_options.unwrap(),
            serde_json::json!({"depth": 3, "hints": true, "ratio": 0.5, "when": "1979-05-27"})
        );
        assert_eq!(p.configuration.unwrap(), serde_json::json!({"gopls": {"flags": ["-x"], "staticcheck": false}}));
        let e = parse_here("[client]\ninitialization_options = { x = nan }\n").unwrap_err();
        assert!(e.contains("client.initialization_options.x"), "{e}");
        // Any TOML value will do, not only a table.
        assert_eq!(
            parse_here("[client]\ninitialization_options = 7\n").unwrap().initialization_options,
            Some(7.into())
        );
    }

    #[test]
    fn skips_need_a_known_id_and_a_reason() {
        let p = parse_here("[allow]\nskip = [{ id = \"L7\", reason = \"exits 0 by design\" }]\n").unwrap();
        assert_eq!(p.skip, [Skip { id: "L7", reason: "exits 0 by design".into() }]);
        for (entry, want) in [
            ("{ id = \"L99\", reason = \"r\" }", "\"L99\" is not a check"),
            ("{ id = \"L7\", reason = \" \" }", "needs a reason"),
            ("{ id = \"L7\" }", "reason"),
            ("{ id = \"L7\", reason = \"a\" }, { id = \"L7\", reason = \"b\" }", "twice"),
        ] {
            let e = parse_here(&format!("[allow]\nskip = [{entry}]\n")).unwrap_err();
            assert!(e.contains(want), "{entry}: {e}");
        }
    }

    #[test]
    fn the_command_line_wins_over_the_profile_which_wins_over_the_default() {
        let secs = Duration::from_secs;
        assert_eq!(timeout(Some(3), Some(secs(20)), 10), secs(3));
        assert_eq!(timeout(None, Some(Duration::from_millis(1500)), 10), Duration::from_millis(1500));
        assert_eq!(timeout(None, None, 10), secs(10));
        assert_eq!(timeout(Some(0), None, 10), secs(0));
    }

    #[test]
    fn a_command_after_dashes_wins_over_the_profiles() {
        let p = parse_here("[server]\ncommand = [\"gopls\", \"serve\"]\n").unwrap();
        let os = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(server_argv(&os(&["other", "--stdio"]), Some(&p)), Some(os(&["other", "--stdio"])));
        assert_eq!(server_argv(&[], Some(&p)), Some(os(&["gopls", "serve"])));
        assert_eq!(server_argv(&[], Some(&Profile::default())), None);
        assert_eq!(server_argv(&[], None), None);
        assert!(parse_here("[server]\ncommand = []\n").unwrap_err().contains("server.command"));
    }

    const LUA: &str = "[workspace]\nfiles = { \"main.lua\" = \"local x = 1\\n\", \"lib.lua\" = \"return {}\\n\" }\n\
                       [document]\npath = \"main.lua\"\nlanguage_id = \"lua\"\n";

    #[test]
    fn a_mutation_section_is_read_whole() {
        let p = parse_here(&format!(
            "{LUA}[mutation]\nfiles = [\"main.lua\", \"lib.lua\"]\nlanguage_id = \"lua\"\nseed = 20261109\ncases = 10\n\
             steps = 2\noperators = [\"delete-tokens\", \"open-string\", \"nest\"]\nfragments = [\"0x\", \"1e\"]\n\
             requests = [\"textDocument/completion\", \"textDocument/hover\"]\nrequests_per_step = 16\n\
             max_document_kib = 1024\nrun_timeout = \"4h\"\n\
             [mutation.lexicon]\nline_comments = [\"--\"]\nblock_comments = [[\"--[[\", \"]]\"]]\n\
             strings = [{{ open = '\"', close = '\"', escape = '\\' }}, {{ open = \"[[\", close = \"]]\" }}]\n\
             identifier_chars = \"_$\"\nbrackets = [\"()\", \"«»\"]\nkeywords = [\"local\", \"end\"]\n\
             [mutation.recovery]\nprobes = [\"publishDiagnostics\", \"textDocument/documentSymbol\"]\n\
             settle_timeout = \"5m\"\nrecovery_timeout = \"20s\"\nexpect = \"baseline\"\nno_errors = true\n"
        ))
        .unwrap();
        let m = p.mutation.unwrap();
        let seed =
            |path: &str, text: &str| SeedFile { path: path.into(), language_id: "lua".into(), text: text.into() };
        assert_eq!(m.seeds, [seed("main.lua", "local x = 1\n"), seed("lib.lua", "return {}\n")]);
        assert_eq!((m.seed, m.cases, m.requests_per_step), (Some(20261109), 10, 16));
        assert_eq!(m.run_timeout, MAX_RUN_TIMEOUT);
        // Methods keep K1's order, whatever the profile's.
        assert_eq!(m.requests, Some(vec!["textDocument/hover", "textDocument/completion"]));
        assert_eq!(
            m.planner,
            mutation::Config {
                steps: 2,
                operators: vec![Operator::DeleteTokens, Operator::OpenString, Operator::Nest],
                fragments: vec!["0x".into(), "1e".into()],
                max_document_bytes: 1024 * 1024,
                lexicon: Lexicon {
                    line_comments: vec!["--".into()],
                    block_comments: vec![("--[[".into(), "]]".into())],
                    strings: vec![
                        StringDelimiters { open: "\"".into(), close: "\"".into(), escape: Some('\\') },
                        StringDelimiters { open: "[[".into(), close: "]]".into(), escape: None },
                    ],
                    identifier_chars: "_$".into(),
                    brackets: vec![('(', ')'), ('«', '»')],
                    keywords: vec!["local".into(), "end".into()],
                },
            }
        );
        assert_eq!(
            m.recovery,
            Recovery {
                probes: Some(vec![Probe::PublishDiagnostics, Probe::DocumentSymbol]),
                settle_timeout: Some(MAX_SETTLE_TIMEOUT),
                recovery_timeout: Some(Duration::from_secs(20)),
                expect: Expect::Baseline,
                no_errors: true,
            }
        );
    }

    #[test]
    fn an_empty_mutation_section_mutates_the_document_with_the_defaults() {
        let m = parse_here(&format!("{LUA}[mutation]\n")).unwrap().mutation.unwrap();
        let defaults = Mutation::defaults(vec![SeedFile {
            path: "main.lua".into(),
            language_id: "lua".into(),
            text: "local x = 1\n".into(),
        }]);
        assert_eq!(m, defaults);
        assert_eq!((m.cases, m.requests_per_step, m.run_timeout), (20, 4, Duration::from_secs(3600)));
        assert_eq!(m.planner, mutation::Config::default());
        assert_eq!(m.recovery.expect, Expect::Observe);
        // Seed files of their own take [document]'s language id unless given one.
        let m = parse_here(&format!("{LUA}[mutation]\nfiles = [\"lib.lua\"]\n")).unwrap().mutation.unwrap();
        assert_eq!((m.seeds[0].path.as_str(), m.seeds[0].language_id.as_str()), ("lib.lua", "lua"));
        // Without [document], files and a language id are enough.
        let p = parse_here(
            "[workspace]\nfiles = { \"a.txt\" = \"x\" }\n[mutation]\nfiles = [\"a.txt\"]\nlanguage_id = \"plaintext\"\n",
        )
        .unwrap();
        assert_eq!(p.mutation.unwrap().seeds[0].language_id, "plaintext");
    }

    #[test]
    fn a_lexicon_key_left_out_keeps_the_default() {
        let m = parse_here(&format!("{LUA}[mutation.lexicon]\nkeywords = [\"local\"]\n")).unwrap().mutation.unwrap();
        assert_eq!(m.planner.lexicon, Lexicon { keywords: vec!["local".into()], ..Lexicon::default() });
        let m = parse_here(&format!("{LUA}[mutation.lexicon]\nbrackets = []\nidentifier_chars = \"\"\n"))
            .unwrap()
            .mutation
            .unwrap();
        assert!(m.planner.lexicon.brackets.is_empty() && m.planner.lexicon.identifier_chars.is_empty());
    }

    #[test]
    fn a_bad_mutation_section_is_an_error_naming_the_key() {
        let big = "x".repeat(1025);
        for (section, want) in [
            ("files = []\n", "mutation.files is empty"),
            ("files = [\"main.lua\", \"main.lua\"]\n", "mutation.files: \"main.lua\" is listed twice"),
            ("files = [\"other.lua\"]\n", "mutation.files: \"other.lua\" isn't a file"),
            ("files = [\"../main.lua\"]\n", "mutation.files: \"../main.lua\""),
            ("language_id = \"\"\n", "mutation.language_id is empty"),
            ("seed = -1\n", "mutation.seed must be a whole number from 0"),
            ("cases = 0\n", "mutation.cases must be from 1 to 1000, not 0"),
            ("cases = 1001\n", "mutation.cases must be from 1 to 1000, not 1001"),
            ("steps = 4\n", "mutation.steps must be from 1 to 3"),
            ("steps = 0\n", "mutation.steps must be from 1 to 3"),
            ("operators = []\n", "mutation.operators is empty"),
            ("operators = [\"explode\"]\n", "mutation.operators: \"explode\" isn't an operator"),
            ("operators = [\"nest\", \"nest\"]\n", "mutation.operators: nest is listed twice"),
            ("fragments = [\"\"]\n", "mutation.fragments: a fragment is empty"),
            (&format!("fragments = [\"{big}\"]\n"), "a fragment of 1025 bytes"),
            ("requests = []\n", "mutation.requests is empty"),
            ("requests = [\"shutdown\"]\n", "mutation.requests: \"shutdown\" isn't a request K1 asks"),
            ("requests = [\"textDocument/hover\", \"textDocument/hover\"]\n", "listed twice"),
            ("requests_per_step = 0\n", "mutation.requests_per_step must be from 1 to 16"),
            ("requests_per_step = 17\n", "mutation.requests_per_step must be from 1 to 16"),
            ("max_document_kib = 0\n", "mutation.max_document_kib must be from 1 to 1024"),
            ("max_document_kib = 1025\n", "mutation.max_document_kib must be from 1 to 1024"),
            ("run_timeout = \"5h\"\n", "mutation.run_timeout is at most \"4h\""),
            ("run_timeout = \"1 hour\"\n", "mutation.run_timeout must be"),
            ("cases = \"20\"\n", "cases"),
            ("mutate = true\n", "mutate"),
            ("[mutation.lexicon]\nline_comments = [\"\"]\n", "mutation.lexicon.line_comments: a delimiter is empty"),
            ("[mutation.lexicon]\nline_comments = [\"--\\n\"]\n", "holds a line end"),
            ("[mutation.lexicon]\nblock_comments = [[\"/*\", \"\"]]\n", "mutation.lexicon.block_comments"),
            ("[mutation.lexicon]\nblock_comments = [[\"/*\"]]\n", "block_comments"),
            ("[mutation.lexicon]\nstrings = [{ open = \"\\r\", close = \"'\" }]\n", "mutation.lexicon.strings"),
            ("[mutation.lexicon]\nstrings = [{ open = \"'\", close = \"'\", escape = \"\" }]\n", "escape \"\""),
            (
                "[mutation.lexicon]\nstrings = [{ open = \"'\", close = \"'\", escape = \"\\\\\\\\\" }]\n",
                "one character",
            ),
            ("[mutation.lexicon]\nstrings = [{ open = \"'\" }]\n", "close"),
            ("[mutation.lexicon]\nidentifier_chars = \"_\\n\"\n", "mutation.lexicon.identifier_chars"),
            ("[mutation.lexicon]\nbrackets = [\"(\"]\n", "mutation.lexicon.brackets: \"(\" must be two characters"),
            ("[mutation.lexicon]\nbrackets = [\"(])\"]\n", "must be two characters"),
            ("[mutation.lexicon]\nbrackets = [\"\\n)\"]\n", "holds a line end"),
            ("[mutation.lexicon]\nkeywords = [\"\"]\n", "mutation.lexicon.keywords"),
            ("[mutation.lexicon]\ncomments = [\"#\"]\n", "comments"),
            ("[mutation.recovery]\nprobes = []\n", "mutation.recovery.probes is empty"),
            ("[mutation.recovery]\nprobes = [\"textDocument/hover\"]\n", "isn't a recovery probe"),
            ("[mutation.recovery]\nprobes = [\"publishDiagnostics\", \"publishDiagnostics\"]\n", "listed twice"),
            ("[mutation.recovery]\nsettle_timeout = \"6m\"\n", "mutation.recovery.settle_timeout is at most"),
            ("[mutation.recovery]\nrecovery_timeout = \"soon\"\n", "mutation.recovery.recovery_timeout must be"),
            ("[mutation.recovery]\nexpect = \"fixed\"\n", "\"observe\" or \"baseline\", not \"fixed\""),
            ("[mutation.recovery]\nno_errors = \"yes\"\n", "no_errors"),
        ] {
            let e = parse_here(&format!("{LUA}[mutation]\n{section}")).unwrap_err();
            assert!(e.contains(want), "{section}: {e}");
        }
        // Seed files need a language id, and something to mutate.
        let no_document = "[workspace]\nfiles = { \"a.txt\" = \"x\" }\n[mutation]\n";
        let e = parse_here(no_document).unwrap_err();
        assert!(e.contains("mutation.files is required without a [document]"), "{e}");
        let e = parse_here(&format!("{no_document}files = [\"a.txt\"]\n")).unwrap_err();
        assert!(e.contains("mutation.language_id is required"), "{e}");
    }

    #[test]
    fn a_seed_file_must_fit_the_document_limit_and_be_utf8() {
        let text = |kib: usize| {
            format!(
                "[workspace]\nfiles = {{ \"a.txt\" = \"{}\" }}\n[mutation]\nfiles = [\"a.txt\"]\n\
                 language_id = \"plaintext\"\nmax_document_kib = {kib}\n",
                "x".repeat(2048)
            )
        };
        assert!(parse_here(&text(2)).is_ok());
        let e = parse_here(&text(1)).unwrap_err();
        assert!(
            e.contains("mutation.files: \"a.txt\" is 2048 bytes, more than mutation.max_document_kib (1 KiB)"),
            "{e}"
        );
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("fixture")).unwrap();
        std::fs::write(dir.path().join("fixture/blob.bin"), [0xff, 0xfe]).unwrap();
        let e = parse(
            "[workspace]\npath = \"fixture\"\n[mutation]\nfiles = [\"blob.bin\"]\nlanguage_id = \"x\"\n",
            dir.path(),
        )
        .unwrap_err();
        assert!(e.starts_with("mutation.files") && e.contains("isn't UTF-8"), "{e}");
    }

    #[test]
    fn mutate_uses_the_section_else_the_document_else_nothing() {
        let secs = Duration::from_secs;
        let section = parse_here(&format!("{LUA}[mutation]\nfiles = [\"lib.lua\"]\nseed = 7\n")).unwrap();
        let m = mutation_settings(&section, None, None, secs(10)).unwrap();
        assert_eq!((m.seeds[0].path.as_str(), m.seed, m.cases), ("lib.lua", Some(7), 20));
        // The flags win.
        let m = mutation_settings(&section, Some(9), Some(1000), secs(10)).unwrap();
        assert_eq!((m.seed, m.cases), (Some(9), 1000));
        for cases in [0, 1001] {
            let e = mutation_settings(&section, None, Some(cases), secs(10)).unwrap_err();
            assert!(e.contains(&format!("--mutation-cases must be from 1 to 1000, not {cases}")), "{e}");
        }
        // [document] alone gets the defaults.
        let document = parse_here(LUA).unwrap();
        assert_eq!(document.mutation, None);
        let m = mutation_settings(&document, None, None, secs(10)).unwrap();
        assert_eq!(m, Mutation::defaults(section_seed("main.lua", "local x = 1\n")));
        // Nothing to mutate: M1 to M3 will be skipped, so the budget isn't checked.
        let m = mutation_settings(&Profile::default(), None, None, secs(3600)).unwrap();
        assert!(m.seeds.is_empty());
    }

    fn section_seed(path: &str, text: &str) -> Vec<SeedFile> {
        vec![SeedFile { path: path.into(), language_id: "lua".into(), text: text.into() }]
    }

    #[test]
    fn a_document_too_large_for_the_default_limit_is_refused_only_by_mutate() {
        let p = parse_here(&format!(
            "[workspace]\nfiles = {{ \"big.txt\" = \"{}\" }}\n[document]\npath = \"big.txt\"\nlanguage_id = \"x\"\n",
            "x".repeat(64 * 1024 + 1)
        ))
        .unwrap();
        let e = mutation_settings(&p, None, None, Duration::from_secs(10)).unwrap_err();
        assert!(e.contains("is 65537 bytes, more than the 64 KiB") && e.contains("max_document_kib"), "{e}");
    }

    #[test]
    fn the_case_budget_counts_every_deadline_of_a_case() {
        let secs = Duration::from_secs;
        let mut m = Mutation::defaults(section_seed("main.lua", ""));
        // (3 + 1) steps × (1 + 4) writes and replies × 30 s, the restoration's write, recovery_timeout (60 s),
        // three terminal probes' writes and replies, and 5 s.
        assert_eq!(m.case_budget(secs(30)), secs(600 + 30 + 60 + 180 + 5));
        m.recovery.probes = Some(vec![Probe::PublishDiagnostics]);
        m.recovery.recovery_timeout = Some(secs(20));
        m.seeds.push(m.seeds[0].clone());
        assert_eq!(m.case_budget(secs(30)), secs(600 + 2 * 30 + 20 + 5));
        m.planner.steps = 1;
        m.requests_per_step = 1;
        assert_eq!(m.case_budget(secs(1)), secs(4 + 2 + 20 + 5));
    }

    #[test]
    fn a_case_budget_over_the_run_timeout_is_refused_naming_both() {
        let p = parse_here(&format!("{LUA}[mutation]\nrun_timeout = \"14m\"\n")).unwrap();
        // 875 s at 30 s per request: one case can't fit in 840 s.
        let e = mutation_settings(&p, None, None, Duration::from_secs(30)).unwrap_err();
        assert!(e.contains("up to 875 s") && e.contains("of 30 s") && e.contains("run_timeout of 840 s"), "{e}");
        assert!(mutation_settings(&p, None, None, Duration::from_secs(10)).is_ok());
        let e = mutation_settings(&p, None, None, Duration::from_millis(29_500)).unwrap_err();
        assert!(e.contains("of 29.500 s"), "{e}");
    }

    #[test]
    fn a_random_seed_fits_json_and_toml() {
        let seeds: Vec<u64> = (0..64).map(|_| random_seed()).collect();
        assert!(seeds.iter().all(|&s| s < 1 << 53));
        assert!(seeds.iter().any(|&s| s != seeds[0]), "{seeds:?}");
    }

    #[test]
    fn a_command_path_is_relative_to_the_profile() {
        let p = parse_here("[server]\ncommand = [\"bin/server\", \"bin/arg\"]\n").unwrap();
        let argv = p.command.unwrap();
        assert_eq!(PathBuf::from(&argv[0]), Path::new("/profiles").join("bin/server"));
        // Arguments are passed as written.
        assert_eq!(argv[1], "bin/arg");
    }
}
