// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! Runs the lspcheck binary against the stand-in server: clean, every check
//! passes; with each fault, the check that exists to catch it fails, and
//! lawful-but-unusual behaviour fails nothing.

#![cfg(feature = "standin")]

use serde_json::Value;
use std::collections::BTreeMap;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const LSPCHECK: &str = env!("CARGO_BIN_EXE_lspcheck");
const STANDIN: &str = env!("CARGO_BIN_EXE_lspcheck-standin");
/// No run should take this long; a hang fails the test instead of the job.
const WATCHDOG: Duration = Duration::from_secs(90);

struct Run {
    status: i32,
    verdicts: BTreeMap<String, String>,
    stdout: String,
    /// What the server wrote to stderr, from every transcript.
    server_stderr: String,
    took: Duration,
}

/// Runs `lspcheck [lspcheck_args] -- <server>` with a watchdog.
fn run_command(lspcheck_args: &[&str], server: &[&str], cwd: Option<&std::path::Path>) -> Run {
    let dir = tempfile::tempdir().unwrap();
    let report = dir.path().join("report.json");
    let mut cmd = Command::new(LSPCHECK);
    // Defaults the caller can override.
    for (flag, value) in [("--timeout", "5"), ("--startup-timeout", "5"), ("--exit-timeout", "2")] {
        if !lspcheck_args.contains(&flag) {
            cmd.args([flag, value]);
        }
    }
    cmd.args(lspcheck_args)
        .arg("--json")
        .arg(&report)
        .arg("--transcripts")
        .arg(dir.path().join("t"))
        .arg("--")
        .args(server)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    let started = Instant::now();
    let mut child = cmd.spawn().unwrap();
    let out = loop {
        if child.try_wait().unwrap().is_some() {
            break child.wait_with_output().unwrap();
        }
        if started.elapsed() > WATCHDOG {
            let _ = child.kill();
            panic!("lspcheck ran longer than {WATCHDOG:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let took = started.elapsed();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let text = std::fs::read_to_string(&report).unwrap_or_else(|e| {
        panic!("no report ({e}); stdout:\n{stdout}\nstderr:\n{}", String::from_utf8_lossy(&out.stderr))
    });
    let json: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(json["schema"], "lspcheck-report-v1");
    let verdicts = json["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["id"].as_str().unwrap().to_string(), r["verdict"].as_str().unwrap().to_string()))
        .collect();
    let mut server_stderr = String::new();
    if let Ok(entries) = std::fs::read_dir(dir.path().join("t")) {
        for entry in entries {
            let path = entry.unwrap().path();
            for line in std::fs::read_to_string(&path).unwrap().lines() {
                let event = serde_json::from_str::<Value>(line)
                    .unwrap_or_else(|e| panic!("{}: bad transcript line {line}: {e}", path.display()));
                if event["dir"] == "err" {
                    server_stderr.push_str(event["data"].as_str().unwrap_or(""));
                }
            }
        }
    }
    Run { status: out.status.code().unwrap_or(-1), verdicts, stdout, server_stderr, took }
}

fn run(standin_args: &[&str], lspcheck_args: &[&str]) -> Run {
    let server: Vec<&str> = std::iter::once(STANDIN).chain(standin_args.iter().copied()).collect();
    run_command(lspcheck_args, &server, None)
}

fn fault(name: &str) -> Run {
    run(&["--fault", name], &[])
}

/// Runs a fault whose effect depends on where the server's reads happen to
/// end, until the stand-in says on stderr that it took effect. A stand-in
/// descheduled for the whole of a split write reads it in one piece, so the
/// fault can't act; that run says nothing either way and is repeated.
fn fault_taking_effect(name: &str, marker: &str) -> Run {
    for _ in 0..5 {
        let r = fault(name);
        if r.server_stderr.contains(marker) {
            return r;
        }
    }
    panic!("the {name} fault never took effect in 5 runs");
}

fn expect(run: &Run, id: &str, verdict: &str) {
    assert_eq!(run.verdicts.get(id).map(String::as_str), Some(verdict), "{id}\n{}", run.stdout);
}

/// Every check except `failing` passes.
fn only_fails(run: &Run, failing: &[&str]) {
    for (id, verdict) in &run.verdicts {
        if id == "L9" && !cfg!(unix) {
            continue;
        }
        let want = if failing.contains(&id.as_str()) { "fail" } else { "pass" };
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
fn lawful_variations_fail_nothing() {
    for name in ["extension-header", "decimal-ids", "long-header", "answers-reordered", "slow-reads", "no-open-close"] {
        let r = fault(name);
        only_fails(&r, &[]);
        assert_eq!(r.status, 0, "{name}\n{}", r.stdout);
    }
}

#[test]
fn stdout_output_outside_frames_fails_f1_in_any_scenario() {
    let r = fault("stdout-banner");
    only_fails(&r, &["F1"]);
    assert_eq!(r.status, 1, "F1 is a MUST check");
    assert!(r.stdout.contains("standin starting"), "{}", r.stdout);
    // Only the pre-initialize scenario sees this one.
    let r = fault("preinit-banner");
    only_fails(&r, &["F1"]);
    assert!(r.stdout.contains("in pre-initialize"), "{}", r.stdout);
}

#[test]
fn a_control_byte_in_a_header_fails_f1() {
    only_fails(&fault("nul-header"), &["F1"]);
}

#[test]
fn a_content_type_that_isnt_a_media_type_fails_f1() {
    only_fails(&fault("bad-content-type"), &["F1"]);
}

#[test]
fn output_written_just_before_exit_is_still_judged() {
    // The bad line follows 100,000 frames that are still queued when the
    // server exits; F1 must see it.
    // The server writes it all after exit arrives, and a slow runner may take
    // a while to read it, so the exit allowance is generous.
    let r = run(&["--fault", "exit-flood"], &["--exit-timeout", "30"]);
    expect(&r, "F1", "fail");
    assert!(r.stdout.contains("goodbye"), "{}", r.stdout);
}

#[test]
fn a_failure_seen_directly_stands_when_other_frames_were_skipped() {
    // shutdown's wrong result was read; a skipped frame elsewhere can't explain it.
    let r = run(&["--fault", "shutdown-result", "--fault", "big-frame"], &["--max-frame-mib", "1"]);
    expect(&r, "L4", "fail");
    expect(&r, "F1", "error");
    assert_eq!(r.status, 1, "{}", r.stdout);
}

#[test]
fn a_frame_skipped_after_a_missed_reply_doesnt_excuse_it() {
    // The $/ request goes unanswered; the oversized frame comes only on exit,
    // long after lspcheck stopped waiting, so it can't have held the reply.
    let r = run(&["--fault", "ignore-dollar-request", "--fault", "big-frame-on-exit"], &["--max-frame-mib", "1"]);
    expect(&r, "J4", "fail");
    expect(&r, "F1", "error");
}

#[test]
fn requests_lspcheck_couldnt_send_are_errors_not_failures() {
    // The server stops reading while lspcheck answers its request, so
    // lspcheck can't deliver the requests J4 and L4 depend on.
    let r = run(&["--fault", "block-after-init"], &["--timeout", "1"]);
    expect(&r, "L2", "pass");
    for id in ["J4", "L4", "L6"] {
        expect(&r, id, "error");
    }
    // So can the plain request the framing checks start with, which can't
    // then tell whether the server answers requests.
    for id in ["F3", "F4", "F5", "F6", "F7"] {
        expect(&r, id, "error");
    }
    // exit reached its pipe there, and the server never read it: that's real.
    expect(&r, "L7", "fail");
    assert_eq!(r.status, 2, "no MUST failure\n{}", r.stdout);
}

#[test]
fn input_split_across_writes_must_be_read() {
    // A reader that expects each header section in one read.
    only_fails(&fault_taking_effect("split-header", "split-header dropped"), &["F3"]);
    // A reader that decodes each read on its own, so a character cut in two is mangled.
    let r = fault_taking_effect("split-utf8", "split-utf8 mangled");
    only_fails(&r, &["F3"]);
    assert!(r.stdout.contains("cut inside them"), "{}", r.stdout);
}

#[test]
fn frames_coalesced_in_one_write_must_all_be_read() {
    let r = fault("first-frame-per-read");
    only_fails(&r, &["F4"]);
    assert!(r.stdout.contains("1 of 2 requests went unanswered"), "{}", r.stdout);
}

#[test]
fn counting_incoming_characters_instead_of_bytes_fails_f5() {
    let r = fault("char-length-input");
    only_fails(&r, &["F5"]);
    // F3's multibyte part depends on F5, and says it wasn't run.
    assert!(r.stdout.contains("wasn't run, because F5 didn't pass"), "{}", r.stdout);
}

#[test]
fn rejecting_a_client_content_type_fails_f6_and_f7() {
    only_fails(&fault("no-content-type"), &["F6", "F7"]);
    let r = fault("strict-charset");
    only_fails(&r, &["F7"]);
    assert_eq!(r.status, 0, "F7 is a SHOULD check");
}

#[test]
fn variants_after_the_server_stops_answering_are_inconclusive() {
    // The server exits at F6; nothing after it can be judged.
    let r = fault("exit-on-content-type");
    expect(&r, "F4", "pass");
    expect(&r, "F6", "fail");
    for id in ["F7", "F3", "F5"] {
        expect(&r, id, "error");
    }
    assert!(r.stdout.contains("the server exited after F6 didn't pass"), "{}", r.stdout);
}

#[test]
fn framing_variants_are_not_exercised_when_plain_requests_go_unanswered() {
    let r = run(&["--fault", "ignore-unknown-requests"], &["--timeout", "2"]);
    expect(&r, "J4", "fail");
    for id in ["F3", "F4", "F5", "F6", "F7"] {
        expect(&r, id, "not-exercised");
    }
}

#[test]
fn a_non_utf8_charset_fails_f8() {
    let r = fault("latin1");
    only_fails(&r, &["F8"]);
    assert_eq!(r.status, 0, "F8 is a SHOULD check");
}

#[test]
fn answering_before_initialize_fails_l1() {
    let r = fault("no-pre-init-error");
    only_fails(&r, &["L1"]);
    assert_eq!(r.status, 0, "L1 is a SHOULD check");
}

#[test]
fn strict_turns_should_failures_into_status_1() {
    assert_eq!(run(&["--fault", "no-pre-init-error"], &["--strict"]).status, 1);
}

#[test]
fn bad_initialize_results_fail_l2() {
    only_fails(&fault("bad-encoding"), &["L2"]);
    only_fails(&fault("no-capabilities"), &["L2"]);
}

#[test]
fn a_refused_initialize_is_an_error_not_a_failure() {
    let r = fault("init-error");
    expect(&r, "L2", "error");
    assert!(r.verdicts.values().all(|v| v != "fail"), "{}", r.stdout);
    assert_eq!(r.status, 2);
    assert!(r.stdout.contains("Required SDK is unavailable"), "{}", r.stdout);
}

#[test]
fn replies_sent_before_their_requests_dont_count() {
    only_fails(&fault("early-replies"), &["J4"]);
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
    // lspcheck had to stop it in every scenario, so cleanup is untested rather than passed.
    if cfg!(unix) {
        expect(&r, "L9", "not-exercised");
    }
}

#[cfg(unix)]
#[test]
fn an_orphaned_child_fails_l9() {
    let r = fault("orphan-child");
    only_fails(&r, &["L9"]);
    assert!(r.stdout.contains("lspcheck-stand"), "names the process:\n{}", r.stdout);
}

#[cfg(target_os = "linux")]
#[test]
fn a_child_that_leaves_the_group_with_a_non_utf8_name_still_fails_l9() {
    let r = fault("escaped-child");
    only_fails(&r, &["L9"]);
    assert!(r.stdout.contains("\u{fffd}standin"), "{}", r.stdout);
}

#[test]
fn a_server_that_stops_reading_cant_stall_lspcheck() {
    let r = fault("block-on-config");
    expect(&r, "L2", "error");
    // Three scenarios, each bounded by the 5 s startup timeout plus cleanup
    // (a few seconds); unbounded, it would hit the watchdog.
    assert!(r.took < Duration::from_secs(60), "took {:?}\n{}", r.took, r.stdout);
}

#[test]
fn output_past_the_budget_is_inconclusive_not_a_failure() {
    let r = run(&["--fault", "flood"], &["--max-output-mib", "1"]);
    assert!(r.verdicts.values().all(|v| v != "fail"), "{}", r.stdout);
    expect(&r, "F1", "error");
    assert_eq!(r.status, 2);
    // Within budget, the same server passes.
    only_fails(&fault("flood"), &[]);
}

#[test]
fn a_frame_past_the_inspection_limit_is_skipped_not_failed() {
    let r = run(&["--fault", "big-frame"], &["--max-frame-mib", "1"]);
    expect(&r, "F1", "error");
    expect(&r, "L2", "pass");
    assert!(r.verdicts.values().all(|v| v != "fail"), "{}", r.stdout);
}

#[test]
fn a_relative_server_path_is_resolved_before_entering_the_workspace() {
    let path = std::path::Path::new(STANDIN);
    let dir = path.parent().unwrap();
    let rel = format!("./{}", path.file_name().unwrap().to_str().unwrap());
    let r = run_command(&["--only", "L2"], &[&rel], Some(dir));
    expect(&r, "L2", "pass");
}

#[test]
fn only_runs_the_named_checks() {
    let r = run(&[], &["--only", "L4,L7"]);
    assert_eq!(r.verdicts.keys().cloned().collect::<Vec<_>>(), ["L4", "L7"]);
}

#[test]
fn only_a_cross_cutting_check_still_runs_every_scenario() {
    let r = run(&["--fault", "preinit-banner"], &["--only", "F1"]);
    expect(&r, "F1", "fail");
}

#[test]
fn a_missing_server_is_an_error_not_a_failure() {
    let r = run_command(&[], &["/nonexistent/lspcheck-no-such-server"], None);
    assert_eq!(r.status, 2);
    assert!(r.verdicts.values().all(|v| v == "error"), "{}", r.stdout);
}

#[test]
fn unreadable_frames_make_other_failures_inconclusive() {
    // Every frame has a NUL in a header, so no reply can be read: F1 fails,
    // and what that hides is an error, not a failure.
    let r = fault("all-nul-headers");
    expect(&r, "F1", "fail");
    for (id, v) in &r.verdicts {
        if id != "F1" {
            assert_ne!(v, "fail", "{id}\n{}", r.stdout);
        }
    }
}
