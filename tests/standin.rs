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
use std::process::Command;
use std::time::{Duration, Instant};

const LSPCHECK: &str = env!("CARGO_BIN_EXE_lspcheck");
const STANDIN: &str = env!("CARGO_BIN_EXE_lspcheck-standin");
/// No run should take this long; a hang fails the test instead of the job.
const WATCHDOG: Duration = Duration::from_secs(90);

/// Capture output in files so a full pipe can't block exit while the watchdog
/// polls. This also keeps the watchdog independent of output-reader threads.
fn output_with_watchdog(cmd: &mut Command) -> std::process::Output {
    let stdout = tempfile::NamedTempFile::new().unwrap();
    let stderr = tempfile::NamedTempFile::new().unwrap();
    cmd.stdout(stdout.reopen().unwrap()).stderr(stderr.reopen().unwrap());
    let started = Instant::now();
    let mut child = cmd.spawn().unwrap();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return std::process::Output {
                status,
                stdout: std::fs::read(stdout.path()).unwrap(),
                stderr: std::fs::read(stderr.path()).unwrap(),
            };
        }
        if started.elapsed() > WATCHDOG {
            let _ = child.kill();
            panic!("command ran longer than {WATCHDOG:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn watchdog_capture_doesnt_block_on_large_output() {
    // LSP lifecycle input makes the stand-in emit over a pipe's capacity on
    // exit. The capture must let it finish and retain the final bytes too.
    let input = tempfile::NamedTempFile::new().unwrap();
    let messages = [
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "shutdown"}),
        serde_json::json!({"jsonrpc": "2.0", "method": "exit"}),
    ];
    let bytes: Vec<u8> =
        messages.iter().flat_map(|m| lspcheck::transport::encode(&serde_json::to_vec(m).unwrap())).collect();
    std::fs::write(input.path(), bytes).unwrap();
    let mut cmd = Command::new(STANDIN);
    cmd.args(["--fault", "exit-flood"]).stdin(input.reopen().unwrap());
    let out = output_with_watchdog(&mut cmd);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.len() > 1024 * 1024);
    assert!(out.stdout.ends_with(b"goodbye\n"));
}

struct Run {
    status: i32,
    verdicts: BTreeMap<String, String>,
    stdout: String,
    /// The JSON report.
    report: Value,
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
    cmd.args(lspcheck_args).arg("--json").arg(&report).arg("--transcripts").arg(&transcripts).arg("--").args(server);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    let started = Instant::now();
    let out = output_with_watchdog(&mut cmd);
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
    Run { status: out.status.code().unwrap_or(-1), verdicts, stdout, report: json, server_stderr, took }
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
    cmd.args(lspcheck_args).arg("--").args(server);
    output_with_watchdog(&mut cmd)
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
    run_taking_effect(&["--fault", name], marker)
}

