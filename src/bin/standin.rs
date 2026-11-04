// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! A minimal, correct language server for lspcheck's own tests, with faults
//! that can be switched on one at a time: `lspcheck-standin --fault NAME`.
//! Each check has a fault that must make it fail, and some faults are
//! lawful-but-unusual behaviour that must *not* make anything fail.
//!
//! It deliberately doesn't use lspcheck's library, so a bug there can't hide
//! the same bug here.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};

const FAULTS: &[(&str, &str)] = &[
    ("stdout-banner", "print a line to stdout before the first frame"),
    ("preinit-banner", "print a line to stdout when a request arrives before initialize"),
    ("nul-header", "send one notification with a NUL byte in its Content-Type header"),
    ("all-nul-headers", "put a NUL byte in every frame's Content-Type header"),
    ("bad-content-type", "send one notification whose Content-Type isn't a media type"),
    ("latin1", "declare charset=iso-8859-1 in a Content-Type header"),
    ("extension-header", "lawful: send an X-Trace header before Content-Length"),
    ("long-header", "lawful: send a 200 KiB X-Pad header with every frame"),
    ("decimal-ids", "lawful: spell numeric response ids with a fraction, as 2.0"),
    ("flood", "write 2 MiB of log messages before replying to initialize"),
    ("big-frame", "lawful: send one 2 MiB log message before replying to initialize"),
    ("init-error", "lawful: refuse to initialize with an InitializeError"),
    ("block-after-init", "after initialize, send a huge configuration request and stop reading"),
    ("block-on-config", "on initialize, send a huge configuration request and stop reading"),
    ("no-pre-init-error", "answer requests before initialize normally"),
    ("bad-encoding", "choose a position encoding the client didn't offer"),
    ("no-capabilities", "leave capabilities out of the initialize result"),
    ("early-replies", "answer guessed request ids before they are sent, then answer unknown requests wrongly"),
    ("wrong-unknown-error", "answer unknown requests with InvalidRequest"),
    ("ignore-dollar-request", "never answer an unknown $/ request"),
    ("shutdown-result", "return {} from shutdown"),
    ("exit-on-shutdown", "exit right after replying to shutdown"),
    ("big-frame-on-exit", "on exit, send one 2 MiB log message before exiting"),
    ("exit-flood", "on exit, write 100,000 small log messages and then a line outside any frame"),
    ("exit-status", "exit with status 3 on exit"),
    ("ignore-exit", "ignore exit and stay running"),
    ("orphan-child", "start a child process that outlives the server"),
    ("escaped-child", "start a child in a new session, with a non-UTF-8 name, that outlives the server"),
];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--child") => return child(false),
        Some("--escaped-child") => return child(true),
        Some("--relay") => return relay(args[1].parse().unwrap()),
        _ => {}
    }
    let mut faults = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--fault" => {
                let name = it.next().expect("--fault needs a name");
                assert!(FAULTS.iter().any(|(f, _)| f == name), "unknown fault {name}");
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
    let mut out = Writer { out: std::io::stdout().lock(), faults: faults.clone() };
    if fault("stdout-banner") {
        out.raw(b"standin starting\n");
    }
    let mut input = BufReader::new(std::io::stdin().lock());
    let (mut initialized, mut shut_down) = (false, false);
    let mut children: Vec<Child> = Vec::new();
    while let Some(msg) = read_frame(&mut input) {
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("").to_string();
        let Some(id) = msg.get("id").cloned() else {
            // A notification.
            if method == "exit" && !fault("ignore-exit") {
                if fault("big-frame-on-exit") {
                    let text = "x".repeat(2 * 1024 * 1024);
                    out.frame(&json!({"jsonrpc": "2.0", "method": "window/logMessage", "params": {"type": 4, "message": text}}));
                }
                if fault("exit-flood") {
                    let log =
                        json!({"jsonrpc": "2.0", "method": "window/logMessage", "params": {"type": 4, "message": "x"}});
                    let mut bytes = out.encode(&log).repeat(100_000);
                    bytes.extend_from_slice(b"goodbye\n");
                    out.raw(&bytes);
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
        if method == "initialize" {
            if fault("orphan-child") {
                children.push(spawn_child("--child"));
            }
            if fault("escaped-child") {
                children.push(spawn_child("--escaped-child"));
            }
            if fault("flood") || fault("big-frame") {
                let (count, size) = if fault("flood") { (8, 256 * 1024) } else { (1, 2 * 1024 * 1024) };
                let text = "x".repeat(size);
                for _ in 0..count {
                    out.frame(&json!({"jsonrpc": "2.0", "method": "window/logMessage", "params": {"type": 4, "message": text}}));
                }
            }
            if fault("nul-header") {
                let body = br#"{"jsonrpc":"2.0","method":"window/logMessage","params":{"type":4,"message":"x"}}"#;
                out.raw(
                    format!("Content-Length: {}\r\nContent-Type: application/vscode-jsonrpc\x00\r\n\r\n", body.len())
                        .as_bytes(),
                );
                out.raw(body);
            }
            if fault("bad-content-type") {
                let body = br#"{"jsonrpc":"2.0","method":"window/logMessage","params":{"type":4,"message":"x"}}"#;
                out.raw(format!("Content-Length: {}\r\nContent-Type: utf-8\r\n\r\n", body.len()).as_bytes());
                out.raw(body);
            }
            if fault("block-on-config") {
                let items = vec![json!({"section": "x"}); 40_000];
                out.frame(&json!({"jsonrpc": "2.0", "id": "cfg", "method": "workspace/configuration", "params": {"items": items}}));
                std::thread::sleep(std::time::Duration::from_secs(60));
            }
            if fault("init-error") {
                out.frame(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32001, "message": "Required SDK is unavailable", "data": {"retry": false}}}));
                continue;
            }
            initialized = true;
            let encoding = if fault("bad-encoding") { "utf-7" } else { "utf-16" };
            let mut result = json!({
                "capabilities": {"positionEncoding": encoding, "textDocumentSync": 1, "hoverProvider": true},
                "serverInfo": {"name": "lspcheck-standin"}
            });
            if fault("no-capabilities") {
                result = json!({"serverInfo": {"name": "lspcheck-standin"}});
            }
            out.frame(&ok(&id, result));
            if fault("block-after-init") {
                let items = vec![json!({"section": "x"}); 40_000];
                out.frame(&json!({"jsonrpc": "2.0", "id": "cfg", "method": "workspace/configuration", "params": {"items": items}}));
                std::thread::sleep(std::time::Duration::from_secs(60));
            }
            if fault("early-replies")
                && let Some(n) = id.as_i64()
            {
                for next in [n + 1, n + 2] {
                    out.frame(&err(&json!(next), -32601, "method not found"));
                }
            }
            continue;
        }
        if !initialized && !fault("no-pre-init-error") {
            if fault("preinit-banner") {
                out.raw(b"pre-init server log\n");
            }
            out.frame(&err(&id, -32002, "not initialized"));
            continue;
        }
        if shut_down {
            out.frame(&err(&id, -32600, "shutting down"));
            continue;
        }
        match method.as_str() {
            "shutdown" => {
                shut_down = true;
                let result = if fault("shutdown-result") { json!({}) } else { Value::Null };
                if fault("exit-on-shutdown") {
                    // Exit first and have a helper deliver the reply, so the
                    // server is certainly gone by the time a client reads it.
                    let mut helper = Command::new(std::env::current_exe().unwrap())
                        .arg("--relay")
                        .arg(std::process::id().to_string())
                        .stdin(Stdio::piped())
                        .stdout(Stdio::inherit())
                        .stderr(Stdio::null())
                        .spawn()
                        .unwrap();
                    // The helper's stdin stays open until this process is
                    // gone, which is how the helper knows it may deliver.
                    let mut pipe = helper.stdin.take().unwrap();
                    pipe.write_all(&out.encode(&ok(&id, result))).unwrap();
                    pipe.flush().unwrap();
                    std::process::exit(0);
                }
                out.frame(&ok(&id, result));
            }
            "textDocument/hover" | "workspace/symbol" => out.frame(&ok(&id, Value::Null)),
            _ if fault("early-replies") => out.frame(&ok(&id, Value::Null)),
            m if m.starts_with("$/") && fault("ignore-dollar-request") => {}
            _ if fault("wrong-unknown-error") => out.frame(&err(&id, -32600, "unknown")),
            _ => out.frame(&err(&id, -32601, "method not found")),
        }
    }
    // End of input without exit.
    drop(children);
    std::process::exit(1);
}

