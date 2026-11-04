// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! Transcript files: one JSON object per line, in the order things happened.
//!
//! `out` lines hold bytes that reached the server's stdin, `in` lines the
//! bytes read from its stdout exactly as read, `parsed` lines what the frame
//! reader made of them, `err` lines its stderr (the first MiB), and `meta`
//! lines exits and lspcheck's own notes. Bytes are a `data` string when they
//! are UTF-8, otherwise `data_base64`.

use crate::session::{Event, EventKind};
use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;

/// Writes `events` to `path` as JSON Lines.
pub fn write(path: &Path, events: &[Event]) -> std::io::Result<()> {
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    for e in events {
        let t = e.at.as_secs_f64() * 1000.0;
        let line = match &e.kind {
            EventKind::Sent(b) => with_bytes(json!({"t_ms": t, "dir": "out"}), b),
            EventKind::Read(b) => with_bytes(json!({"t_ms": t, "dir": "in"}), b),
            EventKind::Parsed { kind, detail } => {
                json!({"t_ms": t, "dir": "parsed", "kind": kind, "detail": detail})
            }
            EventKind::Stderr(b) => with_bytes(json!({"t_ms": t, "dir": "err"}), b),
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

fn with_bytes(mut v: Value, bytes: &[u8]) -> Value {
    let obj = v.as_object_mut().expect("an object");
    match std::str::from_utf8(bytes) {
        Ok(text) => obj.insert("data".into(), text.into()),
        Err(_) => obj.insert("data_base64".into(), base64(bytes).into()),
    };
    v
}

fn base64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
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
