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
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};

const FAULTS: &[(&str, &str)] = &[
    ("stdout-banner", "print a line to stdout before the first frame"),
    ("preinit-banner", "print a line to stdout when a request arrives before initialize"),
    ("nul-header", "send one notification with a NUL byte in its Content-Type header"),
    ("all-nul-headers", "put a NUL byte in every frame's Content-Type header"),
    ("bad-content-type", "send one notification whose Content-Type isn't a media type"),
    ("latin1", "declare charset=iso-8859-1 in a Content-Type header"),
    ("char-length", "count the Content-Length of outgoing frames in characters, not bytes"),
    ("trailing-junk", "send one non-ASCII notification whose body is JSON then a header and {}, correctly counted"),
    ("newline-after-body", "lawful: end every outgoing body with a newline, counted in its Content-Length"),
    ("bad-bodies", "on initialize, send 70 frames whose body isn't JSON"),
    ("json-into-next-header", "send a correctly counted body that the next frame's first header line would complete"),
    ("json-into-bad-header", "like json-into-next-header, with a space before that header line, making it invalid"),
    ("long-miscount", "send one long non-ASCII log message declaring a Content-Length of 1"),
    ("overlong-last-frame", "on exit, send a non-ASCII log message and newline declaring 10,000 bytes too many"),
    ("ascii-escape", "lawful: escape non-ASCII text in outgoing frames as \\u sequences"),
    ("extension-header", "lawful: send an X-Trace header before Content-Length"),
    ("long-header", "lawful: send a 200 KiB X-Pad header with every frame"),
    ("decimal-ids", "lawful: spell numeric response ids with a fraction, as 2.0"),
    ("flood", "write 2 MiB of log messages before replying to initialize"),
    ("big-frame", "lawful: send one 2 MiB log message before replying to initialize"),
    ("init-error", "lawful: refuse to initialize with an InitializeError"),
    ("block-after-init", "after initialize, send a huge configuration request and stop reading"),
    ("block-on-config", "on initialize, send a huge configuration request and stop reading"),
    ("no-pre-init-error", "answer requests before initialize normally"),
    ("ignore-pre-init-requests", "never answer requests before initialize"),
    ("reply-to-pre-init-notification", "answer a notification before initialize with an error whose id is null"),
    ("late-reply-to-pre-init-notification", "like reply-to-pre-init-notification, after the next request's answer"),
    ("reinitialize", "answer a second initialize as if it were the first"),
    ("exit-on-reinitialize", "exit on a second initialize"),
    ("post-shutdown-results", "answer requests after shutdown normally"),
    ("ignore-eof", "keep running when stdin ends"),
    ("bad-encoding", "choose a position encoding the client didn't offer"),
    ("no-capabilities", "leave capabilities out of the initialize result"),
    ("early-replies", "answer guessed request ids before they are sent, then answer unknown requests wrongly"),
    ("wrong-unknown-error", "answer unknown requests with InvalidRequest"),
    ("string-id-to-number", "reply to a request whose id is a string of digits with that number as id"),
    ("reply-twice", "answer every lspcheck/probe twice"),
    ("reply-unsent-id", "after initialize, send a response with an id lspcheck never used"),
    ("reply-to-notification", "answer an unknown notification with an error whose id is null"),
    ("reply-to-exit", "answer exit with an error whose id is null before exiting"),
    ("out-of-range-id", "after initialize, send a request whose id 2147483648 is outside LSP's integer range"),
    (
        "fill-stdin-on-initialized",
        "on initialized, ask for a configuration whose answer fills a 64 KiB pipe, then stop reading for 3 s",
    ),
    ("exit-on-unknown-notification", "exit after the second unknown notification (after any reply to it)"),
    ("big-reply-to-notification", "answer an unknown notification with a 2 MiB error whose id is null"),
    ("no-jsonrpc-field", "after initialize, send a window/logMessage notification without \"jsonrpc\""),
    ("result-and-error", "answer an unknown $/ request with both a result and an error"),
    ("string-error-code", "answer an unknown $/ request with an error whose code is the string \"-32601\""),
    (
        "drop-concurrent",
        "on a request with an all-digit string id, wait for the next frame and drop the earlier request",
    ),
    ("ignore-dollar-request", "never answer an unknown $/ request"),
    ("shutdown-result", "return {} from shutdown"),
    ("exit-on-shutdown", "exit right after replying to shutdown"),
    ("big-frame-on-exit", "on exit, send one 2 MiB log message before exiting"),
    ("exit-flood", "on exit, write 100,000 small log messages and then a line outside any frame"),
    ("exit-flood-bad-envelope", "on exit, write 200,000 log messages and then one without \"jsonrpc\""),
    ("exit-status", "exit with status 3 on exit"),
    ("ignore-exit", "ignore exit and stay running"),
    ("split-header", "drop input when a read ends inside a header section, as if each header came in one read"),
    ("split-utf8", "decode each read as UTF-8 on its own, mangling characters split across reads"),
    ("first-frame-per-read", "keep only the first frame of a read that holds $/setTrace and two more"),
    ("char-length-input", "count the Content-Length of incoming frames in characters, not bytes"),
    ("no-content-type", "ignore incoming frames that have a Content-Type header"),
    ("strict-charset", "ignore incoming frames whose charset isn't spelled utf-8"),
    ("exit-on-content-type", "exit when an incoming frame has a Content-Type header"),
    ("ignore-unknown-requests", "never answer requests for unknown methods"),
    ("exit-on-string-id", "exit when a request has a string id"),
    ("answers-reordered", "lawful: answer requests that arrive together in reverse order"),
    ("defer-until-next", "lawful: hold a workspace/symbol reply until the next message, cancelling it if asked"),
    ("cancelled-no-reply", "never answer a deferred request that is cancelled"),
    ("cancelled-reply-twice", "answer a cancelled request with both -32800 and a result"),
    ("cancelled-other-error", "lawful: answer a cancelled request with InternalError (-32603)"),
    ("cancel-unknown-reply", "answer a $/cancelRequest for an id that isn't pending with an error carrying it"),
    ("stop-after-unknown-cancel", "stop answering requests after a $/cancelRequest for an id that isn't pending"),
    ("reply-to-cancel", "answer every $/cancelRequest with an error whose id is null"),
    ("late-cancel-replies", "send what the cancel faults send only after answering the next request"),
    ("big-cancel-replies", "pad what the cancel faults send with 2 MiB of error data"),
    ("null-reply-after-first-probe", "after initialize, follow the first answer with an error whose id is null"),
    ("shutdown-no-jsonrpc", "leave \"jsonrpc\" out of the answer to shutdown"),
    (
        "shutdown-block",
        "on shutdown, send its answer, a huge configuration request and an error with id null, then stop reading",
    ),
    ("big-frame-on-cancel", "after answering a cancelled request, send a 2 MiB log message"),
    (
        "initialized-reply-after-cancel",
        "answer initialized twice with an error whose id is null, once a cancel arrives",
    ),
    ("big-frame-after-drop", "after cancelled-no-reply drops a request, send a 2 MiB log message with the next answer"),
    ("no-open-close", "lawful: don't ask for didOpen, and exit if one arrives anyway"),
    ("slow-reads", "lawful: pause 30 ms before each read of stdin"),
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
    // Whether a workspace/symbol reply waits to see whether it is cancelled.
    let holds = ["defer-until-next", "cancelled-no-reply", "cancelled-reply-twice", "cancelled-other-error"]
        .iter()
        .any(|name| fault(name));
    let mut out = Writer { out: std::io::stdout().lock(), faults: faults.clone(), holding: false, held: Vec::new() };
    if fault("stdout-banner") {
        out.raw(b"standin starting\n");
    }
    let mut input = Input { inner: std::io::stdin().lock(), buf: Vec::new(), faults: faults.clone() };
    let (mut initialized, mut shut_down) = (false, false);
    // A reply to a notification, held until the next request has been answered.
    let mut late_reply: Option<Value> = None;
    // A workspace/symbol request whose reply waits for the next message.
    let mut deferred: Option<Value> = None;
    // Set by stop-after-unknown-cancel: requests are no longer answered.
    let mut no_more_answers = false;
    // What the cancel faults send, and where: now, or after the next answer.
    let mut cancel_out =
        CancelOut { late: fault("late-cancel-replies"), big: fault("big-cancel-replies"), held: Vec::new() };
    // Set when cancelled-no-reply drops a request, for big-frame-after-drop.
    let mut dropped = false;
    // For initialized-reply-after-cancel: a reply to `initialized` not sent yet.
    let mut initialized_reply = fault("initialized-reply-after-cancel");
    // For null-reply-after-first-probe: whether a request was answered since initialize.
    let mut answered_one = false;
    // For fill-stdin-on-initialized: pause once `initialized` arrives.
    let mut pause_on_initialized = false;
    let mut children: Vec<Child> = Vec::new();
    loop {
        if !input.ready() {
            // About to wait for input: send what was held back, newest first.
            out.release();
        }
        let Some((mut headers, mut body)) = input.next() else { break };
        // A server that answers only the latest of several concurrent requests:
        // J6's batch is the only one with an all-digit string id, and lspcheck
        // writes all four requests before reading, so waiting for the next
        // frame is guaranteed to succeed. This depends on protocol order, not
        // on where the OS happened to end its reads.
        if fault("drop-concurrent") && is_digit_string_request(&body) {
            let Some(next) = input.next() else { break };
            eprintln!("standin: drop-concurrent dropped a request");
            (headers, body) = next;
        }
        if fault("answers-reordered") && input.ready() {
            out.holding = true;
        }
        let charset = headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.split_once("charset=").map_or("", |(_, c)| c).trim().to_string());
        if fault("exit-on-content-type") && charset.is_some() {
            std::process::exit(2);
        }
        if (fault("no-content-type") && charset.is_some())
            || (fault("strict-charset") && charset.as_deref().is_some_and(|c| c != "utf-8"))
        {
            continue;
        }
        let Ok(msg) = serde_json::from_slice::<Value>(&body) else { continue };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("").to_string();
        // An answer to one of this server's own requests: nothing to do.
        if msg.get("method").is_none() && (msg.get("result").is_some() || msg.get("error").is_some()) {
            continue;
        }
        if fault("exit-on-string-id") && msg.get("id").is_some_and(Value::is_string) {
            std::process::exit(2);
        }
        // A held reply: a cancel for it decides its answer, and any other
        // message is handled after the reply, as if the two had been read in
        // that order.
        if let Some(deferred_id) = deferred.clone() {
            let cancels_it = method == "$/cancelRequest" && msg.pointer("/params/id") == Some(&deferred_id);
            if cancels_it {
                deferred = None;
                if fault("cancelled-no-reply") {
                    dropped = true;
                } else if fault("cancelled-other-error") {
                    out.frame(&err(&deferred_id, -32603, "internal error"));
                } else {
                    out.frame(&err(&deferred_id, -32800, "request cancelled"));
                    if fault("big-frame-on-cancel") {
                        let text = "x".repeat(2 * 1024 * 1024);
                        out.frame(&json!({"jsonrpc": "2.0", "method": "window/logMessage", "params": {"type": 4, "message": text}}));
                    }
                    if fault("cancelled-reply-twice") {
                        cancel_out.send(&mut out, ok(&deferred_id, Value::Null));
                    }
                }
                if fault("reply-to-cancel") {
                    cancel_out.send(&mut out, cancel_reply());
                }
                continue;
            }
            out.frame(&ok(&deferred_id, Value::Null));
            deferred = None;
        }
        let Some(id) = msg.get("id").cloned() else {
            // A notification.
            if method == "initialized" && std::mem::take(&mut pause_on_initialized) {
                std::thread::sleep(std::time::Duration::from_secs(3));
            }
            if method == "$/cancelRequest" {
                if std::mem::take(&mut initialized_reply) {
                    for _ in 0..2 {
                        out.frame(&err(&Value::Null, -32601, "unknown notification"));
                    }
                }
                if fault("reply-to-cancel") {
                    cancel_out.send(&mut out, cancel_reply());
                }
                if let Some(cancelled) = msg.pointer("/params/id")
                    && fault("cancel-unknown-reply")
                {
                    cancel_out.send(&mut out, err(cancelled, -32800, "request cancelled"));
                }
                if fault("stop-after-unknown-cancel") {
                    no_more_answers = true;
                }
                continue;
            }
            if method == "textDocument/didOpen" && fault("no-open-close") {
                std::process::exit(4);
            }
            if initialized
                && (fault("reply-to-notification") || fault("big-reply-to-notification"))
                && (method.starts_with("lspcheck/") || method.starts_with("$/lspcheck/"))
            {
                let mut reply = err(&Value::Null, -32601, "unknown notification");
                if fault("big-reply-to-notification") {
                    reply["error"]["data"] = json!("x".repeat(2 * 1024 * 1024));
                }
                out.frame(&reply);
            }
            if initialized && method.starts_with("$/lspcheck/") && fault("exit-on-unknown-notification") {
                std::process::exit(0);
            }
            if !initialized && fault("late-reply-to-pre-init-notification") {
                late_reply = Some(
                    json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32002, "message": "not initialized"}}),
                );
            }
            if !initialized && fault("reply-to-pre-init-notification") {
                out.frame(
                    &json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32002, "message": "not initialized"}}),
                );
            }
            if method == "exit" && !fault("ignore-exit") {
                if fault("reply-to-exit") {
                    out.frame(&err(&Value::Null, -32601, "unknown notification"));
                }
                if fault("big-frame-on-exit") {
                    let text = "x".repeat(2 * 1024 * 1024);
                    out.frame(&json!({"jsonrpc": "2.0", "method": "window/logMessage", "params": {"type": 4, "message": text}}));
                }
                if fault("overlong-last-frame") {
                    let body = "{\"jsonrpc\":\"2.0\",\"method\":\"window/logMessage\",\"params\":{\"type\":4,\"message\":\"é\"}}\n";
                    out.raw(format!("Content-Length: {}\r\n\r\n{body}", body.len() + 10_000).as_bytes());
                }
                if fault("exit-flood") {
                    let log =
                        json!({"jsonrpc": "2.0", "method": "window/logMessage", "params": {"type": 4, "message": "x"}});
                    let mut bytes = out.encode(&log).repeat(100_000);
                    bytes.extend_from_slice(b"goodbye\n");
                    out.raw(&bytes);
                }
                if fault("exit-flood-bad-envelope") {
                    let log =
                        json!({"jsonrpc": "2.0", "method": "window/logMessage", "params": {"type": 4, "message": "x"}});
                    let mut bytes = out.encode(&log).repeat(200_000);
                    let mut bad =
                        json!({"jsonrpc": "2.0", "method": "window/logMessage", "params": {"type": 4, "message": "x"}});
                    bad.as_object_mut().unwrap().remove("jsonrpc");
                    bytes.extend(out.encode(&bad));
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
        if no_more_answers {
            continue;
        }
        // A numeric string id turned into a number: the wrong JSON type.
        let id = match id.as_str().filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())) {
            Some(s) if fault("string-id-to-number") => s.parse::<i64>().map_or(id, |n| json!(n)),
            _ => id,
        };
        if method == "initialize" && initialized && !fault("reinitialize") {
            if fault("exit-on-reinitialize") {
                std::process::exit(3);
            }
            out.frame(&err(&id, -32600, "already initialized"));
            continue;
        }
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
            if fault("trailing-junk") {
                let body = "{\"jsonrpc\":\"2.0\",\"method\":\"window/logMessage\",\"params\":{\"type\":4,\"message\":\"é\"}}Content-Length: 2\r\n\r\n{}";
                out.raw(format!("Content-Length: {}\r\n\r\n{body}", body.len()).as_bytes());
            }
            for (name, line) in [("json-into-next-header", "X: \"}}"), ("json-into-bad-header", " X: \"}}")] {
                if fault(name) {
                    let body =
                        "{\"jsonrpc\":\"2.0\",\"method\":\"window/logMessage\",\"params\":{\"type\":4,\"message\":\"é";
                    // A well-formed notification follows, so only the framing
                    // mistake this fault is about can be blamed.
                    let notice = serde_json::to_vec(
                        &json!({"jsonrpc": "2.0", "method": "window/logMessage", "params": {"type": 4, "message": "x"}}),
                    )
                    .unwrap();
                    let next = format!("{line}\r\nContent-Length: {}\r\n\r\n", notice.len());
                    out.raw(format!("Content-Length: {}\r\n\r\n{body}{next}", body.len()).as_bytes());
                    out.raw(&notice);
                }
            }
            if fault("long-miscount") {
                let text = "é".repeat(100);
                let body = format!(
                    "{{\"jsonrpc\":\"2.0\",\"method\":\"window/logMessage\",\"params\":{{\"type\":4,\"message\":\"{text}\"}}}}"
                );
                out.raw(format!("Content-Length: 1\r\n\r\n{body}").as_bytes());
            }
            if fault("bad-bodies") {
                for _ in 0..70 {
                    out.raw(b"Content-Length: 1\r\n\r\n!");
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
                "capabilities": {
                    "positionEncoding": encoding,
                    "textDocumentSync": 1,
                    "hoverProvider": true,
                    "workspaceSymbolProvider": true
                },
                "serverInfo": {"name": "lspcheck-standin"}
            });
            if fault("no-open-close") {
                result["capabilities"]["textDocumentSync"] = json!({"openClose": false, "change": 0});
            }
            if fault("no-capabilities") {
                result = json!({"serverInfo": {"name": "lspcheck-standin"}});
            }
            if fault("fill-stdin-on-initialized") {
                // In the same write as the result, so the client reads both at
                // once and answers before its next request. The answer, with
                // `initialized` still unread, leaves 30 bytes of a 64 KiB pipe:
                // too few for the client's next requests while this server
                // isn't reading.
                let mut bytes = out.encode(&ok(&id, result));
                bytes.extend(out.encode(&config_filling(65_536 - 74 - 30)));
                out.raw(&bytes);
                pause_on_initialized = true;
                continue;
            }
            out.frame(&ok(&id, result));
            if fault("reply-unsent-id") {
                out.frame(&err(&json!(format!("lspcheck-unsent-{}", std::process::id())), -32601, "no such request"));
            }
            if fault("out-of-range-id") {
                let request = json!({"jsonrpc": "2.0", "id": 2_147_483_648_i64, "method": "window/workDoneProgress/create", "params": {"token": "t"}});
                out.frame(&request);
            }
            if fault("no-jsonrpc-field") {
                out.frame(
                    &json!({"method": "window/logMessage", "params": {"type": 4, "message": "no jsonrpc field"}}),
                );
            }
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
        if !initialized && fault("ignore-pre-init-requests") {
            continue;
        }
        if !initialized && !fault("no-pre-init-error") {
            if fault("preinit-banner") {
                out.raw(b"pre-init server log\n");
            }
            // Both in one write, so the reply is read in the same go as the answer.
            let mut bytes = out.encode(&err(&id, -32002, "not initialized"));
            if let Some(reply) = late_reply.take() {
                bytes.extend(out.encode(&reply));
            }
            out.raw(&bytes);
            continue;
        }
        if shut_down && !fault("post-shutdown-results") {
            out.frame(&err(&id, -32600, "shutting down"));
            continue;
        }
        if method == "workspace/symbol" && holds {
            deferred = Some(id.clone());
            continue;
        }
        if std::mem::take(&mut dropped) && fault("big-frame-after-drop") {
            let text = "x".repeat(2 * 1024 * 1024);
            out.frame(
                &json!({"jsonrpc": "2.0", "method": "window/logMessage", "params": {"type": 4, "message": text}}),
            );
        }
        if fault("reply-twice") && method == "lspcheck/probe" {
            out.frame(&err(&id, -32601, "method not found"));
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
                if fault("shutdown-block") {
                    // One write; the configuration answer can't be written
                    // because this server stops reading.
                    let items = vec![json!({"section": "x"}); 40_000];
                    let config = json!({"jsonrpc": "2.0", "id": "cfg", "method": "workspace/configuration", "params": {"items": items}});
                    let mut bytes = out.encode(&ok(&id, result));
                    bytes.extend(out.encode(&config));
                    bytes.extend(out.encode(&cancel_reply()));
                    out.raw(&bytes);
                    std::thread::sleep(std::time::Duration::from_secs(60));
                    std::process::exit(1);
                }
                let mut reply = ok(&id, result);
                if fault("shutdown-no-jsonrpc") {
                    reply.as_object_mut().unwrap().remove("jsonrpc");
                }
                out.frame(&reply);
            }
            "textDocument/hover" | "workspace/symbol" => out.frame(&ok(&id, Value::Null)),
            _ if fault("early-replies") => out.frame(&ok(&id, Value::Null)),
            "$/lspcheck/unknownRequest" if fault("result-and-error") => out.frame(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": Value::Null,
                "error": {"code": -32601, "message": "method not found"}
            })),
            "$/lspcheck/unknownRequest" if fault("string-error-code") => out.frame(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": "-32601", "message": "method not found"}
            })),
            m if m.starts_with("$/") && fault("ignore-dollar-request") => {}
            _ if fault("ignore-unknown-requests") => {}
            _ if fault("wrong-unknown-error") => out.frame(&err(&id, -32600, "unknown")),
            _ => out.frame(&err(&id, -32601, "method not found")),
        }
        if !std::mem::replace(&mut answered_one, true) && fault("null-reply-after-first-probe") {
            // As if `initialized` had been answered late.
            out.frame(&err(&Value::Null, -32601, "unknown notification"));
        }
        cancel_out.flush(&mut out);
    }
    // End of input without exit.
    if fault("ignore-eof") {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
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
    /// Hold frames back until [`Writer::release`] (answers-reordered).
    holding: bool,
    held: Vec<Vec<u8>>,
}

