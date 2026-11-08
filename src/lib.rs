// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! lspcheck starts a language server and checks that it speaks the Language
//! Server Protocol correctly: framing, JSON-RPC, lifecycle, and cleanup.
//!
//! The binary is the supported interface; this library is public so the
//! binary and its tests can share it, and its API may change in any release.

pub mod checks;
pub mod documents;
pub mod message;
pub mod process;
pub mod profile;
pub mod replay;
pub mod report;
pub mod session;
pub mod text;
pub mod transcript;
pub mod transport;