/// Like [`fault_taking_effect`], for a stand-in started with `standin_args`.
fn run_taking_effect(standin_args: &[&str], marker: &str) -> Run {
    for _ in 0..5 {
        let r = run(standin_args, &[]);
        if r.server_stderr.contains(marker) {
            return r;
        }
    }
    panic!("the stand-in given {standin_args:?} never said {marker:?} in 5 runs");
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
/// and envelope checks judge, so J1, J2, J5 and J7 are inconclusive.
fn only_fails_unread(run: &Run, failing: &[&str]) {
    only_fails_unread_with(run, failing, &["J1", "J2", "J5", "J7"]);
}

fn only_fails_unread_with(run: &Run, failing: &[&str], inconclusive: &[&str]) {
    for (id, verdict) in &run.verdicts {
        if id == "L9" && !cfg!(unix) {
            continue;
        }
        let want = if failing.contains(&id.as_str()) {
            "fail"
        } else if inconclusive.contains(&id.as_str()) {
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
    expect(&r, "R1", "pass");
    expect(&r, "R3", "pass");
    expect(&r, "R2", "pass");
    expect(&r, "R4", "pass");
    assert_eq!(r.status, 0, "{}", r.stdout);
    assert_eq!(r.verdicts.len(), lspcheck::checks::CATALOGUE.len());
    // Without a profile, the report says so and the header doesn't mention one.
    assert_eq!(r.report["profile"], Value::Null);
    assert!(!r.stdout.lines().next().unwrap_or("").contains("profile"), "{}", r.stdout);
    // D1, D2 and D3 passed on evidence, not for lack of it.
    assert!(r.stdout.contains("informative position(s) on character boundaries"), "{}", r.stdout);
    assert!(
        r.stdout.contains("versioned diagnostics notification(s) named a version lspcheck had sent"),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("informative position(s) on character boundaries after the change"), "{}", r.stdout);
    assert!(r.stdout.contains("textDocument/hover (hoverProvider) returned InvalidParams (-32602)"), "{}", r.stdout);
    assert!(r.stdout.contains("all 8 requests got responses with their exact ids"), "{}", r.stdout);
    assert!(r.stdout.contains("ParseError (-32700) response(s) with id null"), "{}", r.stdout);
    assert!(r.stdout.contains("following didOpen of a 4 MiB document"), "{}", r.stdout);
    // K1 passed on evidence: three positions for hover and highlights, one
    // selectionRange with all three, symbols, folding ranges and workspace symbols.
    assert!(
        r.stdout.contains("10 request(s) for 6 advertised method(s) were answered, none with MethodNotFound"),
        "{}",
        r.stdout
    );
    // D4 passed on evidence: the closed, the never-opened and the plain
    // request were all answered.
    assert!(r.stdout.contains("all three answered"), "{}", r.stdout);
    // D6 passed on evidence: the selectionRange replies were judged.
    assert!(
        r.stdout.contains("every range contains its position, and every parent contains its child"),
        "{}",
        r.stdout
    );
}

#[test]
fn junit_and_markdown_reports_agree_with_json() {
    let dir = tempfile::tempdir().unwrap();
    let json_path = dir.path().join("report.json");
    let junit_path = dir.path().join("report.xml");
    let markdown_path = dir.path().join("report.md");
    let mut cmd = Command::new(LSPCHECK);
    for (flag, value) in [("--timeout", "5"), ("--startup-timeout", "5"), ("--exit-timeout", "2")] {
        cmd.args([flag, value]);
    }
    cmd.arg("--json")
        .arg(&json_path)
        .arg("--junit")
        .arg(&junit_path)
        .arg("--markdown")
        .arg(&markdown_path)
        .arg("--transcripts")
        .arg(dir.path().join("t"))
        .arg("--")
        .arg(STANDIN)
        .args(["--fault", "shutdown-result"]);
    let out = output_with_watchdog(&mut cmd);
    assert_eq!(
        out.status.code(),
        Some(1),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let json: Value = serde_json::from_str(&std::fs::read_to_string(&json_path).unwrap()).unwrap();
    let results = json["results"].as_array().unwrap();
    let verdict = |id: &str| results.iter().find(|r| r["id"] == id).unwrap()["verdict"].as_str().unwrap();
    assert_eq!(verdict("L4"), "fail");
    let problems: Vec<&Value> =
        results.iter().filter(|r| matches!(r["verdict"].as_str(), Some("fail" | "error"))).collect();
    assert_eq!(problems.len(), 1, "the fault fails exactly L4");
    assert_eq!(problems[0]["id"], "L4");

    // The XML carries one testcase per JSON result and the same verdict elements.
    let xml = std::fs::read_to_string(&junit_path).unwrap();
    assert_eq!(xml.matches("<testcase ").count(), results.len());
    assert_eq!(xml.matches("<failure ").count(), results.iter().filter(|r| r["verdict"] == "fail").count());
    assert_eq!(xml.matches("<error ").count(), results.iter().filter(|r| r["verdict"] == "error").count());
    assert_eq!(
        xml.matches("<skipped ").count(),
        results.iter().filter(|r| matches!(r["verdict"].as_str(), Some("skip" | "not-exercised"))).count()
    );
    assert!(xml.contains("<testsuites name=\"lspcheck\""), "{xml}");
    assert!(xml.contains("<failure type=\"MUST\" message="), "{xml}");
    assert!(xml.contains("name=\"L4 shutdown returns null and the server keeps running until exit\""), "{xml}");
    assert!(xml.trim_end().ends_with("</testsuites>"));

    // Every row carries its JSON verdict, and the failure comes first.
    let markdown = std::fs::read_to_string(&markdown_path).unwrap();
    let rows: Vec<&str> = markdown
        .lines()
        .skip_while(|line| !line.starts_with("| Verdict |"))
        .skip(2)
        .take_while(|line| line.starts_with('|'))
        .collect();
    assert_eq!(rows.len(), results.len(), "{markdown}");
    assert!(rows[0].starts_with("| ❌ FAIL | [L4]("), "{markdown}");
    assert!(rows[0].contains("| MUST |"), "{markdown}");
    for r in results {
        let id = r["id"].as_str().unwrap();
        let cell = match r["verdict"].as_str().unwrap() {
            "fail" => "❌ FAIL",
            "error" => "⚠️ ERROR",
            "skip" => "⏭️ SKIP",
            "not-exercised" => "➖ N/E",
            _ => "✅ PASS",
        };
        let row =
            rows.iter().find(|line| line.contains(&format!("| [{id}]("))).unwrap_or_else(|| panic!("no {id} row"));
        assert!(row.starts_with(&format!("| {cell} | [{id}](")), "{row}");
    }
}

#[test]
fn aggregate_reports_reference_recorded_transcript_directory() {
    let dir = tempfile::tempdir().unwrap();
    let junit = dir.path().join("report.xml");
    let markdown = dir.path().join("report.md");
    let transcripts = dir.path().join("evidence");
    let r = run_command_in(
        &["--only", "F1", "--junit", junit.to_str().unwrap(), "--markdown", markdown.to_str().unwrap()],
        &[STANDIN, "--fault", "stdout-banner"],
        None,
        transcripts.clone(),
    );
    assert_eq!(r.status, 1, "{}", r.stdout);
    assert_eq!(r.verdicts.get("F1").map(String::as_str), Some("fail"));
    assert!(std::fs::read_dir(&transcripts).unwrap().next().is_some());
    assert!(r.stdout.contains(&format!("transcripts: {}\n", transcripts.display())), "{}", r.stdout);
    let xml = std::fs::read_to_string(junit).unwrap();
    assert!(xml.contains(&format!("\ntranscripts: {}</failure>", transcripts.display())), "{xml}");
    let text = std::fs::read_to_string(markdown).unwrap();
    assert!(text.contains(&format!("- F1: transcripts: `{}`\n", transcripts.display())), "{text}");
}

#[test]
fn lawful_variations_fail_nothing() {
    for name in [
        "extension-header",
        "decimal-ids",
        "long-header",
        "answers-reordered",
        "slow-reads",
        "big-dollar-reply",
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
        "register-other",
        "unopened-error",
        "closed-from-disk",
        "disk-diagnostics-after-close",
        "empty-selection",
        "reuse-answered-request-id",
        "await-progress-answer",
        "wrong-params-other-code",
        "trailing-fragment",
        "silent-on-bad-json",
        "slow-large-document",
        "capability-other-error",
        "echo-positions",
    ] {
        let r = if name == "slow-large-document" {
            run(&["--fault", name], &["--timeout", "1", "--startup-timeout", "10", "--large-document-mib", "2"])
        } else {
            fault(name)
        };
        if name == "wrong-params-other-code" {
            expect(&r, "R2", "pass");
            assert!(r.stdout.contains("not InvalidParams (which is preferred)"), "{}", r.stdout);
        }
        if name == "capability-other-error" {
            assert!(r.stdout.contains("-32803 (RequestFailed) for textDocument/documentColor"), "{}", r.stdout);
        }
        if name == "slow-large-document" {
            expect(&r, "R3", "pass");
            assert!(r.server_stderr.contains("slow-large-document sleeping"), "{}", r.server_stderr);
            assert!(r.stdout.contains("following didOpen of a 2 MiB document"), "{}", r.stdout);
        }
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
        if name == "no-diagnostics" {
            expected.push(("D5", "not-exercised"));
        }
        passes_except(&r, &expected);
        assert_eq!(r.status, 0, "{name}\n{}", r.stdout);
    }
}

#[test]
fn an_advertised_request_answered_with_method_not_found_fails_k1() {
    let r = fault("advertises-unimplemented");
    only_fails(&r, &["K1"]);
    assert!(
        r.stdout.contains("MethodNotFound (-32601) for 1 advertised request(s): textDocument/documentColor"),
        "{}",
        r.stdout
    );
}

#[test]
fn method_not_found_after_a_result_still_fails_k1() {
    // MethodNotFound seen for a K1 request is evidence even when a result for
    // the same id came first; the duplicate itself is J2's.
    let r = fault("method-not-found-after-result");
    only_fails(&r, &["K1", "J2"]);
    assert!(
        r.stdout.contains("MethodNotFound (-32601) for 1 advertised request(s): textDocument/documentColor"),
        "{}",
        r.stdout
    );
}

#[test]
fn an_advertised_request_left_unanswered_leaves_k1_inconclusive() {
    // LSP sets no deadline for a response, so a request answered neither with
    // MethodNotFound nor anything else by the end of the session is ERROR.
    let r = fault("ignores-advertised");
    passes_except(&r, &[("K1", "error")]);
    assert!(
        r.stdout
            .contains("no response to textDocument/documentColor by the end of the session, so K1 couldn't judge it"),
        "{}",
        r.stdout
    );
}

#[test]
fn a_configuration_request_that_comes_with_k1s_last_reply_is_answered() {
    // A lawful server may hold what follows a request until its answer, and
    // this one asks for configuration with the reply to K1's last request.
    // lspcheck must answer it while it shuts the server down, or the half
    // written log notification is a false F1.
    let r = run(&["--fault", "config-before-log-finish"], &["--only", "K1,F1"]);
    passes_except(&r, &[]);
    expect(&r, "K1", "pass");
    expect(&r, "F1", "pass");
    assert_eq!(r.status, 0, "{}", r.stdout);
}

#[test]
fn a_held_reply_that_arrives_at_shutdown_still_answers_k1() {
    // A server that holds its workspace/symbol reply until the next input
    // answers it when `shutdown` arrives. With no document open, that is
    // K1's only request, and the reply read during `leave` still counts.
    let r = run(&["--fault", "no-open-close", "--fault", "defer-until-next"], &["--only", "K1"]);
    expect(&r, "K1", "pass");
    assert_eq!(r.status, 0, "{}", r.stdout);
}

#[test]
fn k1_counts_replies_read_only_after_shutdown_finishes() {
    for (fault, verdict, status) in
        [("advertised-error-after-shutdown", "fail", 1), ("advertised-result-after-shutdown", "pass", 0)]
    {
        let (r, transcripts, _dir) = run_with_transcripts(&["--fault", fault], &["--only", "K1"]);
        expect(&r, "K1", verdict);
        assert_eq!(r.status, status, "{}", r.stdout);
        let events: Vec<Value> = std::fs::read_to_string(transcripts.join("capabilities.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let exited = events.iter().position(|e| e["event"] == "exited").unwrap();
        assert!(events[exited + 1..].iter().any(|e| e["dir"] == "in"), "{events:?}");
    }
}

#[test]
fn k1_sends_remaining_advertised_requests_after_a_timeout() {
    let r = run(
        &["--fault", "defer-hover-until-shutdown", "--fault", "advertises-unimplemented", "--fault", "echo-positions"],
        &["--only", "K1"],
    );
    expect(&r, "K1", "fail");
    assert_eq!(r.status, 1, "{}", r.stdout);
    assert!(
        r.stdout.contains("MethodNotFound (-32601) for 1 advertised request(s): textDocument/documentColor"),
        "{}",
        r.stdout
    );
    assert_eq!(hovers(&r, "lspcheck-k1.txt"), ["0:1", "1:5", "2:1"], "{}", r.server_stderr);
}

#[test]
fn k1_covers_all_requests_when_hover_waits_for_shutdown() {
    let r = run(&["--fault", "defer-hover-until-shutdown"], &["--only", "K1"]);
    expect(&r, "K1", "pass");
    assert_eq!(r.status, 0, "{}", r.stdout);
    assert!(r.stdout.contains("10 request(s) for 6 advertised method(s) were answered"), "{}", r.stdout);
}

/// The hover positions the stand-in reported for `file`, in order.
fn hovers(r: &Run, file: &str) -> Vec<String> {
    let prefix = format!("standin: hover at {file} ");
    r.server_stderr.lines().filter_map(|l| l.strip_prefix(&prefix)).map(str::to_string).collect()
}

#[test]
fn a_request_about_a_document_left_unanswered_leaves_k1_inconclusive() {
    // The documents scenario takes an unanswered request as no evidence, and
    // K1 can't judge a request with no response by the end of the session
    // either: it is an error, not a failure.
    let r = fault("drop-first-request");
    passes_except(&r, &[("K1", "error")]);
    assert!(r.stdout.contains("no response to textDocument/hover at 1:2 by the end of the session"), "{}", r.stdout);
}

#[test]
fn k1_asks_one_character_into_the_first_identifier_tokens_of_its_own_document() {
    let r = run(&["--fault", "echo-positions"], &["--only", "K1"]);
    expect(&r, "K1", "pass");
    assert_eq!(hovers(&r, "lspcheck-k1.txt"), ["0:1", "1:5", "2:1"], "{}", r.server_stderr);
}

#[test]
fn k1_asks_at_the_profiles_positions_in_the_negotiated_encoding() {
    // Column 3 of line 2 follows an emoji and a space: 3 UTF-16 code units in,
    // and 5 bytes.
    let (path, _keep) = profile(
        "[workspace]\nfiles = { \"src/main.txt\" = \"x\\n\u{1F600} target here\\n\" }\n\
         [document]\npath = \"src/main.txt\"\nlanguage_id = \"plaintext\"\npositions = [\"2:3\", \"1:2\"]\n",
    );
    for (faults, want) in
        [(&["echo-positions"][..], ["1:3", "0:1"]), (&["echo-positions", "utf8-encoding"], ["1:5", "0:1"])]
    {
        let args: Vec<&str> = faults.iter().flat_map(|f| ["--fault", f]).collect();
        let r = run(&args, &["--profile", path.to_str().unwrap(), "--only", "K1"]);
        expect(&r, "K1", "pass");
        assert_eq!(hovers(&r, "main.txt"), want, "{faults:?}\n{}", r.server_stderr);
        assert!(hovers(&r, "lspcheck-k1.txt").is_empty(), "{}", r.server_stderr);
    }
}

#[test]
fn a_bad_document_section_is_a_usage_error_before_any_server_starts() {
    let files = "[workspace]\nfiles = { \"main.go\" = \"package main\\n\" }\n";
    for (document, want) in [
        ("path = \"other.go\"\nlanguage_id = \"go\"\n", "document.path: \"other.go\" isn't a file"),
        ("path = \"main.go\"\nlanguage_id = \"go\"\npositions = [\"one:two\"]\n", "document.positions: \"one:two\""),
        ("path = \"main.go\"\nlanguage_id = \"go\"\npositions = [\"1:14\"]\n", "1:14 is past the end of line 1"),
        ("path = \"main.go\"\n", "document.language_id is required"),
    ] {
        let (path, keep) = profile(&format!("{files}[document]\n{document}"));
        let transcripts = keep.path().join("t");
        let out = Command::new(LSPCHECK)
            .args(["--profile", path.to_str().unwrap(), "--transcripts", transcripts.to_str().unwrap(), "--", STANDIN])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{document}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(want), "{document}: {stderr}");
        assert!(!transcripts.exists(), "nothing was set up for a run");
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
fn a_frame_skipped_before_a_request_was_sent_doesnt_excuse_its_missing_reply() {
    // The oversized frame comes before the reply to initialize, long before
    // J4's and J6's requests are sent, so it can't have held their replies.
    // The checks judged across the whole stream are still inconclusive.
    for (f, id) in [("ignore-dollar-request", "J4"), ("drop-concurrent", "J6")] {
        let r = run(&["--fault", "big-frame", "--fault", f], &["--max-frame-mib", "1"]);
        expect(&r, id, "fail");
        expect(&r, "F1", "error");
    }
    // Skipped while J4's first request was answered, before the $/ request
    // that goes unanswered was sent.
    let r = run(
        &["--fault", "big-frame-before-unknown-reply", "--fault", "ignore-dollar-request"],
        &["--max-frame-mib", "1"],
    );
    expect(&r, "J4", "fail");
}

#[test]
fn a_frame_unfinished_while_lspcheck_waited_makes_its_absence_inconclusive() {
    // A frame begun just before the shutdown reply swallows it, and is only
    // found to be cut short when the stream ends, after lspcheck stopped
    // waiting. The reply may have been in it, so L4 can't fail.
    let r = fault("inflated-before-shutdown");
    expect(&r, "F1", "fail");
    for (id, v) in &r.verdicts {
        if id != "F1" {
            assert_ne!(v, "fail", "{id}\n{}", r.stdout);
        }
    }
    expect(&r, "L4", "error");
    assert!(r.stdout.contains("inconclusive, because some of the server's frames couldn't be read"), "{}", r.stdout);
}

#[test]
fn a_frame_unfinished_when_inspection_stops_makes_its_absence_inconclusive() {
    // J4's $/ reply is swallowed by a frame that never ends; at shutdown,
    // after J4 was judged, the server writes past the output budget, so
    // lspcheck stops inspecting inside that frame. It may have held the reply.
    let r = run(&["--fault", "inflated-on-dollar-request", "--fault", "flood-on-shutdown"], &["--max-output-mib", "1"]);
    expect(&r, "J4", "error");
    assert!(r.stdout.contains("a frame was still unfinished when lspcheck stopped inspecting"), "{}", r.stdout);
    assert!(!r.verdicts.values().any(|v| v == "fail"), "{}", r.stdout);
}

#[test]
fn a_cancelled_answer_swallowed_by_an_unfinished_frame_is_inconclusive() {
    // The answer to the cancelled request goes inside a frame found cut short
    // only when the stream ends, after C1 was judged.
    let r = fault("inflated-on-cancel");
    expect(&r, "C1", "error");
    expect(&r, "F1", "fail");
    for (id, v) in &r.verdicts {
        if id != "F1" {
            assert_ne!(v, "fail", "{id}\n{}", r.stdout);
        }
    }
}

#[test]
fn verdicts_reached_before_a_frame_was_found_cut_short_are_inconclusive() {
    // Each fault starts a frame that never ends, inside the window these
    // checks judge, while they are still judged as passing or not exercised.
    // The frame is found cut short only when the stream ends; it may have
    // held what they look for.
    for (f, ids) in [
        // The frame starts at shutdown, before the cancellation scenario's
        // window ends; C1's ends at the cancel, earlier.
        ("inflated-before-shutdown", &["C2", "C3"][..]),
        // The frame starts at the change, while document A is open.
        ("inflated-on-change", &["D1", "D2", "D3", "D5", "D6"][..]),
        // The frame swallows the framing scenario's plain request, which
        // would have left the variants not exercised.
        ("inflated-on-first-probe", &["F3", "F4", "F5", "F6", "F7"][..]),
    ] {
        let r = run(&["--fault", f], &["--timeout", "2"]);
        for id in ids {
            expect(&r, id, "error");
        }
        assert!(
            r.stdout.contains("inconclusive, because some of the server's frames couldn't be read"),
            "{f}\n{}",
            r.stdout
        );
        if f == "inflated-before-shutdown" {
            // C1's window ended at the cancel, before the frame began.
            expect(&r, "C1", "pass");
        }
        expect(&r, "F1", "fail");
        for (id, v) in &r.verdicts {
            if id != "F1" {
                assert_ne!(v, "fail", "{f}: {id}\n{}", r.stdout);
            }
        }
    }
}

#[test]
fn a_reply_skipped_while_lspcheck_waited_makes_its_absence_inconclusive() {
    // The $/ request's reply is over the inspection limit, so lspcheck never
    // sees it: J4 can't say it went unanswered. Within the limit it passes.
    let r = run(&["--fault", "big-dollar-reply"], &["--max-frame-mib", "1"]);
    expect(&r, "J4", "error");
    assert!(r.stdout.contains("inconclusive"), "{}", r.stdout);
    expect(&fault("big-dollar-reply"), "J4", "pass");
}

#[test]
fn requests_lspcheck_couldnt_send_are_errors_not_failures() {
    // The server stops reading while lspcheck answers its request, so
    // lspcheck can't deliver the requests J4 and L4 depend on.
    // Only run the three scenarios asserted below: waiting on this blocked
    // server in every scenario can exceed the watchdog on Windows.
    let r = run(&["--fault", "block-after-init"], &["--timeout", "1", "--only", "L2,J4,L4,L6,F3,F4,F5,F6,F7,L7"]);
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
    only_fails_unread_with(&r, &["F1", "F2"], &["J1", "J2", "J3", "J5", "J7", "R4"]);
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
    // concurrent and odd string-id requests unanswered, so J6 and R4 fail.
    let r = fault("exit-on-string-id");
    expect(&r, "F2", "not-exercised");
    expect(&r, "F3", "pass");
    expect(&r, "J1", "not-exercised");
    expect(&r, "J6", "fail");
    expect(&r, "R4", "fail");
    assert!(r.stdout.contains("the part that splits multibyte characters wasn't run"), "{}", r.stdout);
    assert!(r.stdout.contains("the server exited with status 2"), "{}", r.stdout);
    assert!(r.verdicts.iter().all(|(id, v)| v != "fail" || matches!(id.as_str(), "J6" | "R4")), "{}", r.stdout);
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
    // Where the server's reads end decides whether the fault can act, so
    // repeat until it says it did.
    let r = fault_taking_effect("first-frame-per-read", "first-frame-per-read dropped");
    only_fails(&r, &["F4"]);
    assert!(r.stdout.contains("1 of 2 requests went unanswered"), "{}", r.stdout);
    // Even when slow reads make the server read each frame separately, the
    // fault must act on F4's shape only: `initialized` coalescing with J6's
    // requests isn't a read of three frames with $/setTrace in it.
    let r = run_taking_effect(
        &["--fault", "first-frame-per-read", "--fault", "slow-reads"],
        "first-frame-per-read dropped",
    );
    only_fails(&r, &["F4"]);
}

#[test]
fn counting_incoming_characters_instead_of_bytes_fails_f5() {
    let r = fault("char-length-input");
    expect(&r, "F5", "fail");
    // The probe with non-ASCII output for F2 isn't sent to a server that can't read it.
    // Nor can it read D1's document, whose emoji put its input out of step:
    // no position or versioned diagnostics come back, which says nothing
    // about how it counts them. It had stopped answering before D4's
    // requests, so their going unanswered says nothing about documents that
    // aren't open: D4 is an error. R4 also fails because this request includes
    // a valid non-ASCII string id that the server can't parse.
    passes_except(
        &r,
        &[
            ("F5", "fail"),
            ("R4", "fail"),
            ("F2", "not-exercised"),
            ("D1", "not-exercised"),
            ("D2", "not-exercised"),
            ("D3", "not-exercised"),
            ("D4", "error"),
            ("D5", "not-exercised"),
            ("D6", "not-exercised"),
        ],
    );
    assert!(r.stdout.contains("but the server had already stopped answering"), "{}", r.stdout);
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
    // doesn't ask for didOpen, so all three are skipped, and it advertises no
    // document request, so D4 skips too. D6 can't clamp in the bad encoding,
    // and skips with no capabilities. K1 can't count its positions in the
    // bad encoding either, and has no capabilities to read without them.
    passes_except(
        &fault("bad-encoding"),
        &[
            ("L2", "fail"),
            ("D1", "error"),
            ("D2", "error"),
            ("D3", "error"),
            ("D5", "error"),
            ("D6", "error"),
            ("K1", "error"),
        ],
    );
    passes_except(
        &fault("no-capabilities"),
        &[
            ("L2", "fail"),
            ("D1", "skip"),
            ("D2", "skip"),
            ("D3", "skip"),
            ("D4", "skip"),
            ("D5", "skip"),
            ("D6", "skip"),
            ("R2", "not-exercised"),
            ("K1", "not-exercised"),
        ],
    );
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
    // own id, so J6 fails too; R4 also checks the numeric-looking string id.
    only_fails(&r, &["J1", "J6", "R4"]);
    expect(&r, "R4", "fail");
    assert!(r.stdout.contains("where lspcheck sent"), "{}", r.stdout);
}

#[test]
fn mangled_odd_string_ids_fail_r4() {
    let r = fault("mangles-odd-ids");
    expect(&r, "R4", "fail");
    assert!(
        r.verdicts.iter().all(|(id, verdict)| { verdict != "fail" || matches!(id.as_str(), "R4" | "J1" | "J2") }),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("likely id mangling"), "{}", r.stdout);
}

#[test]
fn negative_integer_ids_must_get_a_response() {
    let r = fault("drops-negative-ids");
    only_fails(&r, &["R4"]);
    expect(&r, "R4", "fail");
    assert!(r.stdout.contains("-1"), "{}", r.stdout);
}

#[test]
fn odd_id_failures_survive_later_unreadable_output() {
    let r = run(
        &["--fault", "drops-negative-ids", "--fault", "big-frame-after-drop", "--fault", "char-length"],
        &["--max-frame-mib", "1"],
    );
    expect(&r, "R4", "fail");
    expect(&r, "J3", "error");
    assert!(r.stdout.contains("-1"), "{}", r.stdout);
    assert!(r.stdout.contains("inspection limit"), "{}", r.stdout);
}

#[test]
fn a_skipped_frame_before_exit_makes_r1_r2_r3_inconclusive() {
    // The plain request after the wrongly typed one is answered in a frame
    // over the limit, then the server exits: the skipped frame may have held
    // the answer, so R2 can't fail. Read whole, it passes.
    // The same holds for R1 and R3, whose plain requests follow a non-JSON body
    // and a large document.
    for id in ["R1", "R2", "R3"] {
        let r = run(&["--fault", "big-reply-then-exit"], &["--only", id, "--max-frame-mib", "1"]);
        expect(&r, id, "error");
        let r = run(&["--fault", "big-reply-then-exit"], &["--only", id]);
        expect(&r, id, "pass");
    }
}

#[test]
fn accepting_wrongly_typed_params_fails_r2() {
    let r = fault("accepts-wrong-params");
    only_fails(&r, &["R2"]);
    expect(&r, "R2", "fail");
    assert!(r.stdout.contains("returned a result for parameters of the wrong types"), "{}", r.stdout);
}

#[test]
fn malformed_errors_fail_r2() {
    for (name, problem) in [
        ("wrong-params-null-error", "code isn't an integer"),
        ("wrong-params-string-code", "code isn't an integer"),
        ("wrong-params-large-code", "outside LSP's integer range"),
        ("wrong-params-missing-message", "message isn't a string"),
        ("wrong-params-numeric-message", "message isn't a string"),
        ("wrong-params-result-and-error", "both \"result\" and \"error\""),
    ] {
        let r = run(&["--fault", name], &["--only", "R2,J5"]);
        only_fails(&r, &["R2", "J5"]);
        expect(&r, "R2", "fail");
        expect(&r, "J5", "fail");
        assert_eq!(r.status, 1, "{name}\n{}", r.stdout);
        assert!(r.stdout.contains("returned a malformed response instead of an error"), "{name}\n{}", r.stdout);
        assert!(r.stdout.contains(problem), "{name}\n{}", r.stdout);
    }
    for args in [vec![], vec!["--fault", "wrong-params-other-code"]] {
        let r = run(&args, &["--only", "R2,J5"]);
        only_fails(&r, &[]);
        expect(&r, "R2", "pass");
        expect(&r, "J5", "pass");
        assert_eq!(r.status, 0, "{}", r.stdout);
    }
}

#[test]
fn exiting_on_wrongly_typed_params_fails_r2() {
    let r = fault("dies-on-wrong-params");
    only_fails(&r, &["R2"]);
    expect(&r, "R2", "fail");
    assert!(r.stdout.contains("closed its stdout without responding"), "{}", r.stdout);
}

#[test]
fn non_json_bodies_are_rejected_or_ignored_without_losing_service() {
    let r = fault("parse-error-wrong-code");
    only_fails(&r, &["R1"]);
    expect(&r, "R1", "fail");
    expect(&r, "J2", "pass");
    expect(&r, "J5", "pass");
    assert!(
        r.stdout.contains("unmatched response with id null wasn't ParseError (-32700): error -32600"),
        "{}",
        r.stdout
    );

    let r = fault("silent-on-bad-json");
    only_fails(&r, &[]);
    expect(&r, "R1", "pass");
    assert!(r.stdout.contains("ignored the non-JSON body"), "{}", r.stdout);

    let r = fault("dies-on-bad-json");
    only_fails(&r, &["R1"]);
    expect(&r, "R1", "fail");
}

#[test]
fn r1_response_window_includes_queued_wrong_code_responses() {
    for faults in [
        vec!["--fault", "bad-json-reply-after-probe"],
        vec!["--fault", "parse-error-with-initialize", "--fault", "bad-json-reply-after-probe"],
    ] {
        let r = run(&faults, &["--only", "R1", "--strict"]);
        expect(&r, "R1", "fail");
        assert_eq!(r.status, 1, "{faults:?}\n{}", r.stdout);
        assert!(
            r.stdout.contains("unmatched response with id null wasn't ParseError (-32700): error -32600"),
            "{faults:?}\n{}",
            r.stdout
        );
    }
}

#[test]
fn r1_response_window_keeps_response_occurrences_separate() {
    let r = run(
        &["--fault", "parse-error-with-initialize", "--fault", "parse-error-wrong-code"],
        &["--only", "R1", "--strict"],
    );
    expect(&r, "R1", "fail");
    assert_eq!(r.status, 1, "{}", r.stdout);
    assert!(
        r.stdout.contains("unmatched response with id null wasn't ParseError (-32700): error -32600"),
        "{}",
        r.stdout
    );
}

#[test]
fn r1_response_window_excludes_earlier_parse_errors() {
    let r = run(&["--fault", "parse-error-with-initialize", "--fault", "silent-on-bad-json"], &["--only", "R1"]);
    expect(&r, "R1", "pass");
    assert_eq!(r.status, 0, "{}", r.stdout);
    assert!(r.stdout.contains("ignored the non-JSON body"), "{}", r.stdout);
}

#[test]
fn r1_response_window_preserves_lawful_replies_and_service() {
    for faults in [
        vec![],
        vec!["--fault", "silent-on-bad-json"],
        vec!["--fault", "reply-to-exit"],
        vec!["--fault", "await-progress-answer"],
        vec!["--fault", "trailing-fragment"],
        vec!["--fault", "big-reply-then-exit"],
        vec!["--fault", "parse-error-with-initialize"],
    ] {
        let r = run(&faults, &["--only", "R1,R2,R4,J1,J2,J6"]);
        expect(&r, "R1", "pass");
        expect(&r, "R2", "pass");
        expect(&r, "R4", "pass");
        expect(&r, "J1", "pass");
        expect(&r, "J6", "pass");
        if !faults.contains(&"parse-error-with-initialize") {
            expect(&r, "J2", "pass");
        }
        if !faults.contains(&"silent-on-bad-json") {
            assert!(r.stdout.contains("returned 1 ParseError (-32700) response(s)"), "{faults:?}\n{}", r.stdout);
        }
    }
}

#[test]
fn large_document_failure_and_transcript_replay() {
    let r = fault("dies-on-large-document");
    only_fails(&r, &["R3"]);
    expect(&r, "R3", "fail");

    let (r, transcripts, _keep) = run_with_transcripts(&[], &["--only", "R3"]);
    expect(&r, "R3", "pass");
    assert!(r.stdout.contains("following didOpen of a 4 MiB document"), "{}", r.stdout);
    let transcript = transcripts.join("large-document.jsonl");
    let out = replay(&transcript, &[], &[STANDIN]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("requests"), "{}", String::from_utf8_lossy(&out.stdout));
}

#[test]
fn large_document_size_must_be_nonzero() {
    let output = Command::new(LSPCHECK).args(["--large-document-mib", "0", "--", STANDIN]).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--large-document-mib"), "{stderr}");
    assert!(stderr.contains("invalid value"), "{stderr}");
}

#[test]
fn duplicate_and_unsent_responses_fail_j2() {
    // A duplicate or unasked reply in R1's window is J2's alone: only a
    // response with id null can answer a body without an id.
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

#[test]
fn a_server_waiting_on_its_own_request_after_exit_is_answered() {
    // The stand-in reads `exit` but handles it only once its progress request
    // is answered, as gopls does. lspcheck sends `exit` right after
    // `initialized`, so it must answer while waiting for the exit.
    let r = fault("await-progress-answer");
    expect(&r, "L6", "pass");
    expect(&r, "L7", "pass");
}

#[test]
fn a_server_still_running_after_exit_is_inconclusive_when_frames_were_missed() {
    // A frame lspcheck couldn't read may have been a request the server is
    // still waiting on, so its not exiting can't be blamed on it.
    let r = run(&["--fault", "ignore-exit", "--fault", "bad-bodies"], &[]);
    expect(&r, "L6", "error");
    expect(&r, "L7", "error");
    assert!(r.stdout.contains("so it may be waiting on lspcheck"), "{}", r.stdout);
    let r = fault("ignore-exit");
    expect(&r, "L6", "fail");
    expect(&r, "L7", "fail");
}

#[test]
fn unfinished_output_makes_exit_timeouts_inconclusive_after_cleanup() {
    let r = run(&["--fault", "inflated-on-exit"], &["--only", "L6,L7"]);
    expect(&r, "L6", "error");
    expect(&r, "L7", "error");
    assert!(r.stdout.contains("so it may be waiting on lspcheck"), "{}", r.stdout);
}

#[test]
fn a_server_request_reusing_an_unanswered_id_fails_j7() {
    // Both requests arrive in one write, before lspcheck has begun answering
    // the first, so the server can't have had its answer.
    let r = fault("reused-request-id");
    only_fails(&r, &["J7"]);
    assert!(r.stdout.contains("reused id 1 of its window/workDoneProgress/create request"), "{}", r.stdout);
    // Reusing it once the answer has come is lawful, and the clean stand-in
    // is seen to keep serving after lspcheck answers it.
    let r = fault("reuse-answered-request-id");
    expect(&r, "J7", "pass");
    assert!(r.stdout.contains("the server answered lspcheck's later requests after"), "{}", r.stdout);
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
    // is the only one with a six-digit string id, and all four requests are
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
    passes_except(
        &r,
        &[("D1", "skip"), ("D2", "skip"), ("D3", "skip"), ("D5", "skip"), ("D6", "skip"), ("R3", "fail")],
    );
    // D4 still runs: doc.txt on disk, never opened.
    assert!(r.stdout.contains("the document on disk (never opened)"), "{}", r.stdout);
    assert_eq!(r.status, 0, "{}", r.stdout);
}

#[test]
fn requests_about_documents_that_arent_open_must_be_answered() {
    // The stand-in never answers a request about a document that isn't
    // open: D4 fails on the closed document's request. J2 judges responses
    // that arrived unasked, not requests that went unanswered, so it passes.
    let r = fault("ignore-unopened");
    expect(&r, "D4", "fail");
    only_fails(&r, &["D4"]);
    assert_eq!(r.status, 0, "D4 is a POLICY check\n{}", r.stdout);
    assert!(r.stdout.contains("went unanswered"), "{}", r.stdout);
}

#[test]
fn a_server_that_stopped_answering_at_the_second_document_leaves_d4_an_error() {
    // A is answered at both versions; the server stops at B's first request,
    // the latest before D4's, so D4's unanswered requests say nothing about
    // documents that aren't open.
    let r = fault("stop-at-crlf");
    passes_except(&r, &[("D4", "error")]);
    assert!(r.stdout.contains("the server had already stopped answering"), "{}", r.stdout);
}

#[test]
fn a_server_that_recovered_from_an_earlier_timeout_still_fails_d4() {
    // The first document request goes unanswered, then the server answers
    // the rest at both versions, then stops at the closed document. It had
    // recovered, so D4 fails rather than calling it a server that had
    // already stopped answering. K1's first request gets no response by the
    // end of the session either, which it can't judge.
    let r = run(&["--fault", "drop-first-request", "--fault", "stop-after-unopened"], &[]);
    passes_except(&r, &[("D4", "fail"), ("K1", "error")]);
    assert!(r.stdout.contains("the closed document went unanswered"), "{}", r.stdout);
}

#[test]
fn stopping_after_an_unopened_request_fails_d4() {
    // After a request about a document that isn't open, the stand-in stops
    // answering anything: D4's first such request goes unanswered, and so do
    // the requests after it. No other scenario sends such a request, so
    // nothing else fails.
    let r = fault("stop-after-unopened");
    expect(&r, "D4", "fail");
    only_fails(&r, &["D4"]);
    assert_eq!(r.status, 0, "D4 is a POLICY check\n{}", r.stdout);
}

#[test]
fn positions_counted_in_the_wrong_units_fail_d1() {
    // Byte counting under UTF-16 runs past line ends (not judged) or lands
    // on boundaries after the change and on B; its selection ranges still
    // contain the positions, so D6 passes. Code-point counting lands inside
    // characters after the change and on B too, and its selection ranges
    // don't contain the positions: D1 and D6 fail, and each such position is
    // an in-line code-point offset, so D3 and D5 can't blame the change or
    // the line split: an error each.
    for (name, diagnosis, others) in [
        ("utf8-positions-as-utf16", "on a boundary if counted in UTF-8 bytes", &[][..]),
        (
            "codepoint-positions",
            "on a boundary if counted in code points",
            &[("D3", "error"), ("D5", "error"), ("D6", "fail")][..],
        ),
    ] {
        let r = fault(name);
        let mut expected = vec![("D1", "fail")];
        expected.extend_from_slice(others);
        passes_except(&r, &expected);
        assert_eq!(r.status, 1, "D1 is a MUST check");
        assert!(r.stdout.contains("inside a character of version 41"), "{name}\n{}", r.stdout);
        assert!(r.stdout.contains(diagnosis), "{name}\n{}", r.stdout);
    }
    // Counting UTF-16 units under UTF-8: the positions after the change are
    // invalid too, but each fits the changed text counted in UTF-16, so D3
    // can't tell a change-handling fault from a counting fault and is an
    // error; D1 carries the failure, and the selection ranges don't contain
    // the positions, so D6 fails too.
    let r = fault("utf16-positions-as-utf8");
    passes_except(&r, &[("D1", "fail"), ("D3", "error"), ("D5", "error"), ("D6", "fail")]);
    assert_eq!(r.status, 1, "D1 is a MUST check");
    assert!(r.stdout.contains("inside a character of version 41"), "{}", r.stdout);
    assert!(r.stdout.contains("on a boundary if counted in UTF-16 code units"), "{}", r.stdout);
    assert!(r.stdout.contains("can't tell a change-handling fault from a counting fault; see D1"), "{}", r.stdout);
}

#[test]
fn splitting_lines_the_wrong_way_fails_d1_and_d5() {
    // The detail names every wrong split a position fits, the fault's own
    // among them. The selection ranges, built on the wrong split, don't
    // contain the positions, so D6 fails too.
    for (name, split) in [
        ("lf-only-lines", "a line split at `\\n` only"),
        ("crlf-two-breaks", "`\\r\\n` counted as two breaks"),
        ("no-lone-cr", "not a lone `\\r`"),
    ] {
        let r = fault(name);
        only_fails(&r, &["D1", "D5", "D6"]);
        assert_eq!(r.status, 1, "D1, D5 and D6 are MUST checks");
        assert!(r.stdout.contains("under a line split the spec doesn't allow"), "{name}\n{}", r.stdout);
        assert!(r.stdout.contains(split), "{name}: {split}\n{}", r.stdout);
    }
}

#[test]
fn a_change_the_server_doesnt_apply_fails_d1_and_d3() {
    // The old text's positions, labelled with the new version: some, on the
    // inserted emoji line, are on no boundary of the changed text counted any
    // way, but on a boundary of the text before the change. The selection
    // ranges, built on the old text, don't contain the positions of the
    // changed text, so D6 fails too.
    let r = fault("ignore-changes");
    only_fails(&r, &["D1", "D3", "D6"]);
    assert_eq!(r.status, 1, "D1, D3 and D6 are MUST checks");
    assert!(r.stdout.contains("(fits the text before the change)"), "{}", r.stdout);
}

#[test]
fn change_ranges_counted_in_code_points_fail_d1_and_d3_is_an_error() {
    // The stand-in replaces two emoji where lspcheck's range names one, so
    // its positions on line 0 land inside characters of the changed text.
    // Each is also an in-line offset of the changed text counted in code
    // points, so a server that applied the change and counts code points
    // would report the same: D3 can't blame the change, and D1 fails. The
    // selection ranges, counted in code points, don't contain the positions,
    // so D6 fails too.
    let r = fault("codepoint-changes");
    passes_except(&r, &[("D1", "fail"), ("D3", "error"), ("D6", "fail")]);
    assert!(r.stdout.contains("can't tell a change-handling fault from a counting fault"), "{}", r.stdout);
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
fn a_registration_of_another_method_does_not_error_d3() {
    // A `client/registerCapability` for a method D3 doesn't depend on is
    // not a dynamic registration of the sync notifications: D3 proceeds
    // normally and passes.
    let r = fault("register-other");
    only_fails(&r, &[]);
    assert_eq!(r.status, 0, "{}", r.stdout);
}

#[test]
fn code_point_positions_after_a_correct_change_are_a_counting_fault() {
    // The stand-in applies the change correctly but counts positions in code
    // points from the change on. The pre-change positions are correct (so
    // the pin holds), but the post-change positions are on boundaries of
    // version 42 counted in code points: a counting fault (D1's), not a
    // change-handling fault. D3 is an error, not a failure. The post-change
    // selection ranges, counted in code points, don't contain the positions,
    // so D6 fails too.
    let r = fault("codepoint-positions-after-change");
    passes_except(&r, &[("D1", "fail"), ("D3", "error"), ("D5", "error"), ("D6", "fail")]);
    assert_eq!(r.status, 1, "D1 is a MUST check");
    assert!(r.stdout.contains("can't tell a change-handling fault from a counting fault; see D1"), "{}", r.stdout);
}

#[test]
fn an_undeliverable_change_errors_d3_but_keeps_d1s_failure() {
    // The stand-in publishes version 41's diagnostics with a position inside
    // the first character, then exits: lspcheck's `didChange` can't be
    // delivered, so D3 is an error, but D1's failure stands and D2 is still
    // judged (the diagnostic's version 41 was sent). The server is gone, so
    // D4 can't be judged either. The requests at version 41 either can't be
    // written or are cut off by the closed output, depending on when the
    // server's exit lands; both leave the selectionRange request unanswerable,
    // so D6 is an error either way.
    let r = fault("close-stdin-before-change");
    passes_except(&r, &[("D1", "fail"), ("D3", "error"), ("D4", "error"), ("D5", "error"), ("D6", "error")]);
    assert_eq!(r.status, 1, "D1 is a MUST check");
    assert!(r.stdout.contains("couldn't send didChange"), "{}", r.stdout);
}

#[test]
fn positions_in_a_duplicate_response_are_judged_too() {
    // The first answer is valid; the second, a duplicate (J2's finding),
    // has a position inside a character. The stand-in does this at both
    // versions, so the server was seen miscounting before the change and no
    // alternative text explains the position after it: D3 can't blame the
    // change, and says so, while D1 carries the failure. (A late duplicate of
    // a request sent before the change may refer to either version, so D3
    // doesn't judge it at all.)
    let r = fault("invalid-duplicate-symbols");
    passes_except(&r, &[("D1", "fail"), ("J2", "fail"), ("D3", "error")]);
    assert!(r.stdout.contains("can't tell a change-handling fault from a counting fault"), "{}", r.stdout);
}

#[test]
fn utf32_positions_leave_d1_not_exercised() {
    // No offset can fall inside a character in UTF-32.
    let r = fault("utf32-encoding");
    passes_except(&r, &[("D1", "not-exercised"), ("D3", "not-exercised"), ("D5", "not-exercised")]);
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
    assert!(r.stdout.contains("3 without a version, which is lawful"), "{}", r.stdout);
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

// --- D6: selection-range containment ----------------------------------------

#[test]
fn a_selection_range_outside_its_position_fails_d6() {
    // The range is the word before the position, still on a boundary: it
    // doesn't contain the position. Only D6 fails.
    let r = fault("selection-outside");
    only_fails(&r, &["D6"]);
    assert_eq!(r.status, 1, "D6 is a MUST check");
    assert!(r.stdout.contains("doesn't contain the position"), "{}", r.stdout);
}

#[test]
fn a_selection_range_request_cut_off_leaves_d6_an_error() {
    // Version 41's selection ranges are judged and fine; the server exits at
    // the first request at version 42, so that selectionRange request can't
    // be answered. D6 can't pass on half its evidence.
    let r = fault("exit-at-version-42");
    passes_except(&r, &[("D4", "error"), ("D5", "error"), ("D6", "error")]);
    assert!(r.stdout.contains("textDocument/selectionRange was cut off"), "{}", r.stdout);
}

#[test]
fn a_short_selection_range_result_fails_d6() {
    // One entry fewer than the positions: the array length doesn't match.
    // Only D6 fails.
    let r = fault("selection-short");
    only_fails(&r, &["D6"]);
    assert_eq!(r.status, 1, "D6 is a MUST check");
    assert!(r.stdout.contains("entries for"), "{}", r.stdout);
}

#[test]
fn a_selection_parent_inside_its_child_fails_d6() {
    // The parent is a range strictly inside its child: it doesn't contain it.
    // Only D6 fails.
    let r = fault("selection-parent-inside");
    only_fails(&r, &["D6"]);
    assert_eq!(r.status, 1, "D6 is a MUST check");
    assert!(r.stdout.contains("doesn't contain"), "{}", r.stdout);
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

// --- profiles ---------------------------------------------------------------

/// A profile file holding `text`, in a directory kept alive by the returned guard.
fn profile(text: &str) -> (std::path::PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("profile.toml");
    std::fs::write(&path, text).unwrap();
    (path, dir)
}

/// A TOML literal string holding `s` (no escapes, so Windows paths survive).
fn literal(s: &str) -> String {
    assert!(!s.contains('\''), "{s}");
    format!("'{s}'")
}

/// The stand-in with `faults`, checked with the profile at `path`.
fn with_profile(faults: &[&str], path: &std::path::Path) -> Run {
    let args: Vec<&str> = faults.iter().flat_map(|f| ["--fault", f]).collect();
    run(&args, &["--profile", path.to_str().unwrap()])
}

/// Replays `transcript` against the stand-in with `fault`, and asserts every
/// request's outcome is the one recorded.
fn replays_alike(transcript: &std::path::Path, fault: &str) -> String {
    let out = replay(transcript, &[], &[STANDIN, "--fault", fault]);
    assert_eq!(out.status.code().unwrap_or(-1), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let outcomes: Vec<&str> = stdout.lines().filter(|l| l.starts_with("    ") && l.contains(" -> ")).collect();
    assert!(!outcomes.is_empty(), "{stdout}");
    for line in outcomes {
        let (_, outcome) = line.split_once(": ").unwrap();
        let (recorded, replayed) = outcome.split_once(" -> ").unwrap();
        assert_eq!(recorded, replayed, "{stdout}");
    }
    stdout
}

#[test]
fn a_server_that_needs_initialization_options_is_an_error_without_them_and_passes_with_them() {
    let r = fault("needs-init-option");
    expect(&r, "L2", "error");
    assert!(r.verdicts.values().all(|v| v != "fail"), "{}", r.stdout);
    assert_eq!(r.status, 2);
    assert!(r.stdout.contains("refused to initialize"), "{}", r.stdout);

    // Every initialize carries them, the second one (L3) and the one after a
    // request before initialize (L1) included.
    let (path, _keep) = profile("[client]\ninitialization_options = { standin = \"ready\", depth = 2 }\n");
    let r = with_profile(&["needs-init-option"], &path);
    only_fails(&r, &[]);
    assert_eq!(r.status, 0, "{}", r.stdout);
    assert_eq!(r.report["profile"], path.to_str().unwrap());
    assert!(r.stdout.lines().next().unwrap().ends_with(&format!("— profile {}", path.display())), "{}", r.stdout);
}

#[test]
fn a_server_that_needs_a_workspace_file_passes_with_inline_files() {
    let r = fault("needs-workspace-file");
    expect(&r, "L2", "error");
    assert!(r.verdicts.values().all(|v| v != "fail"), "{}", r.stdout);
    assert!(r.stdout.contains("needs standin.toml"), "{}", r.stdout);

    let (path, _keep) = profile(
        "[workspace]\nfiles = { \"standin.toml\" = \"require = \\\"src/lib.txt\\\"\\n\", \"src/lib.txt\" = \"x\" }\n",
    );
    let r = with_profile(&["needs-workspace-file"], &path);
    only_fails(&r, &[]);
    assert_eq!(r.status, 0, "{}", r.stdout);
}

#[test]
fn a_server_that_needs_a_workspace_file_passes_with_a_fixture_and_replays() {
    // The fixture is found relative to the profile, and its nested file is
    // copied too (the stand-in requires it).
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/standin-workspace.toml");
    let dir = tempfile::tempdir().unwrap();
    let t = dir.path().join("t");
    let server = [STANDIN, "--fault", "needs-workspace-file"];
    let r = run_command_in(&["--profile", path.to_str().unwrap()], &server, None, t.clone());
    only_fails(&r, &[]);
    assert_eq!(r.status, 0, "{}", r.stdout);
    // The transcript records the fixture's files, so a replay recreates them
    // and the server initializes there as it did.
    let transcript = t.join("lifecycle.jsonl");
    let first = std::fs::read_to_string(&transcript).unwrap();
    let workspace: Value = first
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["workspace"].is_string())
        .unwrap();
    assert!(workspace["files"]["src/nested/lib.txt"].is_string(), "{workspace}");
    let stdout = replays_alike(&transcript, "needs-workspace-file");
    assert!(stdout.contains("result -> result"), "{stdout}");
}

#[test]
fn a_server_that_needs_settings_fails_without_them_and_passes_and_replays_with_them() {
    let r = fault("needs-configuration");
    expect(&r, "J4", "fail");
    assert_eq!(r.status, 1, "{}", r.stdout);
    assert!(r.stdout.contains("isn't configured"), "{}", r.stdout);

    let (path, _keep) = profile("[client.configuration.standin]\nmode = \"ready\"\n");
    let dir = tempfile::tempdir().unwrap();
    let t = dir.path().join("t");
    let server = [STANDIN, "--fault", "needs-configuration"];
    let r = run_command_in(&["--profile", path.to_str().unwrap()], &server, None, t.clone());
    only_fails(&r, &[]);
    assert_eq!(r.status, 0, "{}", r.stdout);
    // The answer was looked up by section, and the settings were also sent
    // after `initialized`.
    assert!(r.server_stderr.contains("configuration answered with [\"ready\"]"), "{}", r.server_stderr);
    let transcript = t.join("lifecycle.jsonl");
    let text = std::fs::read_to_string(&transcript).unwrap();
    assert!(text.contains("workspace/didChangeConfiguration"), "{text}");
    // A replay answers the server's question from the recorded settings, so
    // it is served as it was.
    let stdout = replays_alike(&transcript, "needs-configuration");
    assert!(stdout.contains("error -32601 -> error -32601"), "{stdout}");
}

#[test]
fn the_profiles_command_is_used_when_none_follows_the_dashes() {
    let (path, _keep) =
        profile(&format!("[server]\ncommand = [{}, \"--fault\", \"wrong-unknown-error\"]\n", literal(STANDIN)));
    let r = run_command(&["--profile", path.to_str().unwrap()], &[], None);
    only_fails(&r, &["J4"]);
    assert_eq!(r.report["server"][2], "wrong-unknown-error");
    // A command after `--` wins.
    let r = run_command(&["--profile", path.to_str().unwrap()], &[STANDIN], None);
    only_fails(&r, &[]);
}

#[cfg(unix)]
#[test]
fn allowed_descendants_dont_fail_l9_but_are_named() {
    // A name not on the list still fails, as without a profile.
    let (path, _keep) = profile("[allow]\ndescendants = [\"some-other-daemon\"]\n");
    let r = with_profile(&["orphan-child"], &path);
    only_fails(&r, &["L9"]);
    // The name lspcheck reports for the leftover: the kernel's 15-byte command
    // name on Linux, or an executable path from ps on macOS.
    let detail = r.report["results"].as_array().unwrap().iter().find(|c| c["id"] == "L9").unwrap()["detail"]
        .as_str()
        .unwrap()
        .to_string();
    let name = detail.split_once(" (").and_then(|(_, rest)| rest.split_once(')')).map(|(n, _)| n).unwrap();
    let basename = std::path::Path::new(name).file_name().unwrap().to_str().unwrap();
    assert!(basename.starts_with("lspcheck-stand"), "{detail}");

    let (path, _keep) = profile(&format!("[allow]\ndescendants = [{name:?}]\n"));
    let r = with_profile(&["orphan-child"], &path);
    only_fails(&r, &[]);
    assert_eq!(r.status, 0, "{}", r.stdout);
    assert!(r.stdout.contains("allowed by the profile to outlive the server"), "{}", r.stdout);
    assert!(r.stdout.contains(&format!("({name})")), "{}", r.stdout);
}

#[test]
fn a_skipped_check_is_reported_as_skip_and_hides_nothing_else() {
    // J4 fails on this fault and L9 is cross-cutting; both are skipped.
    let (path, _keep) = profile(
        "[[allow.skip]]\nid = \"J4\"\nreason = \"answers unknown requests its own way, see issue 7\"\n\
         [[allow.skip]]\nid = \"L9\"\nreason = \"helpers are cleaned up by the editor\"\n",
    );
    let r = with_profile(&["wrong-unknown-error", "exit-status"], &path);
    passes_except(&r, &[("J4", "skip"), ("L9", "skip"), ("L6", "fail"), ("L7", "fail")]);
    assert!(
        r.stdout.contains("skipped by the profile: answers unknown requests its own way, see issue 7"),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("skipped by the profile: helpers are cleaned up by the editor"), "{}", r.stdout);
    // The skipped MUST failure doesn't count; L6 and L7 are SHOULD checks.
    assert_eq!(r.status, 0, "{}", r.stdout);
    assert_eq!(r.verdicts.len(), lspcheck::checks::CATALOGUE.len());
}

#[test]
fn a_scenario_whose_checks_are_all_skipped_or_unselected_doesnt_run() {
    let (path, _keep) = profile("[allow]\nskip = [{ id = \"L7\", reason = \"exits 0 by design\" }]\n");
    let dir = tempfile::tempdir().unwrap();
    let t = dir.path().join("t");
    let r = run_command_in(&["--profile", path.to_str().unwrap(), "--only", "L6,L7"], &[STANDIN], None, t.clone());
    assert_eq!(r.verdicts.len(), 2, "{}", r.stdout);
    expect(&r, "L6", "pass");
    expect(&r, "L7", "skip");
    assert!(t.join("lifecycle.jsonl").exists());
    assert!(!t.join("exit-without-shutdown.jsonl").exists(), "L7's scenario ran");
}

#[test]
fn a_bad_profile_is_a_usage_error_before_any_server_starts() {
    let (path, keep) = profile("[server]\nrequest_timeout = 30\n");
    let transcripts = keep.path().join("t");
    let out = Command::new(LSPCHECK)
        .args(["--profile", path.to_str().unwrap(), "--transcripts", transcripts.to_str().unwrap(), "--", STANDIN])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("server.request_timeout") && stderr.contains("line 2"), "{stderr}");
    assert!(stderr.contains(&path.display().to_string()), "{stderr}");
    assert!(out.stdout.is_empty(), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(!transcripts.exists(), "nothing was set up for a run");
}

#[cfg(unix)]
#[test]
fn profile_regression_symlink_roots_are_usage_errors_before_startup() {
    let (path, keep) = profile("");
    let target = keep.path().join("target");
    std::fs::create_dir(&target).unwrap();
    std::os::unix::fs::symlink(&target, keep.path().join("fixture")).unwrap();
    for spelling in ["fixture", "fixture/", "fixture/."] {
        std::fs::write(&path, format!("[workspace]\npath = {spelling:?}\n")).unwrap();
        let transcripts = keep.path().join("t");
        let out = Command::new(LSPCHECK)
            .arg("--profile")
            .arg(&path)
            .arg("--transcripts")
            .arg(&transcripts)
            .args(["--only", "L2", "--", STANDIN])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{spelling}");
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(error.contains("fixture") && error.contains("symbolic link"), "{error}");
        assert!(!transcripts.exists());
    }
}

#[test]
fn profile_regression_stream_names_are_usage_errors_before_startup() {
    for name in ["missing.txt:payload", "dir:stream/file.txt", "a/NUL.txt", "a/file.", "a/file "] {
        let (path, keep) = profile(&format!("[workspace]\nfiles = {{ {name:?} = \"x\" }}\n"));
        let transcripts = keep.path().join("t");
        let out = Command::new(LSPCHECK)
            .arg("--profile")
            .arg(&path)
            .arg("--transcripts")
            .arg(&transcripts)
            .args(["--only", "L2", "--", STANDIN])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{name}");
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(error.contains("workspace.files") && error.contains(name), "{error}");
        assert!(!transcripts.exists());
    }
}

#[test]
fn profile_regression_binary_fixtures_and_empty_directories_replay() {
    let (path, keep) = profile("[workspace]\npath = \"fixture\"\n");
    let fixture = keep.path().join("fixture");
    std::fs::create_dir_all(fixture.join("assets")).unwrap();
    std::fs::create_dir_all(fixture.join("cache/empty")).unwrap();
    std::fs::write(fixture.join("assets/payload.bin"), [255]).unwrap();
    std::fs::write(
        fixture.join("standin.toml"),
        "require = \"assets/payload.bin\"\nrequire_directory = \"cache/empty\"\n",
    )
    .unwrap();
    let transcripts = keep.path().join("t");
    let server = [STANDIN, "--fault", "needs-workspace-file"];
    let r =
        run_command_in(&["--profile", path.to_str().unwrap(), "--only", "L2,L6"], &server, None, transcripts.clone());
    only_fails(&r, &[]);
    assert_eq!(r.status, 0, "{}", r.stdout);
    let transcript = transcripts.join("lifecycle.jsonl");
    let stdout = replays_alike(&transcript, "needs-workspace-file");
    assert!(stdout.contains("result -> result"), "{stdout}");
    let recording = std::fs::read_to_string(&transcript).unwrap();
    let workspace: Value = recording
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|event| event["workspace"].is_string())
        .unwrap();
    assert_eq!(workspace["files"]["assets/payload.bin"], serde_json::json!({"encoding": "base64", "data": "/w=="}));
    assert!(workspace["directories"].as_array().unwrap().contains(&Value::String("cache/empty".into())));
}

#[cfg(unix)]
#[test]
fn profile_regression_executable_fixture_permissions_replay() {
    use std::os::unix::fs::PermissionsExt;

    // A server may require executable project helpers before initializing.
    // Recreating their bytes alone must not change initialization's result.
    let (path, keep) = profile("[workspace]\npath = \"fixture\"\nfiles = { \"settings.txt\" = \"override\" }\n");
    let fixture = keep.path().join("fixture");
    std::fs::create_dir_all(fixture.join("bin")).unwrap();
    std::fs::create_dir_all(fixture.join("cache/empty")).unwrap();
    std::fs::write(fixture.join("payload.bin"), [255]).unwrap();
    std::fs::write(fixture.join("settings.txt"), "original").unwrap();
    let helper = fixture.join("bin/helper");
    std::fs::write(&helper, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        fixture.join("standin.toml"),
        "require_executable = \"bin/helper\"\nrequire = \"payload.bin\"\nrequire_directory = \"cache/empty\"\n",
    )
    .unwrap();
    let transcripts = keep.path().join("t");
    let server = [STANDIN, "--fault", "needs-workspace-file"];
    let r =
        run_command_in(&["--profile", path.to_str().unwrap(), "--only", "L2,L6"], &server, None, transcripts.clone());
    expect(&r, "L2", "pass");
    assert_eq!(r.status, 0, "{}", r.stdout);
    for recording in ["lifecycle.jsonl", "lifecycle.replay.jsonl"] {
        let stdout = replays_alike(&transcripts.join(recording), "needs-workspace-file");
        assert!(stdout.contains("result -> result"), "{stdout}");
    }
}

#[test]
fn profile_case_collisions_are_usage_errors_before_startup() {
    let (path, keep) = profile("");
    let fixture = keep.path().join("fixture");
    std::fs::create_dir_all(fixture.join("src")).unwrap();
    std::fs::write(fixture.join("src/Settings.json"), "original").unwrap();
    for name in ["src/settings.json", "SRC/other.json", "SRC", "src/SETTINGS.JSON/child.txt"] {
        std::fs::write(&path, format!("[workspace]\npath = \"fixture\"\nfiles = {{ {name:?} = \"override\" }}\n"))
            .unwrap();
        let transcripts = keep.path().join("t");
        let out = Command::new(LSPCHECK)
            .arg("--profile")
            .arg(&path)
            .arg("--transcripts")
            .arg(&transcripts)
            .args(["--only", "L2", "--", STANDIN])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{name}");
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(error.contains("src/Settings.json") && error.contains(name) && error.contains("ASCII case"), "{error}");
        assert!(!transcripts.exists());
    }
}

#[test]
fn profile_case_collisions_in_recordings_are_usage_errors() {
    for (files, directories, left, right) in [
        (
            serde_json::json!({"Settings.json": "one", "settings.json": "two"}),
            serde_json::json!([]),
            "Settings.json",
            "settings.json",
        ),
        (serde_json::json!({"src/file.txt": "one"}), serde_json::json!(["SRC/empty"]), "src/file.txt", "SRC/empty"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspace");
        let transcript = dir.path().join("recording.jsonl");
        std::fs::write(
            &transcript,
            serde_json::json!({"dir": "meta", "workspace": root, "files": files, "directories": directories})
                .to_string(),
        )
        .unwrap();
        let out = replay(&transcript, &[], &[STANDIN]);
        assert_eq!(out.status.code(), Some(2));
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(error.contains(left) && error.contains(right) && error.contains("ASCII case"), "{error}");
        assert!(!root.exists());
    }
}

#[test]
fn profile_case_exact_fixture_overrides_record_once_and_replay() {
    let (path, keep) = profile(
        "[workspace]\npath = \"fixture\"\nfiles = { \"Settings.json\" = \"new top\", \"src/Settings.json\" = \"new nested\" }\n",
    );
    let fixture = keep.path().join("fixture");
    std::fs::create_dir_all(fixture.join("src")).unwrap();
    for name in ["Settings.json", "src/Settings.json"] {
        std::fs::write(fixture.join(name), "old").unwrap();
    }
    std::fs::write(fixture.join("standin.toml"), "require = \"Settings.json\"\nrequire = \"src/Settings.json\"\n")
        .unwrap();
    let transcripts = keep.path().join("t");
    let server = [STANDIN, "--fault", "needs-workspace-file"];
    let r =
        run_command_in(&["--profile", path.to_str().unwrap(), "--only", "L2,L6"], &server, None, transcripts.clone());
    only_fails(&r, &[]);
    assert_eq!(r.status, 0, "{}", r.stdout);
    let transcript = transcripts.join("lifecycle.jsonl");
    let recording = std::fs::read_to_string(&transcript).unwrap();
    let workspace: Value = recording
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|event| event["workspace"].is_string())
        .unwrap();
    let files = workspace["files"].as_object().unwrap();
    assert_eq!(files.len(), 3);
    assert_eq!(files["Settings.json"], "new top");
    assert_eq!(files["src/Settings.json"], "new nested");
    replays_alike(&transcript, "needs-workspace-file");
}

#[cfg(unix)]
#[test]
fn profile_readonly_fixture_override_starts_server_and_replays() {
    use std::os::unix::fs::PermissionsExt;

    let manifest = "require = \"settings.json\"\nrequire = \"src/settings.json\"\n";
    let (path, keep) = profile(&format!(
        "[workspace]\npath = \"fixture\"\nfiles = {{ \"standin.toml\" = {}, \"settings.json\" = \"new top\", \"src/settings.json\" = \"new nested\", \"added.txt\" = \"fresh\" }}\n",
        serde_json::to_string(manifest).unwrap(),
    ));
    let fixture = keep.path().join("fixture");
    std::fs::create_dir_all(fixture.join("src")).unwrap();
    for (name, text) in [
        ("standin.toml", "require = \"missing-payload\"\n"),
        ("settings.json", "original top"),
        ("src/settings.json", "original nested"),
    ] {
        let source = fixture.join(name);
        std::fs::write(&source, text).unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o444)).unwrap();
    }
    let transcripts = keep.path().join("t");
    let server = [STANDIN, "--fault", "needs-workspace-file"];
    let r =
        run_command_in(&["--profile", path.to_str().unwrap(), "--only", "L2,L6"], &server, None, transcripts.clone());
    expect(&r, "L2", "pass");
    assert_eq!(r.status, 0, "{}", r.stdout);
    let transcript = transcripts.join("lifecycle.jsonl");
    let recording = std::fs::read_to_string(&transcript).unwrap();
    let workspace: Value = recording
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|event| event["workspace"].is_string())
        .unwrap();
    for (name, text) in [("standin.toml", manifest), ("settings.json", "new top"), ("src/settings.json", "new nested")]
    {
        assert_eq!(workspace["files"][name], text);
        assert_eq!(workspace["file_modes"][name], workspace["file_modes"]["added.txt"]);
        assert_eq!(std::fs::metadata(fixture.join(name)).unwrap().permissions().mode() & 0o777, 0o444);
    }
    replays_alike(&transcript, "needs-workspace-file");
}