impl<W: Write> Writer<W> {
    fn raw(&mut self, bytes: &[u8]) {
        self.out.write_all(bytes).unwrap();
        self.out.flush().unwrap();
    }

    fn frame(&mut self, v: &Value) {
        let bytes = self.encode(v);
        if self.holding {
            self.held.push(bytes);
        } else {
            self.raw(&bytes);
        }
    }

    /// Writes the frames held back, newest first.
    fn release(&mut self) {
        self.holding = false;
        while let Some(bytes) = self.held.pop() {
            self.raw(&bytes);
        }
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
        let mut body = serde_json::to_vec(&v).unwrap();
        if fault("ascii-escape") {
            // Non-ASCII text can only be inside strings, so escaping it everywhere is safe.
            let mut escaped = String::new();
            for c in String::from_utf8(body).unwrap().chars() {
                if c.is_ascii() {
                    escaped.push(c);
                } else {
                    for unit in c.encode_utf16(&mut [0; 2]) {
                        escaped.push_str(&format!("\\u{unit:04x}"));
                    }
                }
            }
            body = escaped.into_bytes();
        }
        if fault("newline-after-body") {
            body.push(b'\n');
        }
        let length = if fault("char-length") { String::from_utf8_lossy(&body).chars().count() } else { body.len() };
        let mut header = Vec::new();
        if fault("extension-header") {
            header.extend_from_slice(b"X-Trace: standin\r\n");
        }
        if fault("long-header") {
            header.extend_from_slice(format!("X-Pad: {}\r\n", "p".repeat(200 * 1024)).as_bytes());
        }
        header.extend_from_slice(format!("Content-Length: {length}\r\n").as_bytes());
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

/// Where the cancel faults' messages go: straight out, or held until the
/// next request has been answered (late-cancel-replies), so a client can't
/// count on them arriving before that answer.
struct CancelOut {
    late: bool,
    big: bool,
    held: Vec<Value>,
}

impl CancelOut {
    fn send<W: Write>(&mut self, out: &mut Writer<W>, mut v: Value) {
        if self.big
            && let Some(error) = v.get_mut("error")
        {
            error["data"] = json!("x".repeat(2 * 1024 * 1024));
        }
        if self.late {
            self.held.push(v);
        } else {
            out.frame(&v);
        }
    }

    fn flush<W: Write>(&mut self, out: &mut Writer<W>) {
        for v in std::mem::take(&mut self.held) {
            out.frame(&v);
        }
    }
}

/// A configuration request whose answer from lspcheck, framed, is `size`
/// bytes or just under: `{"jsonrpc":"2.0","id":"cfg","result":[null,…]}`.
fn config_filling(size: usize) -> Value {
    let framed = |n: usize| {
        // `null` and a comma per item but the last, inside the envelope.
        let body = r#"{"jsonrpc":"2.0","id":"cfg","result":[]}"#.len() + 5 * n - 1;
        "Content-Length: \r\n\r\n".len() + body.to_string().len() + body
    };
    let n = (1..).take_while(|&n| framed(n) <= size).last().unwrap_or(1);
    let items = vec![json!({}); n];
    json!({"jsonrpc": "2.0", "id": "cfg", "method": "workspace/configuration", "params": {"items": items}})
}

/// A response to a cancel notification: JSON-RPC has no id to give back.
fn cancel_reply() -> Value {
    json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32601, "message": "unhandled notification $/cancelRequest"}})
}

