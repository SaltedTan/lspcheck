// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! The terminal summary, the JSON report, and transcript files.

use crate::checks::{self, Basis, CheckResult, ScenarioRun, Verdict};
use crate::session::{Event, EventKind};
use crate::transport::Chunk;
use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;

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
    if results
        .iter()
        .any(|r| failed(r) && checks::info(r.id).basis == Basis::Must)
    {
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

pub fn print_terminal(
    out: &mut impl Write,
    server: &str,
    runs: &[ScenarioRun],
    transcripts: Option<&Path>,
) -> std::io::Result<()> {
    writeln!(out, "lspcheck {} — {server}", env!("CARGO_PKG_VERSION"))?;
    let results = ordered(runs);
    for r in &results {
        let c = checks::info(r.id);
        writeln!(
            out,
            "  {:<5} {:<3} {:<6}  {}",
            r.verdict.label(),
            c.id,
            c.basis.label(),
            c.title
        )?;
        if !r.detail.is_empty() {
            writeln!(out, "                     {}", r.detail)?;
        }
        if r.verdict == Verdict::Fail
            && let Some(dir) = transcripts
        {
            writeln!(
                out,
                "                     transcript: {}",
                dir.join(format!("{}.jsonl", r.scenario)).display()
            )?;
        }
    }
    let count = |v: Verdict| results.iter().filter(|r| r.verdict == v).count();
    let failed_by = |b: Basis| {
        results
            .iter()
            .filter(|r| r.verdict == Verdict::Fail && checks::info(r.id).basis == b)
            .count()
    };
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

/// Writes one JSON object per transcript event.
pub fn write_transcript(path: &Path, events: &[Event]) -> std::io::Result<()> {
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    for e in events {
        let t = e.at.as_secs_f64() * 1000.0;
        let line = match &e.kind {
            EventKind::Sent(b) => with_bytes(json!({"t_ms": t, "dir": "out"}), "data", b),
            EventKind::Received(Chunk::Frame { headers, body }) => {
                let h: Vec<Value> = headers.iter().map(|h| json!([h.name, h.value])).collect();
                with_bytes(
                    json!({"t_ms": t, "dir": "in", "kind": "frame", "headers": h}),
                    "body",
                    body,
                )
            }
            EventKind::Received(Chunk::Garbage(b)) => with_bytes(
                json!({"t_ms": t, "dir": "in", "kind": "garbage"}),
                "data",
                b,
            ),
            EventKind::Received(Chunk::BadHeader { block, reason }) => with_bytes(
                json!({"t_ms": t, "dir": "in", "kind": "bad-header", "reason": reason}),
                "data",
                block,
            ),
            EventKind::Received(Chunk::Truncated {
                content_length,
                body,
                ..
            }) => with_bytes(
                json!({"t_ms": t, "dir": "in", "kind": "truncated", "content_length": content_length}),
                "body",
                body,
            ),
            EventKind::Stderr(b) => with_bytes(json!({"t_ms": t, "dir": "err"}), "data", b),
            EventKind::StdoutClosed => json!({"t_ms": t, "dir": "meta", "event": "stdout closed"}),
            EventKind::Exited(how) => {
                json!({"t_ms": t, "dir": "meta", "event": "exited", "how": how})
            }
            EventKind::Note(text) => json!({"t_ms": t, "dir": "meta", "note": text}),
        };
        serde_json::to_writer(&mut out, &line)?;
        out.write_all(b"\n")?;
    }
    out.flush()
}

/// Adds bytes as `<key>` text when they are UTF-8, else as `<key>_base64`.
fn with_bytes(mut v: Value, key: &str, bytes: &[u8]) -> Value {
    let obj = v.as_object_mut().expect("an object");
    match std::str::from_utf8(bytes) {
        Ok(text) => obj.insert(key.into(), text.into()),
        Err(_) => obj.insert(format!("{key}_base64"), base64(bytes).into()),
    };
    v
}

fn base64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64_matches_rfc_4648_vectors() {
        for (input, want) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(super::base64(input.as_bytes()), want);
        }
    }
}
