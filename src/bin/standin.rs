// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! A minimal, correct language server for lspcheck's own tests, with faults
//! that can be switched on one at a time: `lspcheck-standin --fault NAME`.
//! Each check has a fault that must make it fail.
//!
//! It deliberately doesn't use lspcheck's library, so a bug there can't hide
//! the same bug here.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

const FAULTS: &[(&str, &str)] = &[
    (
        "stdout-banner",
        "print a line to stdout before the first frame",
    ),
    (
        "no-pre-init-error",
        "answer requests before initialize normally",
    ),
    (
        "bad-encoding",
        "choose a position encoding the client didn't offer",
    ),
    (
        "no-capabilities",
        "leave capabilities out of the initialize result",
    ),
    (
        "wrong-unknown-error",
        "answer unknown requests with InvalidRequest",
    ),
    (
        "ignore-dollar-request",
        "never answer an unknown $/ request",
    ),
    ("shutdown-result", "return {} from shutdown"),
    ("exit-on-shutdown", "exit right after replying to shutdown"),
    ("exit-status", "exit with status 3 on exit"),
    ("ignore-exit", "ignore exit and stay running"),
    (
        "orphan-child",
        "start a child process that outlives the server",
    ),
    (
        "escaped-child",
        "start a child in a new session that outlives the server",
    ),
];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--sleep") {
        std::thread::sleep(std::time::Duration::from_secs(60));
        return;
    }
    if args.first().map(String::as_str) == Some("--sleep-setsid") {
        #[cfg(unix)]
        let _ = nix::unistd::setsid();
        std::thread::sleep(std::time::Duration::from_secs(60));
        return;
    }
    let mut faults = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--fault" => {
                let name = it.next().expect("--fault needs a name");
                assert!(
                    FAULTS.iter().any(|(f, _)| f == name),
                    "unknown fault {name}"
                );
                faults.push(name.clone());
            }
            "--list-faults" => {
                for (f, what) in FAULTS {
                    println!("{f:<24} {what}");
                }
                return;
            }
            other => panic!("unknown argument {other}"),
        }
    }
    let fault = |name: &str| faults.iter().any(|f| f == name);
    let mut out = std::io::stdout().lock();
    if fault("stdout-banner") {
        writeln!(out, "standin starting").unwrap();
        out.flush().unwrap();
    }
    let mut input = BufReader::new(std::io::stdin().lock());
    let (mut initialized, mut shut_down) = (false, false);
    let mut children = Vec::new();
    while let Some(msg) = read_frame(&mut input) {
        let id = msg.get("id").cloned();
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let Some(id) = id else {
            // A notification.
            if method == "exit" {
                if fault("ignore-exit") {
                    continue;
                }
                let status = if fault("exit-status") {
                    3
                } else if shut_down {
                    0
                } else {
                    1
                };
                std::process::exit(status);
            }
            continue;
        };
        let reply = if method == "initialize" {
            if fault("orphan-child") {
                children.push(spawn_self("--sleep"));
            }
            if fault("escaped-child") {
                children.push(spawn_self("--sleep-setsid"));
            }
            initialized = true;
            let encoding = if fault("bad-encoding") {
                "utf-7"
            } else {
                "utf-16"
            };
            let mut result = json!({
                "capabilities": {"positionEncoding": encoding, "textDocumentSync": 1, "hoverProvider": true},
                "serverInfo": {"name": "lspcheck-standin"}
            });
            if fault("no-capabilities") {
                result = json!({"serverInfo": {"name": "lspcheck-standin"}});
            }
            ok(&id, result)
        } else if !initialized && !fault("no-pre-init-error") {
            err(&id, -32002, "not initialized")
        } else if shut_down {
            err(&id, -32600, "shutting down")
        } else {
            match method.as_str() {
                "shutdown" => {
                    shut_down = true;
                    let result = if fault("shutdown-result") {
                        json!({})
                    } else {
                        Value::Null
                    };
                    write_frame(&mut out, &ok(&id, result));
                    if fault("exit-on-shutdown") {
                        std::process::exit(0);
                    }
                    continue;
                }
                "textDocument/hover" | "workspace/symbol" => ok(&id, Value::Null),
                m if m.starts_with("$/") && fault("ignore-dollar-request") => continue,
                _ if fault("wrong-unknown-error") => err(&id, -32600, "unknown"),
                _ => err(&id, -32601, "method not found"),
            }
        };
        write_frame(&mut out, &reply);
    }
    // End of input without exit.
    drop(children);
    std::process::exit(1);
}

fn spawn_self(arg: &str) -> std::process::Child {
    Command::new(std::env::current_exe().unwrap())
        .arg(arg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn ok(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn err(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn write_frame(out: &mut impl Write, v: &Value) {
    let body = serde_json::to_vec(v).unwrap();
    write!(out, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
    out.write_all(&body).unwrap();
    out.flush().unwrap();
}

/// Reads one frame; `None` at end of input.
fn read_frame(input: &mut impl BufRead) -> Option<Value> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0; length?];
    input.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}