/// Frames read from stdin, with the reading faults applied. Reads go
/// straight to the pipe (a read buffer larger than stdin's own bypasses it),
/// so what one read returns is what one read of the pipe returned.
struct Input<R: Read> {
    inner: R,
    buf: Vec<u8>,
    faults: Vec<String>,
}

type Frame = (Vec<(String, String)>, Vec<u8>);

enum Parsed {
    /// A frame and how many bytes it took.
    Frame(Frame, usize),
    /// A header section without a usable Content-Length, and its length.
    Bad(usize),
    Incomplete,
}

impl<R: Read> Input<R> {
    fn fault(&self, name: &str) -> bool {
        self.faults.iter().any(|f| f == name)
    }

    /// The next frame, skipping header sections it can't use; `None` at end of input.
    fn next(&mut self) -> Option<Frame> {
        loop {
            match self.parse(&self.buf) {
                Parsed::Frame(frame, used) => {
                    self.buf.drain(..used);
                    return Some(frame);
                }
                Parsed::Bad(used) => {
                    self.buf.drain(..used);
                }
                Parsed::Incomplete => {
                    if !self.fill() {
                        return None;
                    }
                }
            }
        }
    }

    /// Whether a whole frame is already buffered.
    fn ready(&self) -> bool {
        matches!(self.parse(&self.buf), Parsed::Frame(..))
    }

