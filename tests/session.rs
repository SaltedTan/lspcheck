// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! Session-level guarantees, driven through the library.

#![cfg(all(unix, feature = "standin"))]

use lspcheck::process::ServerCommand;
use lspcheck::session::{Options, Session};
use std::ffi::OsString;
use std::time::Duration;

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
