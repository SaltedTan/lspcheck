// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! Session-level guarantees, driven through the library.

#![cfg(all(unix, feature = "standin"))]

use lspcheck::message::Id;
use lspcheck::process::ServerCommand;
use lspcheck::session::{CallError, Options, Session, WaitError, WriteError};
use serde_json::json;
use std::ffi::OsString;
use std::time::Duration;
use tokio::time::Instant;

const STANDIN: &str = env!("CARGO_BIN_EXE_lspcheck-standin");

#[tokio::test(flavor = "current_thread")]
async fn finishing_a_session_leaves_unrelated_children_alone() {
    let mut unrelated = std::process::Command::new("sleep").arg("30").spawn().unwrap();
    let cmd = ServerCommand::new(&[OsString::from(STANDIN)]);
    let session = Session::start(&cmd, Options::default()).await.unwrap();
    let finish = session.finish(Duration::from_millis(200)).await;
    assert!(
        !finish.leftovers.iter().any(|p| p.pid == unrelated.id() as i32),
        "an unrelated child was reported as the server's: {:?}",
        finish.leftovers
    );
    assert!(unrelated.try_wait().unwrap().is_none(), "the unrelated child was stopped");
    unrelated.kill().unwrap();
    unrelated.wait().unwrap();
}

fn standin(args: &[&str]) -> ServerCommand {
    let mut argv = vec![OsString::from(STANDIN)];
    argv.extend(args.iter().map(OsString::from));
    ServerCommand::new(&argv)
}

#[tokio::test(flavor = "current_thread")]
async fn a_reused_id_waits_for_its_new_answer() {
    let mut s = Session::start(&standin(&["--fault", "ignore-dollar-request"]), Options::default()).await.unwrap();
    s.call("initialize", Some(json!({"processId": null, "rootUri": null, "capabilities": {}})), Duration::from_secs(5))
        .await
        .unwrap();
    let id = Id::int(7);
    s.request_with_id(&id, "workspace/symbol", Some(json!({"query": ""}))).await.unwrap();
    assert_eq!(
        s.request_with_id(&id, "workspace/symbol", None).await,
        Err(WriteError::DuplicateId("7".into())),
        "a second request can't reuse an outstanding id"
    );
    s.wait_response(&id, Instant::now() + Duration::from_secs(5)).await.unwrap();
    // Answered: the id may be reused, and the old answer doesn't count for the new request.
    s.request_with_id(&id, "$/never/answered", None).await.unwrap();
    let got = s.wait_response(&id, Instant::now() + Duration::from_millis(500)).await;
    assert_eq!(got.err(), Some(WaitError::Timeout));
    s.finish(Duration::from_millis(200)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_call_to_a_server_that_doesnt_read_ends_by_its_deadline() {
    let cmd = ServerCommand::new(&[OsString::from("sleep"), OsString::from("30")]);
    let mut s = Session::start(&cmd, Options::default()).await.unwrap();
    let big = json!({"pad": "x".repeat(4 * 1024 * 1024)});
    let started = std::time::Instant::now();
    let got = s.call("lspcheck/big", Some(big), Duration::from_millis(500)).await;
    assert_eq!(got.err(), Some(CallError::Write(WriteError::Timeout)));
    assert!(started.elapsed() < Duration::from_secs(3), "took {:?}", started.elapsed());
    s.finish(Duration::from_millis(200)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn queued_output_cant_stretch_a_call_past_its_deadline() {
    // 150,000 log messages already queued when the call starts.
    let script = r#"import sys, time
body = b'{"jsonrpc":"2.0","method":"window/logMessage","params":{"type":4,"message":"x"}}'
sys.stdout.buffer.write((b'Content-Length: %d\r\n\r\n' % len(body) + body) * 150000)
sys.stdout.buffer.flush()
time.sleep(30)
"#;
    let cmd = ServerCommand::new(&[
        OsString::from("python3"),
        OsString::from("-I"),
        OsString::from("-c"),
        OsString::from(script),
    ]);
    let mut s = Session::start(&cmd, Options::default()).await.unwrap();
    // Let the reader queue the backlog.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let started = std::time::Instant::now();
    let got = s.call("lspcheck/test", None, Duration::from_millis(10)).await;
    assert!(got.is_err());
    // Taking in the backlog stops at the deadline instead of consuming all of it.
    assert!(s.messages.len() < 150_000, "consumed {} messages", s.messages.len());
    assert!(started.elapsed() < Duration::from_secs(1), "took {:?}", started.elapsed());
    s.finish(Duration::from_millis(200)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn taking_in_what_was_read_doesnt_wait_on_an_answer_it_cant_write() {
    // On shutdown this server sends its answer, a configuration request whose
    // answer is bigger than a pipe, and an error with id null, then stops
    // reading. Everything it sent must still be recorded, promptly.
    let mut s = Session::start(&standin(&["--fault", "shutdown-block"]), Options::default()).await.unwrap();
    let t = Duration::from_secs(5);
    s.call("initialize", Some(json!({"processId": null, "rootUri": null, "capabilities": {}})), t).await.unwrap();
    s.notify("initialized", Some(json!({}))).await.unwrap();
    s.call("shutdown", None, t).await.unwrap();
    // Let the stdout task read the rest; it is already written.
    tokio::time::sleep(Duration::from_secs(2)).await;
    tokio::time::timeout(Duration::from_secs(5), async { s.take_in() })
        .await
        .expect("take_in waited on an answer the server can't read");
    assert!(
        s.messages.iter().any(|(_, m)| m.value.get("id") == Some(&serde_json::Value::Null)),
        "the reply after the configuration request wasn't recorded"
    );
    // The server may be waiting for that answer, so what it didn't send
    // can't be judged after it.
    assert!(s.incomplete_within(Duration::ZERO, s.elapsed()).is_some(), "an unanswered request went unnoticed");
    s.finish(Duration::from_millis(200)).await;
}
