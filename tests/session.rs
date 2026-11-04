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
