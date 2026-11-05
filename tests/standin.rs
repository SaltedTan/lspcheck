// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! Runs the lspcheck binary against the stand-in server: clean, every check
//! passes; with each fault, the check that exists to catch it fails, and
//! lawful-but-unusual behaviour fails nothing.

#![cfg(feature = "standin")]

use lspcheck::message::{Kind, Message};
use lspcheck::transport::{Chunk, FrameReader};
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
    run_command_in(lspcheck_args, server, cwd, dir.path().join("t"))
}

/// The same, with the transcripts directory chosen by the caller.
fn run_command_in(
    lspcheck_args: &[&str],
    server: &[&str],
    cwd: Option<&std::path::Path>,
    transcripts: std::path::PathBuf,
) -> Run {
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
        .arg(&transcripts)
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
    if let Ok(entries) = std::fs::read_dir(&transcripts) {
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

/// Like [`run`], but keeps the transcripts directory alive and returns it.
fn run_with_transcripts(standin_args: &[&str], lspcheck_args: &[&str]) -> (Run, std::path::PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let server: Vec<&str> = std::iter::once(STANDIN).chain(standin_args.iter().copied()).collect();
    let t = dir.path().join("t");
    (run_command_in(lspcheck_args, &server, None, t.clone()), t, dir)
}

/// Runs `lspcheck replay <transcript> [args] -- <server>` with a watchdog.
fn replay(transcript: &std::path::Path, lspcheck_args: &[&str], server: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(LSPCHECK);
    // The subcommand comes first: top-level flags don't parse before it.
    cmd.arg("replay").arg(transcript);
    for (flag, value) in [("--timeout", "5"), ("--exit-timeout", "5")] {
        if !lspcheck_args.contains(&flag) {
            cmd.args([flag, value]);
        }
    }
    cmd.args(lspcheck_args).arg("--").args(server).stdout(Stdio::piped()).stderr(Stdio::piped());
    let started = Instant::now();
    let mut child = cmd.spawn().unwrap();
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if started.elapsed() > WATCHDOG {
            let _ = child.kill();
            panic!("replay ran longer than {WATCHDOG:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The workspace a transcript was recorded in.
fn recorded_workspace(transcript: &std::path::Path) -> std::path::PathBuf {
    for line in std::fs::read_to_string(transcript).unwrap().lines() {
        let v: Value = serde_json::from_str(line).unwrap();
        if let Some(p) = v["workspace"].as_str() {
            return std::path::PathBuf::from(p);
        }
    }
    panic!("no workspace line in {}", transcript.display());
}

fn recorded_request_count(transcript: &std::path::Path) -> usize {
    let text = std::fs::read_to_string(transcript).unwrap();
    let mut reader = FrameReader::default();
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["dir"] == "out")
        .filter_map(|event| {
            event["data"]
                .as_str()
                .map(|s| s.as_bytes().to_vec())
                .or_else(|| event["data_base64"].as_str().and_then(lspcheck::transcript::base64_decode))
        })
        .flat_map(|bytes| reader.feed(&bytes))
        .filter_map(|chunk| match chunk {
            Chunk::Frame { body, .. } => Message::parse(&body).ok(),
            _ => None,
        })
        .filter(|message| matches!(message.kind, Kind::Request { .. }))
        .count()
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
    let expected: Vec<(&str, &str)> = failing.iter().map(|id| (*id, "fail")).collect();
    passes_except(run, &expected);
}

/// Every check passes except those listed, which get the verdict given.
fn passes_except(run: &Run, expected: &[(&str, &str)]) {
    for (id, verdict) in &run.verdicts {
        if id == "L9" && !cfg!(unix) {
            continue;
        }
        let want = expected.iter().find(|(e, _)| e == id).map_or("pass", |(_, v)| v);
        assert_eq!(verdict, want, "{id}\n{}", run.stdout);
    }
}

/// Like [`only_fails`], for a server that sends frames lspcheck can't read
/// (F1): such a frame might have been a response or message the correlation
/// and envelope checks judge, so J1, J2 and J5 are inconclusive.
fn only_fails_unread(run: &Run, failing: &[&str]) {
    for (id, verdict) in &run.verdicts {
        if id == "L9" && !cfg!(unix) {
            continue;
        }
        let want = if failing.contains(&id.as_str()) {
            "fail"
        } else if ["J1", "J2", "J5"].contains(&id.as_str()) {
            "error"
        } else {
            "pass"
        };
        assert_eq!(verdict, want, "{id}\n{}", run.stdout);
    }
}

#[test]
fn the_clean_standin_passes_every_check() {
    let r = run(&[], &[]);
    only_fails(&r, &[]);
    assert_eq!(r.status, 0, "{}", r.stdout);
    assert_eq!(r.verdicts.len(), lspcheck::checks::CATALOGUE.len());
    // D1, D2 and D3 passed on evidence, not for lack of it.
    assert!(r.stdout.contains("informative position(s) on character boundaries"), "{}", r.stdout);
    assert!(
        r.stdout.contains("versioned diagnostics notification(s) named a version lspcheck had sent"),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("informative position(s) on character boundaries after the change"), "{}", r.stdout);
}

#[test]
fn lawful_variations_fail_nothing() {
    for name in [
        "extension-header",
        "decimal-ids",
        "long-header",
        "answers-reordered",
        "slow-reads",
        "newline-after-body",
        "defer-until-next",
        "cancelled-other-error",
        "utf8-encoding",
        "past-line-end",
        "grapheme-split",
        "foreign-locations",
        "positions-in-data",
        "no-diagnostics",
        "unversioned-diagnostics",
        "full-sync",
        "stale-diagnostics",
        "stale-unversioned-diagnostics",
        "no-change-sync",
    ] {
        let r = fault(name);
        // With no versioned publish for the opened document, D2 has nothing
        // to judge and is not exercised instead of passing. A server that
        // takes no changes gives D3 none to judge, so it skips.
        let mut expected: Vec<(&str, &str)> = Vec::new();
        if matches!(name, "no-diagnostics" | "unversioned-diagnostics") {
            expected.push(("D2", "not-exercised"));
        }
        if name == "no-change-sync" {
            expected.push(("D3", "skip"));
        }
        passes_except(&r, &expected);
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
    only_fails_unread(&fault("nul-header"), &["F1"]);
}

#[test]
fn a_content_type_that_isnt_a_media_type_fails_f1() {
    only_fails_unread(&fault("bad-content-type"), &["F1"]);
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
fn messages_still_queued_at_exit_are_classified() {
    // 200,000 valid notifications, then one without "jsonrpc", are written
    // after exit. The last one is J5's failure even though it is still queued
    // when the server exits; a slow runner may take a while to read it all, so
    // the exit allowance is generous.
    let r = run(&["--fault", "exit-flood-bad-envelope"], &["--exit-timeout", "30"]);
    expect(&r, "J5", "fail");
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
fn a_content_length_counted_in_characters_fails_f2() {
    let r = fault("char-length");
    // The frame can't be read either, so F1 fails too.
    only_fails_unread(&r, &["F1", "F2"]);
    assert!(r.stdout.contains("counts characters, but"), "{}", r.stdout);
    // Also with whitespace after the JSON, or an extension header before the next frame's Content-Length.
    for other in ["newline-after-body", "extension-header", "long-header"] {
        let r = run(&["--fault", "char-length", "--fault", other], &[]);
        expect(&r, "F2", "fail");
        assert!(r.stdout.contains("counts characters, but"), "{other}\n{}", r.stdout);
    }
}

#[test]
fn a_correctly_counted_body_that_isnt_json_fails_f1_not_f2() {
    let r = fault("trailing-junk");
    only_fails_unread(&r, &["F1"]);
}

#[test]
fn a_body_the_next_header_line_would_complete_fails_f1_not_f2() {
    // Joined to the next frame's first header line, the body would be JSON,
    // but the next header section starts where the body was declared to end.
    only_fails_unread(&fault("json-into-next-header"), &["F1"]);
    // Even when that header line isn't valid.
    only_fails_unread(&fault("json-into-bad-header"), &["F1"]);
}

#[test]
fn wrong_lengths_that_count_neither_characters_nor_code_units_fail_f1_not_f2() {
    // Content-Length 1 for a long body, and one 10,000 bytes too long on the last frame.
    for name in ["long-miscount", "overlong-last-frame"] {
        let r = fault(name);
        expect(&r, "F1", "fail");
        expect(&r, "F2", "pass");
    }
}

#[test]
fn every_unreadable_body_is_examined_for_f2() {
    // 70 unreadable bodies come before the miscounted ones in each scenario.
    let r = run(&["--fault", "bad-bodies", "--fault", "char-length"], &[]);
    expect(&r, "F2", "fail");
}

#[test]
fn escaped_non_ascii_output_leaves_f2_not_exercised() {
    let r = fault("ascii-escape");
    expect(&r, "F2", "not-exercised");
    for (id, v) in &r.verdicts {
        if id != "F2" && !(id == "L9" && !cfg!(unix)) {
            assert_eq!(v, "pass", "{id}\n{}", r.stdout);
        }
    }
    assert_eq!(r.status, 0, "{}", r.stdout);
}

#[test]
fn a_server_lost_on_the_f2_probe_doesnt_fail_what_follows() {
    // The probe for F2's evidence has a string id, and this server exits on
    // it. F3's last part comes after it, and isn't run. The same exit leaves
    // the concurrent string-id requests unanswered, so J6 fails: that is the
    // rule this server breaks, not an artefact of what follows.
    let r = fault("exit-on-string-id");
    expect(&r, "F2", "not-exercised");
    expect(&r, "F3", "pass");
    expect(&r, "J1", "not-exercised");
    expect(&r, "J6", "fail");
    assert!(r.stdout.contains("the part that splits multibyte characters wasn't run"), "{}", r.stdout);
    assert!(r.verdicts.iter().all(|(id, v)| v != "fail" || id == "J6"), "{}", r.stdout);
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
    // Even when slow reads make the server read each frame separately, the
    // fault must act on F4's shape only: `initialized` coalescing with J6's
    // requests isn't a read of three frames with $/setTrace in it.
    only_fails(&run(&["--fault", "first-frame-per-read", "--fault", "slow-reads"], &[]), &["F4"]);
}

#[test]
fn counting_incoming_characters_instead_of_bytes_fails_f5() {
    let r = fault("char-length-input");
    expect(&r, "F5", "fail");
    // The probe with non-ASCII output for F2 isn't sent to a server that can't read it.
    // Nor can it read D1's document, whose emoji put its input out of step:
    // no position or versioned diagnostics come back, which says nothing
    // about how it counts them.
    passes_except(
        &r,
        &[
            ("F5", "fail"),
            ("F2", "not-exercised"),
            ("D1", "not-exercised"),
            ("D2", "not-exercised"),
            ("D3", "not-exercised"),
        ],
    );
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
fn output_skipped_after_a_missed_answer_doesnt_excuse_it() {
    // The request before initialize goes unanswered; the frame too big to
    // inspect comes only with the answer to initialize, after lspcheck
    // stopped waiting, so it can't have held the answer.
    let r = run(
        &["--fault", "ignore-pre-init-requests", "--fault", "big-frame"],
        &["--timeout", "2", "--max-frame-mib", "1"],
    );
    expect(&r, "L1", "fail");
}

#[test]
fn strict_turns_should_failures_into_status_1() {
    assert_eq!(run(&["--fault", "no-pre-init-error"], &["--strict"]).status, 1);
}

#[test]
fn bad_initialize_results_fail_l2() {
    // D1 can't count positions in an encoding lspcheck didn't offer, and D2
    // and D3 can't judge without it either; without capabilities the server
    // doesn't ask for didOpen, so all three are skipped.
    passes_except(&fault("bad-encoding"), &[("L2", "fail"), ("D1", "error"), ("D2", "error"), ("D3", "error")]);
    passes_except(&fault("no-capabilities"), &[("L2", "fail"), ("D1", "skip"), ("D2", "skip"), ("D3", "skip")]);
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
    // The guessed ids are never sent, so they are unasked responses (J2); the
    // fault also answers unknown requests wrongly (J4).
    only_fails(&fault("early-replies"), &["J2", "J4"]);
}

#[test]
fn wrong_unknown_method_errors_fail_j4() {
    only_fails(&fault("wrong-unknown-error"), &["J4"]);
    only_fails(&fault("ignore-dollar-request"), &["J4"]);
}

#[test]
fn a_string_id_answered_as_a_number_fails_j1() {
    let r = fault("string-id-to-number");
    // The concurrent request whose id was changed never gets a reply with its
    // own id, so J6 fails too: one fault, two rules.
    only_fails(&r, &["J1", "J6"]);
    assert!(r.stdout.contains("where lspcheck sent"), "{}", r.stdout);
}

#[test]
fn duplicate_and_unsent_responses_fail_j2() {
    only_fails(&fault("reply-twice"), &["J2"]);
    let r = fault("reply-unsent-id");
    only_fails(&r, &["J2"]);
    assert!(r.stdout.contains("which lspcheck never sent"), "{}", r.stdout);
}

#[test]
fn a_reply_to_a_notification_fails_j3() {
    let r = fault("reply-to-notification");
    only_fails(&r, &["J3"]);
    assert!(r.stdout.contains("a notification was answered"), "{}", r.stdout);
    // The replies were seen before the server stopped answering, so they
    // stand even though the probes that end J3's window go unanswered.
    let r = run(&["--fault", "reply-to-notification", "--fault", "exit-on-unknown-notification"], &[]);
    only_fails(&r, &["J3"]);
}

#[test]
fn a_skipped_reply_to_a_notification_makes_j3_inconclusive() {
    // The only reply to the unknown notifications is over the inspection
    // limit, so lspcheck can't say whether a notification was answered.
    let r = run(&["--fault", "big-reply-to-notification"], &["--max-frame-mib", "1"]);
    expect(&r, "J3", "error");
    assert!(r.verdicts.values().all(|v| v != "fail"), "{}", r.stdout);
}

#[test]
fn replies_to_exit_and_out_of_range_ids_are_judged() {
    // A reply to `exit` answers a notification too.
    only_fails(&fault("reply-to-exit"), &["J3"]);
    // LSP's integer is signed 32-bit, ids included.
    let r = fault("out-of-range-id");
    only_fails(&r, &["J5"]);
    assert!(r.stdout.contains("2147483648"), "{}", r.stdout);
}

/// The concurrent requests can't all be written by their deadline, because
/// the server stopped reading with a full pipe; that is lspcheck's problem,
/// not a missing reply. (The pipe arithmetic is Linux's.)
#[cfg(target_os = "linux")]
#[test]
fn concurrent_requests_that_cant_be_written_in_time_are_an_error() {
    // The server stops reading before `initialized` arrives, so `initialized`
    // and lspcheck's answer fill the pipe whatever the read grouping.
    for extra in [&[][..], &["--fault", "slow-reads"][..]] {
        let mut args = vec!["--fault", "fill-stdin-on-initialized"];
        args.extend_from_slice(extra);
        let r = run(&args, &["--timeout", "1"]);
        expect(&r, "J6", "error");
        assert!(r.stdout.contains("couldn't send a request"), "{}", r.stdout);
    }
}

#[test]
fn malformed_envelopes_fail_j5() {
    only_fails(&fault("no-jsonrpc-field"), &["J5"]);
    // These mangle the reply the $/ unknown request waits on, so J4 fails too:
    // a response with both result and error, or an error with a string code,
    // is not MethodNotFound either.
    only_fails(&fault("result-and-error"), &["J4", "J5"]);
    only_fails(&fault("string-error-code"), &["J4", "J5"]);
}

#[test]
fn dropping_a_concurrent_request_fails_j6() {
    // Protocol order decides this, not where the OS ended its reads: J6's batch
    // is the only one with an all-digit string id, and all four requests are
    // written before any reply is read, so the next frame is always there.
    let r = fault("drop-concurrent");
    only_fails(&r, &["J6"]);
    assert!(r.stdout.contains("were not answered"), "{}", r.stdout);
    assert!(r.server_stderr.contains("drop-concurrent dropped"), "{}", r.stdout);
}

#[test]
fn replying_to_a_notification_before_initialize_fails_l1() {
    for name in ["reply-to-pre-init-notification", "late-reply-to-pre-init-notification"] {
        let r = fault(name);
        only_fails(&r, &["L1"]);
        assert!(r.stdout.contains("a notification sent before initialize got a reply"), "{name}\n{}", r.stdout);
    }
}

#[test]
fn a_second_initialize_must_get_an_error_and_leave_the_server_serving() {
    only_fails(&fault("reinitialize"), &["L3"]);
    // Exiting fails L3, and leaves L5 unjudged rather than failed.
    let r = fault("exit-on-reinitialize");
    expect(&r, "L3", "fail");
    expect(&r, "L5", "error");
    assert!(r.verdicts.iter().all(|(id, v)| id == "L3" || v != "fail"), "{}", r.stdout);
}

#[test]
fn answering_requests_after_shutdown_fails_l5() {
    let r = fault("post-shutdown-results");
    only_fails(&r, &["L5"]);
    assert_eq!(r.status, 0, "L5 is a SHOULD check");
}

#[test]
fn staying_alive_after_the_end_of_input_fails_l8() {
    let r = fault("ignore-eof");
    only_fails(&r, &["L8"]);
    assert_eq!(r.status, 0, "L8 is a POLICY check");
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
    // It still exits when its input ends, so its cleanup was tested there.
    expect(&r, "L8", "pass");
    if cfg!(unix) {
        expect(&r, "L9", "pass");
    }
    // Ignoring the end of input too, lspcheck had to stop it in every
    // scenario, so cleanup is untested rather than passed.
    let r = run(&["--fault", "ignore-exit", "--fault", "ignore-eof"], &[]);
    expect(&r, "L8", "fail");
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
fn a_cancelled_request_still_gets_exactly_one_response() {
    // A deferred request that is cancelled is never answered.
    let r = fault("cancelled-no-reply");
    only_fails(&r, &["C1"]);
    assert!(r.stdout.contains("no response after $/cancelRequest"), "{}", r.stdout);
    // A cancelled request that gets both the error and a result was answered twice.
    // It is also a second response to one request, which is J2's rule.
    let r = fault("cancelled-reply-twice");
    only_fails(&r, &["C1", "J2"]);
    assert!(r.stdout.contains("answered twice"), "{}", r.stdout);
    // A result, -32800, or another error is lawful for what it contains.
    only_fails(&fault("defer-until-next"), &[]);
}

#[test]
fn cancelling_an_unknown_or_finished_request_produces_nothing() {
    // Holding R's reply keeps C1's cancel from reaching a finished request, so
    // these faults act only on C2's unknown id. That id was reserved by
    // `fresh_id` but never sent, so the reply also carries an id lspcheck never
    // sent: J2's rule, as well as C2's.
    let r = run(&["--fault", "defer-until-next", "--fault", "cancel-unknown-reply"], &[]);
    only_fails(&r, &["C2", "J2"]);
    assert!(r.stdout.contains("for an id never sent"), "{}", r.stdout);
    // A server that stops answering might still send a reply to a cancel
    // later, so C3 can't be judged.
    let r = run(&["--fault", "defer-until-next", "--fault", "stop-after-unknown-cancel"], &[]);
    expect(&r, "C2", "fail");
    expect(&r, "C3", "error");
    assert!(r.stdout.contains("stopped answering"), "{}", r.stdout);
}

#[test]
fn replying_to_a_cancel_notification_fails_c3() {
    // The reply has no id, so it also answers a notification (J3).
    only_fails(&fault("reply-to-cancel"), &["C3", "J3"]);
}

#[test]
fn replies_to_cancels_after_the_next_answer_still_count() {
    // A plain request's answer doesn't order what a server says about a
    // cancel, so replies sent after it are judged too.
    let late = |faults: &[&str]| {
        let mut args = vec!["--fault", "defer-until-next", "--fault", "late-cancel-replies"];
        args.extend(faults.iter().flat_map(|f| ["--fault", f]));
        run(&args, &[])
    };
    // The unknown id was reserved but never sent, so J2 fails too; an id-less
    // reply to a cancel also answers a notification, which is J3's rule.
    only_fails(&late(&["cancel-unknown-reply"]), &["C2", "J2"]);
    only_fails(&late(&["reply-to-cancel"]), &["C3", "J3"]);
    // A second answer to the cancelled request, read before the cancels of
    // C2 were sent, is C1's finding, not C2's (and a duplicate for J2).
    let r = late(&["cancelled-reply-twice"]);
    only_fails(&r, &["C1", "J2"]);
    assert!(r.stdout.contains("answered twice"), "{}", r.stdout);
}

#[test]
fn a_skipped_frame_makes_cancellation_passes_inconclusive() {
    // The oversized replies to the cancels are skipped, so lspcheck can't say
    // they weren't replies.
    let r = run(&["--fault", "reply-to-cancel", "--fault", "big-cancel-replies"], &["--max-frame-mib", "1"]);
    for id in ["C1", "C2", "C3"] {
        expect(&r, id, "error");
    }
}

#[test]
fn c3_counts_only_id_less_replies_read_after_the_first_cancel() {
    // A reply to `initialized` read before any cancel was sent can't answer
    // one. If it is read only after the first cancel was written, it might
    // have answered either, so C3 can't be judged, but it doesn't fail.
    let r = fault("null-reply-after-first-probe");
    assert_ne!(r.verdicts["C3"], "fail", "{}", r.stdout);
    // It is read after `initialized` and before `exit`, so it answers a
    // notification, which J3 judges across the scenarios with well-formed
    // input.
    expect(&r, "J3", "fail");
    let r = fault("initialized-reply-after-cancel");
    expect(&r, "C3", "error");
    expect(&r, "J3", "fail");
    assert!(
        r.verdicts.iter().all(|(id, v)| v == "pass" || id == "C3" || id == "J3" || (id == "L9" && !cfg!(unix))),
        "{}",
        r.stdout
    );
    // A malformed answer to shutdown still carries shutdown's id.
    expect(&fault("shutdown-no-jsonrpc"), "C3", "pass");
}

#[test]
fn a_frame_skipped_during_c1_leaves_c2_judged() {
    // The oversized log comes right after the cancelled request's answer, so
    // it is read before C2's cancels and can't be a reply to them.
    let r = run(&["--fault", "defer-until-next", "--fault", "big-frame-on-cancel"], &["--max-frame-mib", "1"]);
    expect(&r, "C1", "error");
    expect(&r, "C2", "pass");
    expect(&r, "C3", "error");
}

#[test]
fn a_frame_skipped_before_the_cancelled_request_doesnt_excuse_its_absence() {
    let r =
        run(&["--fault", "big-frame", "--fault", "cancelled-no-reply"], &["--max-frame-mib", "1", "--timeout", "3"]);
    expect(&r, "C1", "fail");
}

#[test]
fn a_frame_skipped_after_a_cancelled_request_went_unanswered_doesnt_excuse_it() {
    // The oversized frame comes only after lspcheck stopped waiting for the
    // cancelled request, so it can't have been its answer.
    let r = run(
        &["--fault", "cancelled-no-reply", "--fault", "big-frame-after-drop"],
        &["--max-frame-mib", "1", "--timeout", "3"],
    );
    expect(&r, "C1", "fail");
    // It is read before C2's cancels, so C2 is still judged.
    expect(&r, "C2", "pass");
}

#[test]
fn a_server_that_stops_reading_cant_stall_lspcheck() {
    let r = run(&["--fault", "block-on-config"], &["--startup-timeout", "2"]);
    expect(&r, "L2", "error");
    // Every scenario is bounded by the 2 s startup timeout plus cleanup (a
    // second or two); unbounded, it would hit the watchdog.
    let bound = Duration::from_secs(5) * lspcheck::checks::SCENARIOS.len() as u32;
    assert!(r.took < bound, "took {:?}, more than {bound:?}\n{}", r.took, r.stdout);
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

#[test]
fn a_server_that_doesnt_open_documents_skips_d1() {
    let r = fault("no-open-close");
    passes_except(&r, &[("D1", "skip"), ("D2", "skip"), ("D3", "skip")]);
    assert_eq!(r.status, 0, "{}", r.stdout);
}

#[test]
fn positions_counted_in_the_wrong_units_fail_d1() {
    for (name, diagnosis) in [
        ("utf8-positions-as-utf16", "on a boundary if counted in UTF-8 bytes"),
        ("codepoint-positions", "on a boundary if counted in code points"),
    ] {
        let r = fault(name);
        only_fails(&r, &["D1"]);
        assert_eq!(r.status, 1, "D1 is a MUST check");
        assert!(r.stdout.contains("inside a character of version 41"), "{name}\n{}", r.stdout);
        assert!(r.stdout.contains(diagnosis), "{name}\n{}", r.stdout);
    }
    // Counting UTF-16 units under UTF-8: the positions after the change are
    // invalid too, but each fits the changed text counted in UTF-16, so D3
    // can't tell a change-handling fault from a counting fault and is an
    // error; D1 carries the failure.
    let r = fault("utf16-positions-as-utf8");
    passes_except(&r, &[("D1", "fail"), ("D3", "error")]);
    assert_eq!(r.status, 1, "D1 is a MUST check");
    assert!(r.stdout.contains("inside a character of version 41"), "{}", r.stdout);
    assert!(r.stdout.contains("on a boundary if counted in UTF-16 code units"), "{}", r.stdout);
    assert!(r.stdout.contains("can't tell a change-handling fault from a counting fault; see D1"), "{}", r.stdout);
}

#[test]
fn a_change_the_server_doesnt_apply_fails_d1_and_d3() {
    // The old text's positions, labelled with the new version, are inside
    // characters of the changed text, and the positions before the change
    // were seen on boundaries.
    let r = fault("ignore-changes");
    only_fails(&r, &["D1", "D3"]);
    assert_eq!(r.status, 1, "D1 and D3 are MUST checks");
    assert!(r.stdout.contains("the positions before the change were all on boundaries"), "{}", r.stdout);
}

#[test]
fn change_ranges_counted_in_code_points_fail_d1_and_d3() {
    // The stand-in replaces two emoji where lspcheck's range names one, so
    // its `word` and line ends land inside characters of the changed text.
    let r = fault("codepoint-changes");
    only_fails(&r, &["D1", "D3"]);
    assert_eq!(r.status, 1, "D1 and D3 are MUST checks");
}

#[test]
fn diagnostics_of_the_previous_text_labelled_with_the_new_version_fail_d1_and_d3() {
    let r = fault("mislabelled-diagnostics");
    only_fails(&r, &["D1", "D3"]);
    assert_eq!(r.status, 1, "D1 and D3 are MUST checks");
}

#[test]
fn a_dynamic_registration_of_the_sync_notifications_errors_d3() {
    // lspcheck declares no dynamic registration but answers it with success,
    // so the server may expect another sync kind: D3 is an error, not a
    // failure, and nothing else is affected.
    let r = fault("register-sync");
    passes_except(&r, &[("D3", "error")]);
    assert_eq!(r.status, 2, "D3's error is reported");
    assert!(r.stdout.contains("registered textDocument/didChange or didOpen dynamically"), "{}", r.stdout);
}

#[test]
fn positions_in_a_duplicate_response_are_judged_too() {
    // The first answer is valid; the second, a duplicate (J2's finding),
    // has a position inside a character, read after the change. The
    // pre-change positions (the first answer) were valid, so D3's rule (a)
    // holds and D3 fails too: the position is invalid in the changed text.
    only_fails(&fault("invalid-duplicate-symbols"), &["D1", "J2", "D3"]);
}

#[test]
fn utf32_positions_leave_d1_not_exercised() {
    // No offset can fall inside a character in UTF-32.
    let r = fault("utf32-encoding");
    passes_except(&r, &[("D1", "not-exercised"), ("D3", "not-exercised")]);
    assert!(r.stdout.contains("the server chose utf-32"), "{}", r.stdout);
}

#[test]
fn a_frame_skipped_after_didopen_makes_a_d1_pass_inconclusive() {
    // The oversized log follows the diagnostics, so it might have held
    // positions; D1 can't pass.
    let r = run(&["--fault", "big-frame-on-open"], &["--max-frame-mib", "1"]);
    expect(&r, "D1", "error");
    // A position seen inside a character still fails.
    let r = run(&["--fault", "big-frame-on-open", "--fault", "codepoint-positions"], &["--max-frame-mib", "1"]);
    expect(&r, "D1", "fail");
    // Within the limit, the same server passes.
    only_fails(&fault("big-frame-on-open"), &[]);
}

#[test]
fn diagnostics_versions_the_document_never_had_fail_d2() {
    // The stand-in labels diagnostics with its own counter instead of the
    // document's version; D1 still judges the positions against version 41,
    // since an unknown label makes every version sent a candidate.
    let r = fault("diagnostics-own-version");
    only_fails(&r, &["D2"]);
    assert_eq!(r.status, 1, "D2 is a MUST check");
    assert!(r.stdout.contains("carries version 1,"), "{}", r.stdout);
    assert!(r.stdout.contains("lspcheck had sent 41"), "{}", r.stdout);
}

#[test]
fn a_diagnostics_version_beyond_the_document_fails_d2() {
    // One past the version lspcheck sent is not a version it had either.
    let r = fault("future-diagnostics-version");
    only_fails(&r, &["D2"]);
    assert_eq!(r.status, 1, "D2 is a MUST check");
    assert!(r.stdout.contains("carries version 42,"), "{}", r.stdout);
    assert!(r.stdout.contains("lspcheck had sent 41"), "{}", r.stdout);
}

#[test]
fn diagnostics_without_a_version_leave_d2_not_exercised() {
    // No version at all is lawful, and gives D2 nothing to judge.
    let r = fault("unversioned-diagnostics");
    expect(&r, "D2", "not-exercised");
    assert!(r.stdout.contains("2 without a version, which is lawful"), "{}", r.stdout);
    // Nor is there anything when the server never publishes.
    expect(&fault("no-diagnostics"), "D2", "not-exercised");
}

#[test]
fn a_version_for_a_uri_lspcheck_never_opened_isnt_judged() {
    // The foreign publish carries a version lspcheck never sent (999); it
    // names no document lspcheck opened, so D2 passes on the opened one.
    let r = fault("foreign-locations");
    expect(&r, "D2", "pass");
    assert!(
        r.stdout.contains("versioned diagnostics notification(s) named a version lspcheck had sent"),
        "{}",
        r.stdout
    );
}

// --- replay -----------------------------------------------------------------

#[test]
fn replay_reproduces_a_clean_recording() {
    let (r, t, _keep) = run_with_transcripts(&[], &[]);
    assert_eq!(r.status, 0, "{}", r.stdout);
    let started = Instant::now();
    let out = replay(&t.join("lifecycle.jsonl"), &["--timeout", "30"], &[STANDIN]);
    assert_eq!(out.status.code().unwrap_or(-1), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(started.elapsed() < Duration::from_secs(20), "replay took {:?}", started.elapsed());
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Every recorded reply was reproduced: nothing went unanswered.
    assert!(!stdout.contains("no reply"), "{stdout}");
    assert!(stdout.contains("result -> result"), "{stdout}");
    assert!(stdout.contains("error -32601 -> error -32601"), "{stdout}");
    assert!(stdout.contains("status 0 -> status 0"), "{stdout}");
    let replay_transcript = std::fs::read_to_string(t.join("lifecycle.replay.jsonl")).unwrap();
    assert!(!replay_transcript.contains("no reply to id"), "{replay_transcript}");
    // The replay's own transcript is written next to the input by default.
    assert!(t.join("lifecycle.replay.jsonl").exists());
}

#[test]
fn replay_shows_a_missing_reply_against_a_faulty_server() {
    let (r, t, _keep) = run_with_transcripts(&[], &[]);
    assert_eq!(r.status, 0, "{}", r.stdout);
    let out = replay(&t.join("lifecycle.jsonl"), &[], &[STANDIN, "--fault", "ignore-unknown-requests"]);
    assert_eq!(out.status.code().unwrap_or(-1), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The unknown requests got errors in the recording and nothing in the replay.
    assert!(stdout.contains("error -32601 -> no reply"), "{stdout}");
    // The requests the faulty server does answer are still reproduced.
    assert!(stdout.contains("result -> result"), "{stdout}");
}

#[test]
fn replay_recreates_and_removes_the_workspace() {
    let (r, t, _keep) = run_with_transcripts(&[], &[]);
    assert_eq!(r.status, 0, "{}", r.stdout);
    let ws = recorded_workspace(&t.join("framing.jsonl"));
    assert!(!ws.exists(), "the original workspace is gone");
    let out = replay(&t.join("framing.jsonl"), &[], &[STANDIN]);
    assert_eq!(out.status.code().unwrap_or(-1), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("recreated, removed"), "{stdout}");
    assert!(!ws.exists(), "the replay removes the workspace it recreated");
}

#[test]
fn replay_of_the_documents_scenario_recreates_its_document() {
    let (r, t, _keep) = run_with_transcripts(&[], &[]);
    assert_eq!(r.status, 0, "{}", r.stdout);
    let transcript = t.join("documents.jsonl");
    // doc.txt is written after the scenario starts, so a later workspace
    // line records it.
    let last = std::fs::read_to_string(&transcript)
        .unwrap()
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["workspace"].is_string())
        .expect("a workspace line");
    assert!(last["files"]["doc.txt"].as_str().is_some_and(|t| !t.is_empty()), "{last}");
    let ws = recorded_workspace(&transcript);
    let out = replay(&transcript, &[], &[STANDIN]);
    assert_eq!(out.status.code().unwrap_or(-1), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("recreated, removed"), "{stdout}");
    assert!(stdout.contains("result -> result"), "{stdout}");
    assert!(!stdout.contains("no reply"), "{stdout}");
    assert!(!ws.exists(), "the replay removes the workspace it recreated");

    let second = replay(&t.join("documents.replay.jsonl"), &[], &[STANDIN]);
    assert_eq!(second.status.code().unwrap_or(-1), 0, "{}", String::from_utf8_lossy(&second.stderr));
    let second_transcript = std::fs::read_to_string(t.join("documents.replay.replay.jsonl")).unwrap();
    let workspace_line = second_transcript
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|event| event["workspace"].is_string())
        .expect("replay records its recreated workspace");
    assert!(workspace_line["files"]["doc.txt"].as_str().is_some_and(|text| !text.is_empty()), "{workspace_line}");
}

#[test]
fn replay_refuses_a_non_empty_workspace() {
    let (r, t, _keep) = run_with_transcripts(&[], &[]);
    assert_eq!(r.status, 0, "{}", r.stdout);
    let ws = recorded_workspace(&t.join("framing.jsonl"));
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("not-recorded.txt"), "x").unwrap();
    let out = replay(&t.join("framing.jsonl"), &[], &[STANDIN]);
    assert_eq!(out.status.code().unwrap_or(-1), 2, "{}", String::from_utf8_lossy(&out.stdout));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(ws.to_str().unwrap()), "{stderr}");
    std::fs::remove_dir_all(&ws).unwrap();
}

#[test]
fn replay_refuses_to_overwrite_its_input() {
    let (r, t, _keep) = run_with_transcripts(&[], &[]);
    assert_eq!(r.status, 0, "{}", r.stdout);
    let input = t.join("lifecycle.jsonl");
    let out = replay(&input, &["--transcript", input.to_str().unwrap()], &[STANDIN]);
    assert_eq!(out.status.code().unwrap_or(-1), 2, "{}", String::from_utf8_lossy(&out.stdout));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("already exists"), "{stderr}");
    assert!(stderr.contains("--transcript"), "{stderr}");
    assert!(std::fs::read_to_string(&input).unwrap().contains("\"dir\":\"out\""));
}

#[test]
fn replay_refuses_an_existing_output_without_changing_it() {
    let (r, t, _keep) = run_with_transcripts(&[], &[]);
    assert_eq!(r.status, 0, "{}", r.stdout);
    let input = t.join("lifecycle.jsonl");
    let output = t.join("chosen-output.jsonl");
    std::fs::write(&output, "preserve me").unwrap();
    let result = replay(&input, &["--transcript", output.to_str().unwrap()], &[STANDIN]);
    assert_eq!(result.status.code().unwrap_or(-1), 2);
    assert_eq!(std::fs::read_to_string(output).unwrap(), "preserve me");
    assert!(String::from_utf8_lossy(&result.stderr).contains("already exists"));
}

#[cfg(unix)]
#[test]
fn replay_refuses_a_hard_link_to_its_input_as_output() {
    let (r, t, _keep) = run_with_transcripts(&[], &[]);
    assert_eq!(r.status, 0, "{}", r.stdout);
    let input = t.join("lifecycle.jsonl");
    let before = std::fs::read(&input).unwrap();
    let alias = t.join("input-hard-link.jsonl");
    std::fs::hard_link(&input, &alias).unwrap();
    let result = replay(&input, &["--transcript", alias.to_str().unwrap()], &[STANDIN]);
    assert_eq!(result.status.code().unwrap_or(-1), 2);
    assert_eq!(std::fs::read(&input).unwrap(), before);
    assert!(String::from_utf8_lossy(&result.stderr).contains("already exists"));
}

#[test]
fn replay_does_not_send_the_next_write_before_a_skipped_step_gate() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("gate.jsonl");
    let request = lspcheck::transport::encode(br#"{"jsonrpc":"2.0","id":1,"method":"m"}"#);
    let response = lspcheck::transport::encode(br#"{"jsonrpc":"2.0","id":1,"result":null}"#);
    let server_answer = lspcheck::transport::encode(br#"{"jsonrpc":"2.0","id":"cfg","result":[]}"#);
    let exit = lspcheck::transport::encode(br#"{"jsonrpc":"2.0","method":"exit"}"#);
    let lines = [
        serde_json::json!({"t_ms":0,"dir":"out","data":String::from_utf8(request).unwrap()}).to_string(),
        serde_json::json!({"t_ms":10,"dir":"in","data":String::from_utf8(response).unwrap()}).to_string(),
        serde_json::json!({"t_ms":20,"dir":"out","data":String::from_utf8(server_answer).unwrap()}).to_string(),
        serde_json::json!({"t_ms":320,"dir":"out","data":String::from_utf8(exit).unwrap()}).to_string(),
        serde_json::json!({"t_ms":330,"dir":"meta","event":"exited","how":"status 0"}).to_string(),
    ];
    std::fs::write(&input, lines.join("\n")).unwrap();
    let output = replay(&input, &[], &[STANDIN, "--fault", "replay-gate"]);
    assert_eq!(output.status.code().unwrap_or(-1), 0, "{}", String::from_utf8_lossy(&output.stderr));
    let summary = String::from_utf8_lossy(&output.stdout);
    assert!(summary.contains("result -> result"), "{summary}");
    assert!(summary.contains("status 0 -> status 1"), "{summary}");
}

#[test]
fn replay_reports_reused_ids_as_separate_request_occurrences() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("reused-id.jsonl");
    let initialize = lspcheck::transport::encode(br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
    let initialize_reply = lspcheck::transport::encode(br#"{"jsonrpc":"2.0","id":1,"result":{}}"#);
    let probe = lspcheck::transport::encode(br#"{"jsonrpc":"2.0","id":1,"method":"m"}"#);
    let probe_reply = lspcheck::transport::encode(br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601}}"#);
    let exit = lspcheck::transport::encode(br#"{"jsonrpc":"2.0","method":"exit"}"#);
    let lines = [
        serde_json::json!({"t_ms":0,"dir":"out","data":String::from_utf8(initialize).unwrap()}).to_string(),
        serde_json::json!({"t_ms":1,"dir":"in","data":String::from_utf8(initialize_reply).unwrap()}).to_string(),
        serde_json::json!({"t_ms":2,"dir":"out","data":String::from_utf8(probe).unwrap()}).to_string(),
        serde_json::json!({"t_ms":3,"dir":"in","data":String::from_utf8(probe_reply).unwrap()}).to_string(),
        serde_json::json!({"t_ms":4,"dir":"out","data":String::from_utf8(exit).unwrap()}).to_string(),
    ];
    std::fs::write(&input, lines.join("\n")).unwrap();
    let output = replay(&input, &[], &[STANDIN]);
    assert_eq!(output.status.code().unwrap_or(-1), 0, "{}", String::from_utf8_lossy(&output.stderr));
    let summary = String::from_utf8_lossy(&output.stdout);
    assert!(summary.contains("1: result -> result"), "{summary}");
    assert!(summary.contains("1: error -32601 -> error -32601"), "{summary}");
}

#[test]
fn replay_marks_a_request_unattributable_when_its_id_is_still_pending() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("pending-id.jsonl");
    let request = lspcheck::transport::encode(br#"{"jsonrpc":"2.0","id":1,"method":"m"}"#);
    let response = lspcheck::transport::encode(br#"{"jsonrpc":"2.0","id":1,"result":null}"#);
    let exit = lspcheck::transport::encode(br#"{"jsonrpc":"2.0","method":"exit"}"#);
    let combined = [request.as_slice(), request.as_slice()].concat();
    let lines = [
        serde_json::json!({"t_ms":0,"dir":"out","data":String::from_utf8(combined).unwrap()}).to_string(),
        serde_json::json!({"t_ms":1,"dir":"in","data":String::from_utf8(response).unwrap()}).to_string(),
        serde_json::json!({"t_ms":2,"dir":"out","data":String::from_utf8(exit).unwrap()}).to_string(),
    ];
    std::fs::write(&input, lines.join("\n")).unwrap();
    let output = replay(&input, &[], &[STANDIN]);
    assert_eq!(output.status.code().unwrap_or(-1), 0, "{}", String::from_utf8_lossy(&output.stderr));
    let summary = String::from_utf8_lossy(&output.stdout);
    assert!(summary.contains("not attributable (id reused while pending)"), "{summary}");
}

#[test]
fn replay_of_end_of_input_closes_stdin_at_the_recorded_point() {
    let (r, transcripts, _keep) = run_with_transcripts(&[], &["--only", "L8"]);
    assert_eq!(r.status, 0, "{}", r.stdout);
    let input = transcripts.join("end-of-input.jsonl");
    let recorded_exit = std::fs::read_to_string(&input)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|event| event["event"] == "exited")
        .expect("recorded exit")["how"]
        .as_str()
        .unwrap()
        .to_string();
    let output = replay(&input, &[], &[STANDIN]);
    assert_eq!(output.status.code().unwrap_or(-1), 0, "{}", String::from_utf8_lossy(&output.stderr));
    let summary = String::from_utf8_lossy(&output.stdout);
    assert!(summary.contains(&format!("{recorded_exit} -> {recorded_exit}")), "{summary}");
    assert!(!summary.contains("stopped by lspcheck"), "{summary}");
    let replayed = std::fs::read_to_string(transcripts.join("end-of-input.replay.jsonl")).unwrap();
    assert!(replayed.contains("\"event\":\"stdin closed\""), "{replayed}");
}

#[test]
fn replay_summary_lists_every_request_in_the_framing_recording() {
    let (r, transcripts, _keep) = run_with_transcripts(&[], &[]);
    assert_eq!(r.status, 0, "{}", r.stdout);
    let recording = transcripts.join("framing.jsonl");
    let expected = recorded_request_count(&recording);
    assert_eq!(expected, 11, "the framing scenario's expected request count");
    let output = replay(&recording, &[], &[STANDIN]);
    assert_eq!(output.status.code().unwrap_or(-1), 0, "{}", String::from_utf8_lossy(&output.stderr));
    let count = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.starts_with("    ") && line.contains(" -> "))
        .count();
    assert_eq!(count, expected, "{}", String::from_utf8_lossy(&output.stdout));
}

#[test]
fn replay_of_a_transcript_without_a_workspace_notes_it() {
    let (r, t, _keep) = run_with_transcripts(&[], &[]);
    assert_eq!(r.status, 0, "{}", r.stdout);
    // An older transcript: the same bytes, minus the workspace line.
    let input = t.join("lifecycle.jsonl");
    let text = std::fs::read_to_string(&input).unwrap();
    let lines: Vec<&str> = text.lines().filter(|l| !l.contains("\"workspace\"")).collect();
    let old = t.join("old.jsonl");
    std::fs::write(&old, lines.join("\n") + "\n").unwrap();
    let out = replay(&old, &[], &[STANDIN]);
    assert_eq!(out.status.code().unwrap_or(-1), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("none recorded"), "{stdout}");
}