    /// Reads once; false at end of input.
    fn fill(&mut self) -> bool {
        if self.fault("slow-reads") {
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        let mut chunk = vec![0u8; 64 * 1024];
        let n = match self.inner.read(&mut chunk) {
            Ok(0) | Err(_) => return false,
            Ok(n) => n,
        };
        chunk.truncate(n);
        // Each fault says on stderr when it took effect, since that depends
        // on where the reads happened to end.
        if self.fault("split-header") && !self.buf.is_empty() && find(&self.buf, b"\r\n\r\n").is_none() {
            eprintln!("standin: split-header dropped {} bytes", self.buf.len());
            self.buf.clear();
        }
        if self.fault("split-utf8") && std::str::from_utf8(&chunk).is_err() {
            eprintln!("standin: split-utf8 mangled a read");
            chunk = String::from_utf8_lossy(&chunk).into_owned().into_bytes();
        }
        self.buf.extend_from_slice(&chunk);
        if self.fault("first-frame-per-read") {
            let mut ends = Vec::new();
            let mut at = 0;
            let mut set_trace = false;
            while let Parsed::Frame((_, body), used) = self.parse(&self.buf[at..]) {
                at += used;
                ends.push(at);
                // Only F4's shape: two requests with $/setTrace between them.
                // Any notification would also match `initialized` coalescing
                // with J6's requests, which this fault must not touch.
                set_trace |= serde_json::from_slice::<Value>(&body).is_ok_and(|v| {
                    v.get("id").is_none() && v.get("method").and_then(Value::as_str) == Some("$/setTrace")
                });
            }
            if ends.len() >= 3 && set_trace {
                self.buf.truncate(ends[0]);
            }
        }
        true
    }

    fn parse(&self, buf: &[u8]) -> Parsed {
        let Some(end) = find(buf, b"\r\n\r\n") else { return Parsed::Incomplete };
        let mut headers = Vec::new();
        let mut length = None;
        for line in String::from_utf8_lossy(&buf[..end]).split("\r\n") {
            let Some((name, value)) = line.split_once(':') else { return Parsed::Bad(end + 4) };
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse::<usize>().ok();
            }
            headers.push((name.to_string(), value.trim().to_string()));
        }
        let Some(length) = length else { return Parsed::Bad(end + 4) };
        let start = end + 4;
        let size = if self.fault("char-length-input") {
            // The bytes that `length` characters take.
            let mut at = start;
            for _ in 0..length {
                at += match buf.get(at) {
                    None => return Parsed::Incomplete,
                    Some(b) if *b >= 0xf0 => 4,
                    Some(b) if *b >= 0xe0 => 3,
                    Some(b) if *b >= 0xc0 => 2,
                    Some(_) => 1,
                };
            }
            at - start
        } else {
            length
        };
        if buf.len() < start + size {
            return Parsed::Incomplete;
        }
        Parsed::Frame((headers, buf[start..start + size].to_vec()), start + size)
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Whether `body` is a request whose id is a non-empty all-digit string. Only
/// J6 sends one, which is what makes drop-concurrent act in protocol order.
fn is_digit_string_request(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body).is_ok_and(|v| {
        v.get("method").is_some_and(Value::is_string)
            && v.get("id")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
    })
}