/// A leftover child: it signals that it's ready, then sleeps for a minute.
fn child(escape: bool) {
    #[cfg(unix)]
    if escape {
        let _ = nix::unistd::setsid();
        #[cfg(target_os = "linux")]
        let _ = nix::sys::prctl::set_name(c"\xffstandin");
    }
    let _ = escape;
    println!("ready");
    let _ = std::io::stdout().flush();
    std::thread::sleep(std::time::Duration::from_secs(60));
}

/// Writes its input to stdout once `parent` has exited: its stdin ends when
/// the parent's handle closes, and on Unix it also waits to be reparented.
fn relay(parent: u32) {
    let mut bytes = Vec::new();
    std::io::stdin().read_to_end(&mut bytes).unwrap();
    #[cfg(unix)]
    {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while nix::unistd::getppid().as_raw() as u32 == parent && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    #[cfg(not(unix))]
    let _ = parent;
    let mut out = std::io::stdout().lock();
    out.write_all(&bytes).unwrap();
    out.flush().unwrap();
}

/// Starts a leftover child and waits until it has set itself up.
fn spawn_child(arg: &str) -> Child {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg(arg)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = [0u8; 6];
    child.stdout.take().unwrap().read_exact(&mut line).unwrap();
    assert_eq!(&line, b"ready\n");
    child
}

struct Writer<W: Write> {
    out: W,
    faults: Vec<String>,
}

impl<W: Write> Writer<W> {
    fn raw(&mut self, bytes: &[u8]) {
        self.out.write_all(bytes).unwrap();
        self.out.flush().unwrap();
    }

    fn frame(&mut self, v: &Value) {
        let bytes = self.encode(v);
        self.out.write_all(&bytes).unwrap();
        self.out.flush().unwrap();
    }

    /// A frame with the header variants the faults ask for.
    fn encode(&self, v: &Value) -> Vec<u8> {
        let fault = |name: &str| self.faults.iter().any(|f| f == name);
        let mut v = v.clone();
        if fault("decimal-ids")
            && let Some(n) = v.get("id").and_then(Value::as_i64)
            && v.get("method").is_none()
        {
            v["id"] = serde_json::from_str(&format!("{n}.0")).unwrap();
        }
        let body = serde_json::to_vec(&v).unwrap();
        let mut header = Vec::new();
        if fault("extension-header") {
            header.extend_from_slice(b"X-Trace: standin\r\n");
        }
        if fault("long-header") {
            header.extend_from_slice(format!("X-Pad: {}\r\n", "p".repeat(200 * 1024)).as_bytes());
        }
        header.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
        if fault("all-nul-headers") {
            header.extend_from_slice(b"Content-Type: application/vscode-jsonrpc\x00\r\n");
        }
        if fault("latin1") {
            header.extend_from_slice(b"Content-Type: application/vscode-jsonrpc; charset=iso-8859-1\r\n");
        }
        header.extend_from_slice(b"\r\n");
        header.extend_from_slice(&body);
        header
    }
}

fn ok(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn err(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
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
