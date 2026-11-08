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
    write_events(std::fs::File::create(path)?, events)
}

/// Writes `events` to a new path as JSON Lines, refusing to replace anything.
pub fn write_new(path: &Path, events: &[Event]) -> std::io::Result<()> {
    let file = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    write_events(file, events)
}

fn write_events(file: std::fs::File, events: &[Event]) -> std::io::Result<()> {
    let mut out = std::io::BufWriter::new(file);
    // Reads can be appended while a write is flushing. Its timestamp marks
    // when it began, so restore event order before replay consumes the lines.
    let mut events: Vec<_> = events.iter().collect();
    events.sort_by_key(|event| event.at);
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
            EventKind::StdinClosed => json!({"t_ms": t, "dir": "meta", "event": "stdin closed"}),
            EventKind::Exited(how) => {
                json!({"t_ms": t, "dir": "meta", "event": "exited", "how": how})
            }
            EventKind::Note(text) => json!({"t_ms": t, "dir": "meta", "note": text}),
            EventKind::Workspace { path, files } => {
                let mut v = json!({"t_ms": t, "dir": "meta", "workspace": path.to_string_lossy()});
                let files: serde_json::Map<String, Value> =
                    files.iter().map(|(name, text)| (name.clone(), Value::String(text.clone()))).collect();
                v["files"] = Value::Object(files);
                v
            }
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

pub fn base64(bytes: &[u8]) -> String {
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

/// Decodes a base64 string, as used in the `data_base64` transcript field.
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<u8> = text.bytes().filter(|&b| b != b'=').collect();
    if bytes.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut n = 0;
    for b in &bytes {
        let v = A.iter().position(|&c| c == *b)?;
        acc = (acc << 6) | v as u32;
        n += 1;
        if n == 4 {
            n = 0;
            out.push(((acc >> 16) & 0xFF) as u8);
            out.push(((acc >> 8) & 0xFF) as u8);
            out.push((acc & 0xFF) as u8);
        }
    }
    match n {
        2 => out.push(((acc >> 4) & 0xFF) as u8),
        3 => {
            out.push(((acc >> 10) & 0xFF) as u8);
            out.push(((acc >> 2) & 0xFF) as u8);
        }
        _ => {}
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Id, request};
    use crate::transport;
    use std::time::Duration;

    #[test]
    fn replies_read_during_a_flush_follow_their_request_in_both_transcript_writers() {
        let request = transport::encode(&serde_json::to_vec(&request(&Id::int(7), "unknown", None)).unwrap());
        let reply = transport::encode(
            &serde_json::to_vec(&json!({"jsonrpc": "2.0", "id": 7, "error": {"code": -32601}})).unwrap(),
        );
        let split = request.len() / 2;
        // The public JSONL contract is event-time order. A write is timestamped
        // when it begins but appended only after flushing; the reader can
        // append its reply first. Split writes must retain their byte order.
        let events = [
            Event { at: Duration::ZERO, kind: EventKind::Sent(request[..split].to_vec()) },
            Event { at: Duration::from_millis(20), kind: EventKind::Read(reply) },
            Event { at: Duration::from_millis(10), kind: EventKind::Sent(request[split..].to_vec()) },
            Event { at: Duration::from_millis(30), kind: EventKind::StdinClosed },
            Event { at: Duration::from_millis(30), kind: EventKind::Note("after stdin closed".into()) },
        ];
        let dir = tempfile::tempdir().unwrap();
        for writer in [write, write_new] {
            let path = dir.path().join("transcript.jsonl");
            writer(&path, &events).unwrap();
            // Replay is the real consumer: JSON-RPC's matching-id reply must
            // count, and must gate stdin closure rather than be lost.
            let plan = crate::replay::parse(&path).unwrap();
            assert_eq!(plan.requests.len(), 1);
            assert_eq!(plan.requests[0].id, Id::int(7));
            assert_eq!(plan.requests[0].recorded.as_deref(), Some("error -32601"));
            let close = plan.stdin_close.unwrap();
            assert_eq!(close.after_steps, 2);
            assert_eq!(close.gates, [Id::int(7)]);
            let lines: Vec<Value> = std::fs::read_to_string(&path)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(
                lines.iter().map(|line| line["t_ms"].as_f64().unwrap()).collect::<Vec<_>>(),
                [0.0, 10.0, 20.0, 30.0, 30.0],
            );
            assert_eq!(lines[3]["event"], "stdin closed", "equal timestamps retain insertion order");
            assert_eq!(lines[4]["note"], "after stdin closed");
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn base64_round_trips() {
        for input in [b"".as_slice(), b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar", &[0, 1, 2, 127, 128, 255]] {
            assert_eq!(&super::base64_decode(&super::base64(input)).expect("decodes"), input);
        }
        assert!(super::base64_decode("abcde").is_none(), "a length mod 4 of 1 is not base64");
    }

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
