// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! The terminal summary and the JSON report.

use crate::checks::{self, Basis, CheckResult, ScenarioRun, Verdict};
use serde_json::{Value, json};
use std::io::Write;

/// The JSON report's schema identifier.
pub const SCHEMA: &str = "lspcheck-report-v1";

/// Results in catalogue order.
pub fn ordered(runs: &[ScenarioRun]) -> Vec<&CheckResult> {
    let mut all: Vec<&CheckResult> = runs.iter().flat_map(|r| &r.results).collect();
    all.sort_by_key(|r| checks::CATALOGUE.iter().position(|c| c.id == r.id));
    all
}

/// The process exit status for a run: 1 when a MUST check (or with `strict`,
/// any check) failed, 2 when lspcheck itself hit an error, otherwise 0.
pub fn exit_code(runs: &[ScenarioRun], strict: bool) -> i32 {
    let results = ordered(runs);
    let failed = |r: &CheckResult| r.verdict == Verdict::Fail;
    if results.iter().any(|r| failed(r) && checks::info(r.id).basis == Basis::Must) {
        return 1;
    }
    if results.iter().any(|r| r.verdict == Verdict::Error) {
        return 2;
    }
    if strict && results.iter().any(|r| failed(r)) {
        return 1;
    }
    0
}

pub fn print_terminal(out: &mut impl Write, server: &str, runs: &[ScenarioRun]) -> std::io::Result<()> {
    writeln!(out, "lspcheck {} — {server}", env!("CARGO_PKG_VERSION"))?;
    let results = ordered(runs);
    for r in &results {
        let c = checks::info(r.id);
        writeln!(out, "  {:<5} {:<3} {:<6}  {}", r.verdict.label(), c.id, c.basis.label(), c.title)?;
        if !r.detail.is_empty() {
            writeln!(out, "                     {}", r.detail)?;
        }
        let transcript = runs.iter().find(|run| run.name == r.scenario).and_then(|run| run.transcript.as_ref());
        if matches!(r.verdict, Verdict::Fail | Verdict::Error)
            && let Some(path) = transcript
        {
            writeln!(out, "                     transcript: {}", path.display())?;
        }
    }
    let count = |v: Verdict| results.iter().filter(|r| r.verdict == v).count();
    let failed_by =
        |b: Basis| results.iter().filter(|r| r.verdict == Verdict::Fail && checks::info(r.id).basis == b).count();
    writeln!(
        out,
        "{} checks: {} pass, {} fail ({} MUST, {} SHOULD, {} POLICY), {} skipped, {} not exercised, {} errors",
        results.len(),
        count(Verdict::Pass),
        count(Verdict::Fail),
        failed_by(Basis::Must),
        failed_by(Basis::Should),
        failed_by(Basis::Policy),
        count(Verdict::Skip),
        count(Verdict::NotExercised),
        count(Verdict::Error),
    )
}

pub fn json_report(server: &[String], runs: &[ScenarioRun]) -> Value {
    let results: Vec<Value> = ordered(runs)
        .into_iter()
        .map(|r| {
            let c = checks::info(r.id);
            json!({
                "id": r.id,
                "area": c.area,
                "basis": c.basis,
                "title": c.title,
                "spec": checks::spec_url(c),
                "verdict": r.verdict,
                "detail": r.detail,
                "scenario": r.scenario,
            })
        })
        .collect();
    json!({
        "schema": SCHEMA,
        "lspcheck": env!("CARGO_PKG_VERSION"),
        "server": server,
        "host": {"os": std::env::consts::OS, "arch": std::env::consts::ARCH},
        "results": results,
    })
}
