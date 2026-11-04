// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! Runs the lspcheck binary against the stand-in server: clean, every check
//! passes; with each fault, the check that exists to catch it fails.

#![cfg(feature = "standin")]

use serde_json::Value;
use std::collections::BTreeMap;
use std::process::Command;

const LSPCHECK: &str = env!("CARGO_BIN_EXE_lspcheck");
const STANDIN: &str = env!("CARGO_BIN_EXE_lspcheck-standin");

struct Run {
    status: i32,
    verdicts: BTreeMap<String, String>,
    stdout: String,
}

fn run(standin_args: &[&str], lspcheck_args: &[&str]) -> Run {
    let dir = tempfile::tempdir().unwrap();
    let report = dir.path().join("report.json");
    let out = Command::new(LSPCHECK)
        .args([
            "--timeout",
            "5",
            "--startup-timeout",
            "5",
            "--exit-timeout",
            "2",
        ])
        .args(lspcheck_args)
        .arg("--json")
        .arg(&report)
        .arg("--transcripts")
        .arg(dir.path().join("t"))
        .arg("--")
        .arg(STANDIN)
        .args(standin_args)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let text = std::fs::read_to_string(&report).unwrap_or_else(|e| {
        panic!(
            "no report ({e}); stdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    let json: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(json["schema"], "lspcheck-report-v1");
    let verdicts = json["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["id"].as_str().unwrap().to_string(),
                r["verdict"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    for scenario in ["lifecycle", "pre-initialize", "exit-without-shutdown"] {
        let path = dir.path().join("t").join(format!("{scenario}.jsonl"));
        if path.exists() {
            for line in std::fs::read_to_string(&path).unwrap().lines() {
                serde_json::from_str::<Value>(line)
                    .unwrap_or_else(|e| panic!("{scenario}: bad transcript line {line}: {e}"));
            }
        }
    }
    Run {
        status: out.status.code().unwrap_or(-1),
        verdicts,
        stdout,
    }
}

fn fault(name: &str) -> Run {
    run(&["--fault", name], &[])
}

fn expect(run: &Run, id: &str, verdict: &str) {
    assert_eq!(
        run.verdicts.get(id).map(String::as_str),
        Some(verdict),
        "{id}\n{}",
        run.stdout
    );
}

/// Every check except `failing` passes.
fn only_fails(run: &Run, failing: &[&str]) {
    for (id, verdict) in &run.verdicts {
        let want = if failing.contains(&id.as_str()) {
            "fail"
        } else {
            "pass"
        };
        if id == "L9" && !cfg!(unix) {
            continue;
        }
        assert_eq!(verdict, want, "{id}\n{}", run.stdout);
    }
}

#[test]
fn the_clean_standin_passes_every_check() {
    let r = run(&[], &[]);
    only_fails(&r, &[]);
    assert_eq!(r.status, 0, "{}", r.stdout);
    assert_eq!(r.verdicts.len(), lspcheck::checks::CATALOGUE.len());
}

#[test]
fn stdout_banner_fails_f1() {
    let r = fault("stdout-banner");
    only_fails(&r, &["F1"]);
    assert_eq!(r.status, 1, "F1 is a MUST check");
    assert!(r.stdout.contains("standin starting"), "{}", r.stdout);
}

#[test]
fn answering_before_initialize_fails_l1() {
    let r = fault("no-pre-init-error");
    only_fails(&r, &["L1"]);
    assert_eq!(r.status, 0, "L1 is a SHOULD check");
}

#[test]
fn strict_turns_should_failures_into_status_1() {
    let r = run(&["--fault", "no-pre-init-error"], &["--strict"]);
    assert_eq!(r.status, 1);
}

#[test]
fn bad_initialize_results_fail_l2() {
    only_fails(&fault("bad-encoding"), &["L2"]);
    only_fails(&fault("no-capabilities"), &["L2"]);
}

#[test]
fn wrong_unknown_method_errors_fail_j4() {
    only_fails(&fault("wrong-unknown-error"), &["J4"]);
    only_fails(&fault("ignore-dollar-request"), &["J4"]);
}

#[test]
fn a_non_null_shutdown_result_fails_l4() {
    only_fails(&fault("shutdown-result"), &["L4"]);
}

#[test]
fn exiting_on_shutdown_fails_l4_and_skips_l6() {
    let r = fault("exit-on-shutdown");
    expect(&r, "L4", "fail");
    expect(&r, "L6", "not-exercised");
}

#[test]
fn a_wrong_exit_status_fails_l6_and_l7() {
    only_fails(&fault("exit-status"), &["L6", "L7"]);
}

#[test]
fn ignoring_exit_fails_l6_and_l7() {
    let r = fault("ignore-exit");
    expect(&r, "L6", "fail");
    expect(&r, "L7", "fail");
    // lspcheck had to stop it, so cleanup is untested rather than passed.
    if cfg!(unix) {
        expect(&r, "L9", "not-exercised");
    }
}

#[cfg(unix)]
#[test]
fn an_orphaned_child_fails_l9() {
    let r = fault("orphan-child");
    only_fails(&r, &["L9"]);
    assert!(
        r.stdout.contains("lspcheck-stand"),
        "names the process:\n{}",
        r.stdout
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_child_that_leaves_the_process_group_still_fails_l9() {
    only_fails(&fault("escaped-child"), &["L9"]);
}

#[test]
fn only_runs_the_named_checks() {
    let r = run(&[], &["--only", "L4,L7"]);
    assert_eq!(r.verdicts.keys().cloned().collect::<Vec<_>>(), ["L4", "L7"]);
}

#[test]
fn a_missing_server_is_an_error_not_a_failure() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(LSPCHECK)
        .arg("--json")
        .arg(dir.path().join("r.json"))
        .args(["--", "/nonexistent/lspcheck-no-such-server"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let json: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("r.json")).unwrap()).unwrap();
    assert!(
        json["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["verdict"] == "error")
    );
}
