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
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};

const R1_BAD_JSON: &[u8] = b"lspcheck: this body is not JSON {";

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
    ("parse-error-wrong-code", "answer a non-JSON body with InvalidRequest (-32600), not ParseError"),
    ("bad-json-reply-after-probe", "in one write, answer the probe after a non-JSON body then send InvalidRequest"),
    ("parse-error-with-initialize", "in one write, answer initialize then send a null-id ParseError"),
    ("silent-on-bad-json", "lawful: ignore a non-JSON body without answering it"),
    ("dies-on-bad-json", "exit when a non-JSON body arrives"),
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
    ("replay-gate", "ask a client request and answer it only after the client answers workspace/configuration"),
    ("reinitialize", "answer a second initialize as if it were the first"),
    ("exit-on-reinitialize", "exit on a second initialize"),
    ("post-shutdown-results", "answer requests after shutdown normally"),
    ("ignore-eof", "keep running when stdin ends"),
    ("bad-encoding", "choose a position encoding the client didn't offer"),
    ("no-capabilities", "leave capabilities out of the initialize result"),
    ("early-replies", "answer guessed request ids before they are sent, then answer unknown requests wrongly"),
    ("wrong-unknown-error", "answer unknown requests with InvalidRequest"),
    ("string-id-to-number", "reply to a request whose id is a string of digits with that number as id"),
    ("mangles-odd-ids", "drop non-ASCII characters from string request ids before replying"),
    ("drops-negative-ids", "never answer a request with a negative integer id"),
    ("accepts-wrong-params", "answer a request with wrongly typed params with result null"),
    ("dies-on-wrong-params", "exit when a request arrives with wrongly typed params"),
    ("wrong-params-other-code", "lawful: answer wrongly typed params with InternalError (-32603)"),
    ("wrong-params-null-error", "answer wrongly typed params with error null"),
    ("wrong-params-string-code", "answer wrongly typed params with a string error code"),
    ("wrong-params-large-code", "answer wrongly typed params with an error code outside the signed 32-bit range"),
    ("wrong-params-missing-message", "answer wrongly typed params with no error message"),
    ("wrong-params-numeric-message", "answer wrongly typed params with a numeric error message"),
    ("wrong-params-result-and-error", "answer wrongly typed params with both result and error"),
    (
        "trailing-fragment",
        "lawful: after the last odd-id probe, and after the probe that follows wrongly typed params, a non-JSON \
         body or a large document, begin a logMessage and finish it only when shutdown arrives",
    ),
    (
        "big-reply-then-exit",
        "answer the probe that follows wrongly typed params, a non-JSON body or a large document with a 2 MiB error, \
         then exit",
    ),
    ("reply-twice", "answer every lspcheck/probe twice"),
    ("reply-unsent-id", "after initialize, send a response with an id lspcheck never used"),
    ("reply-to-notification", "answer an unknown notification with an error whose id is null"),
    ("reply-to-exit", "answer exit with an error whose id is null before exiting"),
    ("out-of-range-id", "after initialize, send a request whose id 2147483648 is outside LSP's integer range"),
    (
        "await-progress-answer",
        "lawful: after initialized, handle nothing else until the progress request is answered (as gopls does)",
    ),
    ("reused-request-id", "on initialized, send two progress requests with the same id in one write"),
    ("reuse-answered-request-id", "lawful: once the progress request is answered, send another with the same id"),
    (
        "fill-stdin-on-initialized",
        "with the initialize result, ask for a configuration whose answer fills a 64 KiB pipe; stop reading for 3 s",
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
    ("big-dollar-reply", "lawful: answer an unknown $/ request with MethodNotFound carrying 2 MiB of data"),
    (
        "big-frame-before-unknown-reply",
        "send a 2 MiB log message before answering lspcheck/unknownRequest (J4's first request)",
    ),
    (
        "inflated-before-shutdown",
        "before replying to shutdown, start a frame whose 2 MiB body never comes, so it swallows the reply",
    ),
    (
        "inflated-on-dollar-request",
        "before answering an unknown $/ request, start a frame whose 2 MiB body never comes, so it swallows the answer",
    ),
    ("flood-on-shutdown", "before replying to shutdown, write 2 MiB of bytes"),
    ("inflated-on-change", "on a didChange, start a frame whose 2 MiB body never comes, so it swallows what follows"),
    (
        "inflated-on-first-probe",
        "before answering the session's first lspcheck/probe, start a frame whose 2 MiB body never comes",
    ),
    (
        "inflated-on-cancel",
        "before answering a cancelled request, start a frame whose 2 MiB body never comes, so it swallows the answer",
    ),
    ("shutdown-result", "return {} from shutdown"),
    ("exit-on-shutdown", "exit right after replying to shutdown"),
    ("big-frame-on-exit", "on exit, send one 2 MiB log message before exiting"),
    ("exit-flood", "on exit, write 100,000 small log messages and then a line outside any frame"),
    ("exit-status", "exit with status 3 on exit"),
    ("ignore-exit", "ignore exit and stay running"),
    ("inflated-on-exit", "on exit, start an inflated frame containing a progress request and stay running"),
    ("split-header", "drop input when a read ends inside a header section, as if each header came in one read"),
    ("split-utf8", "decode each read as UTF-8 on its own, mangling characters split across reads"),
    (
        "first-frame-per-read",
        "drop $/setTrace and the frames after it from a read that holds a request, $/setTrace and another frame",
    ),
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
    ("big-frame-after-drop", "after dropping a request, send a 2 MiB log message with the next answer"),
    ("no-open-close", "don't ask for didOpen, but exit if one arrives"),
    ("dies-on-large-document", "exit when a document larger than 1 MiB is opened"),
    ("slow-large-document", "lawful: take 3 s to process a document larger than 1 MiB"),
    ("utf8-positions-as-utf16", "count positions in UTF-8 bytes while UTF-16 is negotiated"),
    ("codepoint-positions", "count positions in code points while UTF-16 is negotiated"),
    ("lf-only-lines", "split lines at \\n only; a \\r is an ordinary character"),
    ("crlf-two-breaks", "count \\r\\n as two line breaks, leaving an empty line between"),
    ("no-lone-cr", "split at \\n and \\r\\n, but not a lone \\r"),
    (
        "invalid-duplicate-symbols",
        "after answering documentSymbol, answer it again with a range inside the first character of line 0",
    ),
    ("utf16-positions-as-utf8", "choose utf-8 when offered, but count positions in UTF-16 code units"),
    ("utf8-encoding", "lawful: choose utf-8 when offered, and count positions in bytes"),
    ("utf32-encoding", "lawful: choose utf-32 when offered, and count positions in code points"),
    ("past-line-end", "lawful: add diagnostics ending past the end of a line and past the end of the document"),
    ("grapheme-split", "lawful: add a diagnostic starting between a letter and its combining mark"),
    (
        "foreign-locations",
        "lawful: add diagnostics and related information inside characters of a URI never opened, labelled with a version lspcheck never sent",
    ),
    ("positions-in-data", "lawful: put positions inside characters into each diagnostic's data"),
    ("no-diagnostics", "lawful: never publish diagnostics"),
    ("unversioned-diagnostics", "lawful: publish diagnostics without a version"),
    (
        "diagnostics-own-version",
        "label publishDiagnostics with the stand-in's own counter instead of the document's version",
    ),
    ("future-diagnostics-version", "label publishDiagnostics with the document's version plus one"),
    ("ignore-changes", "don't apply didChange, and label the old text's diagnostics with the new version"),
    ("codepoint-changes", "apply incremental change ranges counting code points, and answer correctly otherwise"),
    (
        "mislabelled-diagnostics",
        "after a didChange, publish the previous text's diagnostics labelled with the new version",
    ),
    (
        "stale-diagnostics",
        "lawful: on a didChange, first publish the previous text's diagnostics labelled with the previous version, then the new",
    ),
    ("stale-unversioned-diagnostics", "lawful: as stale-diagnostics, without versions"),
    ("full-sync", "lawful: advertise Full sync, and take the whole text each change"),
    ("no-change-sync", "lawful: advertise openClose without a change kind"),
    ("register-sync", "after initialized, register textDocument/didChange dynamically"),
    ("register-other", "lawful: after initialized, register workspace/didChangeWatchedFiles dynamically"),
    ("codepoint-positions-after-change", "count positions in code points from a didChange on, correctly before it"),
    (
        "close-stdin-before-change",
        "on didOpen, publish a bad-boundary diagnostic, then exit so didChange can't be sent",
    ),
    ("big-frame-on-open", "after publishing diagnostics for an opened document, send a 2 MiB log message"),
    ("ignore-unopened", "never answer a request about a document that isn't open"),
    ("stop-after-unopened", "stop answering anything after a request about a document that isn't open"),
    ("unopened-error", "lawful: answer a request about a document that isn't open with InvalidParams"),
    ("closed-from-disk", "lawful: answer requests about closed documents from the file on disk"),
    (
        "disk-diagnostics-after-close",
        "lawful: on didClose, publish unversioned diagnostics for the file on disk, which another tool changed to ASCII",
    ),
    ("drop-first-request", "never answer the first request about a document, then answer normally"),
    ("exit-on-first-hover", "exit with status 17 without answering the first hover request"),
    ("stop-at-crlf", "stop answering anything from the first request about crlf.txt"),
    ("exit-at-version-42", "exit, without answering, at the first request about a document at version 42"),
    ("slow-reads", "lawful: pause 30 ms before each read of stdin"),
    ("orphan-child", "start a child process that outlives the server"),
    ("escaped-child", "start a child in a new session, with a non-UTF-8 name, that outlives the server"),
    ("empty-selection", "lawful: an empty range at each position, parent the whole line"),
    ("selection-outside", "the range is the word before the position, still on a boundary"),
    ("selection-short", "one entry fewer than the positions"),
    ("selection-parent-inside", "the parent is a range strictly inside its child"),
    ("advertises-unimplemented", "advertise colorProvider, and answer textDocument/documentColor with MethodNotFound"),
    (
        "capability-other-error",
        "lawful: advertise colorProvider, and answer textDocument/documentColor with RequestFailed (-32803)",
    ),
    ("ignores-advertised", "advertise colorProvider, and never answer textDocument/documentColor"),
    (
        "method-not-found-after-result",
        "advertise colorProvider, answer textDocument/documentColor with a result, then again with MethodNotFound when \
         shutdown arrives",
    ),
    ("defer-hover-until-shutdown", "lawful: answer hover requests only when shutdown arrives"),
    (
        "advertised-error-after-shutdown",
        "answer shutdown, then have a helper answer held hover requests with MethodNotFound after stdin closes",
    ),
    (
        "advertised-result-after-shutdown",
        "answer shutdown, then have a helper answer held hover requests with null after stdin closes",
    ),
    (
        "config-before-log-finish",
        "with the reply to workspace/symbol, ask for configuration and finish a log notification only after the answer",
    ),
    ("echo-positions", "lawful: say on stderr where each hover request asks, as FILE LINE:CHARACTER"),
    ("hover-invalid-second-result", "answer hover with null, then with a number"),
    ("hover-invalid-result-after-error", "answer hover with an error, then with a number"),
    ("hover-contents-number", "answer hover with a Hover whose contents is the number 3"),
    ("symbol-missing-name", "answer workspace/symbol with a symbol that has no name"),
    ("bad-message-type", "on initialized, send a window/logMessage with type 9, which MessageType doesn't have"),
    (
        "extra-properties",
        "lawful: add a property LSP doesn't define to every result and params object, and to each object in an array \
         result",
    ),
    (
        "custom-folding-range-kind",
        "lawful: give folding ranges the kind \"lspcheck.custom\" (custom kinds are allowed)",
    ),
    ("needs-init-option", "refuse to initialize unless initializationOptions has standin = \"ready\""),
    (
        "needs-workspace-file",
        "refuse to initialize unless the root holds standin.toml and every file its require lines name",
    ),
    (
        "needs-configuration",
        "after initialized, ask for setting standin.mode and wait for it; unless it is \"ready\", answer every request with an error",
    ),
];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--child") => return child(false),
        Some("--escaped-child") => return child(true),
        Some("--relay") => return relay(args[1].parse().unwrap()),
        Some("--reply-after-input") => return reply_after_input(&args[1]),
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
    // How the stand-in splits lines, per the line-splitting faults.
    let split = line_split(&faults);
    // Whether a workspace/symbol reply waits to see whether it is cancelled.
    let holds = [
        "defer-until-next",
        "cancelled-no-reply",
        "cancelled-reply-twice",
        "cancelled-other-error",
        "inflated-on-cancel",
    ]
    .iter()
    .any(|name| fault(name));
    // Stdout's line buffer can split a coalesced write at the last header's
    // newline, leaving its body for a second write. Use the pipe directly so
    // the small batches below really are written together.
    let stdout = std::io::stdout();
    #[cfg(unix)]
    let stdout = std::fs::File::from(std::os::fd::AsFd::as_fd(&stdout).try_clone_to_owned().unwrap());
    #[cfg(not(unix))]
    let stdout = std::fs::File::from(std::os::windows::io::AsHandle::as_handle(&stdout).try_clone_to_owned().unwrap());
    let mut out = Writer { out: stdout, faults: faults.clone(), holding: false, held: Vec::new() };
    if fault("stdout-banner") {
        out.raw(b"standin starting\n");
    }
    let mut input = Input { inner: std::io::stdin().lock(), buf: Vec::new(), faults: faults.clone() };
    let (mut initialized, mut shut_down) = (false, false);
    let mut replay_gate_id: Option<Value> = None;
    // A reply to a notification, held until the next request has been answered.
    let mut late_reply: Option<Value> = None;
    // A workspace/symbol request whose reply waits for the next message.
    let mut deferred: Option<Value> = None;
    let mut held_hovers: Vec<Value> = Vec::new();
    // Set by stop-after-unknown-cancel: requests are no longer answered.
    let mut no_more_answers = false;
    // For drop-first-request: whether the first document request was dropped.
    let mut dropped_first = false;
    // What the cancel faults send, and where: now, or after the next answer.
    let mut cancel_out =
        CancelOut { late: fault("late-cancel-replies"), big: fault("big-cancel-replies"), held: Vec::new() };
    // Set when cancelled-no-reply drops a request, for big-frame-after-drop.
    let mut dropped = false;
    // For initialized-reply-after-cancel: a reply to `initialized` not sent yet.
    let mut initialized_reply = fault("initialized-reply-after-cancel");
    // For null-reply-after-first-probe: whether a request was answered since initialize.
    let mut answered_one = false;
    // Wrongly typed params, a non-JSON body or a large document just came,
    // and the next probe not yet.
    let mut after_odd_input = false;
    // The rest of a notification begun after the checks were done, written
    // only once `shutdown` arrives.
    let mut unfinished: Option<Vec<u8>> = None;
    // A documentColor request answered once, to answer again at shutdown.
    let mut answered_color: Option<Value> = None;
    // For inflated-on-first-probe.
    let mut probed = false;
    // Messages read by await-progress-answer while it waited, to handle next.
    let mut queued: std::collections::VecDeque<Frame> = std::collections::VecDeque::new();
    // For reuse-answered-request-id: whether the id was used again.
    let mut reused_id = false;
    // For needs-configuration: whether the client gave the setting it needs.
    let mut configured = !fault("needs-configuration");
    let mut children: Vec<Child> = Vec::new();
    // The text of each open document, with the version it was last sent at,
    // and how positions are counted.
    let mut docs: HashMap<String, (String, i64)> = HashMap::new();
    // The URIs closed with didClose, for closed-from-disk.
    let mut closed: HashSet<String> = HashSet::new();
    let mut units = Units::Utf16;
    // The unit of the encoding the stand-in declared, used to apply changes;
    // a position-counting fault changes `units`, not this.
    let mut negotiated_units = Units::Utf16;
    // For diagnostics-own-version: the stand-in's own diagnostics counter.
    let mut own_version = 0i64;
    loop {
        if queued.is_empty() && !input.ready() {
            // About to wait for input: send what was held back, newest first.
            out.release();
        }
        let Some((mut headers, mut body)) = queued.pop_front().or_else(|| input.next()) else { break };
        // A server that answers only the latest of several concurrent requests:
        // J6's batch is the only one with a six-digit string id, and lspcheck
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
        let msg = match serde_json::from_slice::<Value>(&body) {
            Ok(msg) => msg,
            Err(_) => {
                if body.as_slice() != R1_BAD_JSON {
                    continue;
                }
                if fault("dies-on-bad-json") {
                    std::process::exit(2);
                }
                if !fault("silent-on-bad-json") && !fault("bad-json-reply-after-probe") {
                    let code = if fault("parse-error-wrong-code") { -32600 } else { -32700 };
                    out.frame(&err(&Value::Null, code, "parse error"));
                }
                after_odd_input = true;
                continue;
            }
        };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("").to_string();
        // An answer to one of this server's own requests: nothing to do.
        if msg.get("method").is_none() && (msg.get("result").is_some() || msg.get("error").is_some()) {
            // Once the first progress request is answered, its id is free again.
            if fault("reuse-answered-request-id") && !reused_id && msg.get("id") == Some(&json!(1)) {
                reused_id = true;
                out.frame(&json!({"jsonrpc": "2.0", "id": 1, "method": "window/workDoneProgress/create", "params": {"token": "second"}}));
            }
            if fault("replay-gate")
                && msg.get("id").is_some_and(|id| id == "cfg")
                && let Some(id) = replay_gate_id.take()
            {
                out.frame(&ok(&id, Value::Null));
            }
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
                    if fault("inflated-on-cancel") {
                        out.raw(b"Content-Length: 2097152\r\n\r\n{\"jsonrpc\":\"2.0\",");
                    }
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
            if method == "textDocument/didOpen"
                && let (Some(uri), Some(text)) = (
                    msg.pointer("/params/textDocument/uri").and_then(Value::as_str),
                    msg.pointer("/params/textDocument/text").and_then(Value::as_str),
                )
            {
                let large = text.len() > 1024 * 1024;
                after_odd_input |= large;
                if large && fault("dies-on-large-document") {
                    eprintln!("standin: dies-on-large-document");
                    std::process::exit(2);
                }
                if large && fault("slow-large-document") {
                    eprintln!("standin: slow-large-document sleeping");
                    std::thread::sleep(std::time::Duration::from_secs(3));
                }
                let version = msg.pointer("/params/textDocument/version").and_then(Value::as_i64).unwrap_or(0);
                docs.insert(uri.to_string(), (text.to_string(), version));
                if fault("close-stdin-before-change") && text.contains("word") {
                    // Publish version 41's diagnostics with a position inside
                    // the first character, then exit: the stdin pipe's read
                    // end closes, so lspcheck's `didChange` can't be
                    // delivered (D3 is an error) while D1's failure stands.
                    // Only the documents scenario's document carries "word";
                    // the framing scenario's text does not, so it is
                    // unaffected. The message carries non-ASCII text so F2 is
                    // still exercised.
                    let params = json!({
                        "uri": uri,
                        "version": version,
                        "diagnostics": [{"range": range(0, 0, 0, 1), "severity": 2, "message": "bad boundary \u{1F600}"}]
                    });
                    out.frame(
                        &json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": params}),
                    );
                    std::process::exit(0);
                }
                if !fault("no-diagnostics") && !large {
                    let mut label = Some(json!(version));
                    if fault("diagnostics-own-version") {
                        // Its own counter instead: versions the document never had.
                        own_version += 1;
                        label = Some(json!(own_version));
                    } else if fault("future-diagnostics-version") {
                        label = Some(json!(version + 1));
                    }
                    if fault("unversioned-diagnostics") {
                        label = None;
                    }
                    publish_diagnostics(&mut out, &faults, units, uri, text, label);
                }
                if fault("big-frame-on-open") {
                    let text = "x".repeat(2 * 1024 * 1024);
                    out.frame(&json!({"jsonrpc": "2.0", "method": "window/logMessage", "params": {"type": 4, "message": text}}));
                }
            }
            if method == "textDocument/didClose"
                && let Some(uri) = msg.pointer("/params/textDocument/uri").and_then(Value::as_str)
            {
                // The document is no longer open: requests about it are
                // answered from disk (closed-from-disk) or not at all.
                if docs.remove(uri).is_some() && fault("closed-from-disk") {
                    closed.insert(uri.to_string());
                }
                // The file on disk now reads "abc": offset 1 is a boundary
                // there, though it is inside an emoji of every version sent.
                if fault("disk-diagnostics-after-close") {
                    let d = json!({"range": {"start": {"line": 0, "character": 1}, "end": {"line": 0, "character": 2}},
                                   "severity": 3, "message": "on disk"});
                    let params = json!({"uri": uri, "diagnostics": [d]});
                    out.frame(
                        &json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": params}),
                    );
                }
            }
            if method == "textDocument/didChange" && fault("inflated-on-change") {
                out.raw(b"Content-Length: 2097152\r\n\r\n{\"jsonrpc\":\"2.0\",");
            }
            if method == "textDocument/didChange"
                && let (Some(uri), Some(changes)) = (
                    msg.pointer("/params/textDocument/uri").and_then(Value::as_str),
                    msg.pointer("/params/contentChanges").and_then(Value::as_array),
                )
                && let Some((text, old_version)) = docs.get_mut(uri)
            {
                let version = msg.pointer("/params/textDocument/version").and_then(Value::as_i64).unwrap_or(0);
                let previous_version = *old_version;
                let old_text = text.clone();
                if fault("codepoint-positions-after-change") {
                    // From the change on, count positions in code points; the
                    // change itself is applied in the negotiated encoding.
                    units = Units::CodePoints;
                }
                if !fault("ignore-changes") {
                    // The stand-in applies the ranges in the encoding it
                    // declared; codepoint-changes counts code points.
                    let apply_units = if fault("codepoint-changes") { Units::CodePoints } else { negotiated_units };
                    for c in changes {
                        if let (Some(range), Some(insert)) = (c.get("range"), c.get("text").and_then(Value::as_str)) {
                            apply_change(text, apply_units, split, range, insert);
                        } else if let Some(insert) = c.get("text").and_then(Value::as_str) {
                            *text = insert.to_string();
                        }
                    }
                }
                *old_version = version;
                if !fault("no-diagnostics") {
                    let mut label = Some(json!(version));
                    if fault("diagnostics-own-version") {
                        own_version += 1;
                        label = Some(json!(own_version));
                    } else if fault("future-diagnostics-version") {
                        label = Some(json!(version + 1));
                    }
                    // The lawful stale faults publish the previous text's
                    // diagnostics first, labelled with the previous version.
                    if fault("stale-diagnostics") || fault("stale-unversioned-diagnostics") {
                        let stale =
                            if fault("stale-unversioned-diagnostics") { None } else { Some(json!(previous_version)) };
                        publish_diagnostics(&mut out, &faults, units, uri, &old_text, stale);
                    }
                    // ignore-changes and mislabelled-diagnostics publish the
                    // previous text, labelled with the new version.
                    let publish_text =
                        if fault("ignore-changes") || fault("mislabelled-diagnostics") { &old_text } else { text };
                    if fault("unversioned-diagnostics") || fault("stale-unversioned-diagnostics") {
                        label = None;
                    }
                    publish_diagnostics(&mut out, &faults, units, uri, publish_text, label);
                }
                if fault("codepoint-positions-after-change") {
                    // Mark one character of the all-emoji line (line 1) in
                    // code points: (1,1) is inside the first emoji in UTF-16
                    // but a boundary in code points, so it is a counting
                    // fault (D1's), not a change-handling fault.
                    let params = json!({
                        "uri": uri,
                        "version": version,
                        "diagnostics": [{"range": range(1, 0, 1, 1), "severity": 2, "message": "emoji"}]
                    });
                    out.frame(
                        &json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": params}),
                    );
                }
            }
            if method == "initialized" && fault("register-sync") {
                // A dynamic registration lspcheck didn't ask for; it answers
                // it with success.
                out.frame(&json!({
                    "jsonrpc": "2.0",
                    "id": 777,
                    "method": "client/registerCapability",
                    "params": {"registrations": [{"id": "1", "method": "textDocument/didChange", "registerOptions": {"documentSelector": null, "syncKind": 2}}]}
                }));
            }
            if method == "initialized" {
                // Like many servers, ask for a progress token. reused-request-id
                // asks again with the same id in the same write, before the
                // answer to the first can have arrived.
                let create = |token: &str| json!({"jsonrpc": "2.0", "id": 1, "method": "window/workDoneProgress/create", "params": {"token": token}});
                if fault("reused-request-id") {
                    let mut both = out.encode(&create("first"));
                    both.extend(out.encode(&create("second")));
                    out.raw(&both);
                } else {
                    out.frame(&create("first"));
                }
                if fault("await-progress-answer") {
                    // Like gopls: handle nothing else until the answer comes.
                    while let Some(frame) = input.next() {
                        let answer = serde_json::from_slice::<Value>(&frame.1)
                            .is_ok_and(|v| v.get("method").is_none() && v.get("id") == Some(&json!(1)));
                        if answer {
                            break;
                        }
                        queued.push_back(frame);
                    }
                }
            }
            if method == "initialized" && fault("needs-configuration") {
                // Like a server that reads its settings before doing
                // anything else: handle nothing until the answer comes.
                let id = json!("standin-config");
                let items = json!([{"section": "standin.mode"}]);
                out.frame(&json!({"jsonrpc": "2.0", "id": id, "method": "workspace/configuration", "params": {"items": items}}));
                while let Some(frame) = input.next() {
                    let Ok(v) = serde_json::from_slice::<Value>(&frame.1) else { continue };
                    if v.get("method").is_none() && v.get("id") == Some(&id) {
                        configured = v.get("result") == Some(&json!(["ready"]));
                        eprintln!("standin: configuration answered with {}", v.get("result").unwrap_or(&Value::Null));
                        break;
                    }
                    queued.push_back(frame);
                }
            }
            if method == "initialized" && fault("bad-message-type") {
                out.frame(&json!({"jsonrpc": "2.0", "method": "window/logMessage", "params": {"type": 9, "message": "no such type"}}));
            }
            if method == "initialized" && fault("register-other") {
                // A dynamic registration of a method D3 doesn't depend on;
                // it must not make D3 an error.
                out.frame(&json!({
                    "jsonrpc": "2.0",
                    "id": 777,
                    "method": "client/registerCapability",
                    "params": {"registrations": [{"id": "1", "method": "workspace/didChangeWatchedFiles", "registerOptions": {"documents": []}}]}
                }));
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
            if method == "exit" && fault("inflated-on-exit") {
                out.raw(b"Content-Length: 2097152\r\n\r\n");
                out.raw(
                    &serde_json::to_vec(&json!({
                        "jsonrpc": "2.0", "id": "exit-progress", "method": "window/workDoneProgress/create",
                        "params": {"token": "exit"}
                    }))
                    .unwrap(),
                );
                continue;
            }
            if method == "exit" && !fault("ignore-exit") {
                if fault("replay-gate") && replay_gate_id.is_some() {
                    std::process::exit(9);
                }
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
        if fault("exit-at-version-42")
            && method.starts_with("textDocument/")
            && msg
                .pointer("/params/textDocument/uri")
                .and_then(Value::as_str)
                .and_then(|u| docs.get(u))
                .is_some_and(|(_, version)| *version == 42)
        {
            std::process::exit(0);
        }
        if fault("stop-at-crlf")
            && method.starts_with("textDocument/")
            && msg.pointer("/params/textDocument/uri").and_then(Value::as_str).is_some_and(|u| u.ends_with("crlf.txt"))
        {
            no_more_answers = true;
            continue;
        }
        if method == "textDocument/hover" && fault("exit-on-first-hover") {
            std::process::exit(17);
        }
        if fault("drop-first-request")
            && !dropped_first
            && method.starts_with("textDocument/")
            && msg.pointer("/params/textDocument/uri").and_then(Value::as_str).is_some()
        {
            dropped_first = true;
            continue;
        }
        // A request about a document that isn't open: the D4 faults decide
        // its fate. `hover` answers null for every URI, so it is included.
        let unopened =
            matches!(
                method.as_str(),
                "textDocument/hover"
                    | "textDocument/documentSymbol"
                    | "textDocument/foldingRange"
                    | "textDocument/documentHighlight"
                    | "textDocument/selectionRange"
            ) && msg.pointer("/params/textDocument/uri").and_then(Value::as_str).is_some_and(|u| !docs.contains_key(u));
        if unopened && fault("ignore-unopened") {
            continue;
        }
        if unopened && fault("stop-after-unopened") {
            no_more_answers = true;
            continue;
        }
        if unopened && fault("unopened-error") {
            out.frame(&err(&id, -32602, "document not open"));
            continue;
        }
        // A numeric string id turned into a number: the wrong JSON type.
        let id = match id.as_str().filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())) {
            Some(s) if fault("string-id-to-number") => s.parse::<i64>().map_or(id, |n| json!(n)),
            _ => id,
        };
        let id = if let Some(s) = id.as_str().filter(|s| !s.is_ascii()) {
            if fault("mangles-odd-ids") { json!(s.chars().filter(|c| c.is_ascii()).collect::<String>()) } else { id }
        } else {
            id
        };
        if fault("replay-gate") && method == "m" {
            replay_gate_id = Some(id);
            out.frame(
                &json!({"jsonrpc": "2.0", "id": "cfg", "method": "workspace/configuration", "params": {"items": []}}),
            );
            continue;
        }
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
            if fault("needs-init-option")
                && msg.pointer("/params/initializationOptions/standin") != Some(&json!("ready"))
            {
                out.frame(&err(&id, -32001, "the stand-in needs initializationOptions {\"standin\": \"ready\"}"));
                continue;
            }
            if fault("needs-workspace-file")
                && let Err(why) = workspace_ready(&msg)
            {
                out.frame(&err(&id, -32001, &why));
                continue;
            }
            initialized = true;
            let offered = |name: &str| {
                msg.pointer("/params/capabilities/general/positionEncodings")
                    .and_then(Value::as_array)
                    .is_some_and(|a| a.iter().any(|e| e == name))
            };
            let encoding;
            (encoding, units) = if fault("bad-encoding") {
                ("utf-7", Units::Utf16)
            } else if fault("utf32-encoding") && offered("utf-32") {
                ("utf-32", Units::CodePoints)
            } else if fault("utf8-encoding") && offered("utf-8") {
                ("utf-8", Units::Bytes)
            } else if fault("utf16-positions-as-utf8") && offered("utf-8") {
                ("utf-8", Units::Utf16)
            } else if fault("utf8-positions-as-utf16") {
                ("utf-16", Units::Bytes)
            } else if fault("codepoint-positions") {
                ("utf-16", Units::CodePoints)
            } else {
                ("utf-16", Units::Utf16)
            };
            negotiated_units = match encoding {
                "utf-8" => Units::Bytes,
                "utf-32" => Units::CodePoints,
                _ => Units::Utf16,
            };
            let mut result = json!({
                "capabilities": {
                    "positionEncoding": encoding,
                    "textDocumentSync": {"openClose": true, "change": 2},
                    "hoverProvider": true,
                    "workspaceSymbolProvider": true,
                    "documentSymbolProvider": true,
                    "foldingRangeProvider": true,
                    "documentHighlightProvider": true,
                    "selectionRangeProvider": true
                },
                "serverInfo": {"name": "lspcheck-standin"}
            });
            if fault("no-open-close") {
                result["capabilities"]["textDocumentSync"] = json!({"openClose": false, "change": 0});
            }
            if fault("full-sync") {
                result["capabilities"]["textDocumentSync"] = json!({"openClose": true, "change": 1});
            }
            if fault("no-change-sync") {
                result["capabilities"]["textDocumentSync"] = json!({"openClose": true, "change": 0});
            }
            if [
                "advertises-unimplemented",
                "capability-other-error",
                "ignores-advertised",
                "method-not-found-after-result",
            ]
            .iter()
            .any(|f| fault(f))
            {
                result["capabilities"]["colorProvider"] = json!(true);
            }
            if fault("no-capabilities") {
                result = json!({"serverInfo": {"name": "lspcheck-standin"}});
            }
            if fault("fill-stdin-on-initialized") {
                // In the same write as the result, so the client reads both at
                // once and answers before its next request. `initialized` and
                // the answer then leave 30 bytes of a 64 KiB pipe: too few for
                // the client's next request while this server isn't reading.
                let mut bytes = out.encode(&ok(&id, result));
                bytes.extend(out.encode(&config_filling(65_536 - 74 - 30)));
                out.raw(&bytes);
                // Stop reading before `initialized` can even arrive, so what
                // the client writes next stays in the pipe.
                std::thread::sleep(std::time::Duration::from_secs(3));
                continue;
            }
            if fault("parse-error-with-initialize") {
                let mut bytes = out.encode(&ok(&id, result));
                bytes.extend(out.encode(&err(&Value::Null, -32700, "early parse error")));
                out.raw(&bytes);
            } else {
                out.frame(&ok(&id, result));
            }
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
        if !configured {
            out.frame(&err(&id, -32603, "the stand-in isn't configured: setting standin.mode isn't \"ready\""));
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
        if fault("drops-negative-ids") && id.as_i64().is_some_and(|n| n < 0) {
            dropped = true;
            continue;
        }
        if wrong_param_types(&method, msg.get("params").unwrap_or(&Value::Null)) {
            if fault("dies-on-wrong-params") {
                std::process::exit(2);
            }
            if fault("accepts-wrong-params") {
                out.frame(&ok(&id, Value::Null));
                continue;
            }
            let code = if fault("wrong-params-other-code") { -32603 } else { -32602 };
            let mut reply = err(&id, code, "invalid params");
            if fault("wrong-params-null-error") {
                reply["error"] = Value::Null;
            } else if fault("wrong-params-string-code") {
                reply["error"]["code"] = json!("-32602");
            } else if fault("wrong-params-large-code") {
                reply["error"]["code"] = json!(2147483648_i64);
            } else if fault("wrong-params-missing-message") {
                reply["error"].as_object_mut().unwrap().remove("message");
            } else if fault("wrong-params-numeric-message") {
                reply["error"]["message"] = json!(42);
            } else if fault("wrong-params-result-and-error") {
                reply["result"] = Value::Null;
            }
            out.frame(&reply);
            after_odd_input = true;
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
        let follows_odd_input = method == "lspcheck/probe" && std::mem::take(&mut after_odd_input);
        if follows_odd_input && fault("big-reply-then-exit") {
            let mut reply = err(&id, -32601, "method not found");
            reply["error"]["data"] = json!("x".repeat(2 * 1024 * 1024));
            out.frame(&reply);
            std::process::exit(0);
        }
        if method == "shutdown"
            && let Some(rest) = unfinished.take()
        {
            out.raw(&rest);
        }
        if method == "shutdown"
            && let Some(color) = answered_color.take()
        {
            out.frame(&err(&color, -32601, "method not found"));
        }
        if method == "textDocument/hover" && fault("echo-positions") {
            let uri = msg.pointer("/params/textDocument/uri").and_then(Value::as_str).unwrap_or("");
            let position = &msg["params"]["position"];
            eprintln!(
                "standin: hover at {} {}:{}",
                uri.rsplit('/').next().unwrap_or(uri),
                position["line"],
                position["character"]
            );
        }
        if method == "textDocument/hover"
            && ["defer-hover-until-shutdown", "advertised-error-after-shutdown", "advertised-result-after-shutdown"]
                .iter()
                .any(|name| fault(name))
        {
            held_hovers.push(id);
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
                if fault("flood-on-shutdown") {
                    out.raw(&vec![b'x'; 2 * 1024 * 1024]);
                }
                if fault("inflated-before-shutdown") {
                    out.raw(b"Content-Length: 2097152\r\n\r\n{\"jsonrpc\":\"2.0\",");
                }
                let mut reply = ok(&id, result);
                if fault("shutdown-no-jsonrpc") {
                    reply.as_object_mut().unwrap().remove("jsonrpc");
                }
                out.frame(&reply);
                let replies: Vec<Value> = held_hovers
                    .drain(..)
                    .map(|id| {
                        if fault("advertised-error-after-shutdown") {
                            err(&id, -32601, "method not found")
                        } else {
                            ok(&id, Value::Null)
                        }
                    })
                    .collect();
                if fault("advertised-error-after-shutdown") || fault("advertised-result-after-shutdown") {
                    let _helper = Command::new(std::env::current_exe().unwrap())
                        .arg("--reply-after-input")
                        .arg(serde_json::to_string(&replies).unwrap())
                        .stdin(Stdio::inherit())
                        .stdout(Stdio::inherit())
                        .stderr(Stdio::null())
                        .spawn()
                        .unwrap();
                    std::process::exit(0);
                }
                for reply in replies {
                    out.frame(&reply);
                }
            }
            // K1 opens a document before asking, so only its workspace/symbol
            // is affected (the other scenarios ask with none open). Like a
            // server that reads settings before going on, this one holds what
            // follows a request until the answer arrives: if lspcheck records
            // but doesn't answer the request, the log notification stays half
            // written and F1 would see a truncated frame, though the server is
            // behaving lawfully.
            "workspace/symbol" if fault("config-before-log-finish") && !docs.is_empty() => {
                let config_id = json!("standin-config");
                let config = json!({
                    "jsonrpc": "2.0",
                    "id": config_id,
                    "method": "workspace/configuration",
                    "params": {"items": []}
                });
                let log = out.encode(&json!({
                    "jsonrpc": "2.0",
                    "method": "window/logMessage",
                    "params": {"type": 4, "message": "configuration answered"}
                }));
                let mut bytes = out.encode(&ok(&id, Value::Null));
                bytes.extend(out.encode(&config));
                bytes.push(log[0]);
                out.raw(&bytes);
                while let Some(frame) = input.next() {
                    let Ok(v) = serde_json::from_slice::<Value>(&frame.1) else { continue };
                    if v.get("method").is_none() && v.get("id") == Some(&config_id) {
                        out.raw(&log[1..]);
                        break;
                    }
                    queued.push_back(frame);
                }
            }
            "textDocument/hover"
                if fault("hover-invalid-second-result") || fault("hover-invalid-result-after-error") =>
            {
                let first = if fault("hover-invalid-result-after-error") {
                    err(&id, -32603, "failed")
                } else {
                    ok(&id, Value::Null)
                };
                let mut bytes = out.encode(&first);
                bytes.extend(out.encode(&ok(&id, json!(3))));
                out.raw(&bytes);
            }
            "textDocument/hover" if fault("hover-contents-number") => out.frame(&ok(&id, json!({"contents": 3}))),
            "workspace/symbol" if fault("symbol-missing-name") => {
                // Otherwise a whole SymbolInformation, in a document lspcheck never opens.
                let symbol = json!({"kind": 13, "location": {"uri": ELSEWHERE, "range": range(0, 0, 0, 1)}});
                out.frame(&ok(&id, json!([symbol])));
            }
            "textDocument/hover" | "workspace/symbol" => out.frame(&ok(&id, Value::Null)),
            "textDocument/documentSymbol"
            | "textDocument/foldingRange"
            | "textDocument/documentHighlight"
            | "textDocument/selectionRange" => {
                let text = msg
                    .pointer("/params/textDocument/uri")
                    .and_then(Value::as_str)
                    .and_then(|u| docs.get(u).map(|(t, _)| t));
                // A closed document, answered from the file on disk.
                let disk = if text.is_none() && fault("closed-from-disk") {
                    msg.pointer("/params/textDocument/uri")
                        .and_then(Value::as_str)
                        .and_then(|u| closed.get(u).and_then(|u| disk_text(u.as_str())))
                } else {
                    None
                };
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                let result = match (text, disk) {
                    (Some(t), _) => document_answer(&method, &params, t, units, split, &faults),
                    (None, Some(t)) => document_answer(&method, &params, &t, units, split, &faults),
                    _ => Value::Null,
                };
                out.frame(&ok(&id, result));
                if method == "textDocument/documentSymbol" && fault("invalid-duplicate-symbols") {
                    // Offset 1 of line 0 is inside its first emoji in UTF-8, UTF-16 alike.
                    let inside = json!({"start": {"line": 0, "character": 1}, "end": {"line": 0, "character": 1}});
                    let symbol = json!({"name": "x", "kind": 13, "range": inside, "selectionRange": inside});
                    out.frame(&ok(&id, json!([symbol])));
                }
            }
            "textDocument/documentColor" if fault("capability-other-error") => {
                out.frame(&err(&id, -32803, "no colors today"));
            }
            "textDocument/documentColor" if fault("ignores-advertised") => {}
            "textDocument/documentColor" if fault("method-not-found-after-result") => {
                out.frame(&ok(&id, json!([])));
                answered_color = Some(id);
            }
            _ if fault("early-replies") => out.frame(&ok(&id, Value::Null)),
            "lspcheck/unknownRequest" if fault("big-frame-before-unknown-reply") => {
                let text = "x".repeat(2 * 1024 * 1024);
                out.frame(
                    &json!({"jsonrpc": "2.0", "method": "window/logMessage", "params": {"type": 4, "message": text}}),
                );
                out.frame(&err(&id, -32601, "method not found"));
            }
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
            "lspcheck/probe" if fault("inflated-on-first-probe") && !probed => {
                probed = true;
                out.raw(b"Content-Length: 2097152\r\n\r\n{\"jsonrpc\":\"2.0\",");
                out.frame(&err(&id, -32601, "method not found"));
            }
            m if m.starts_with("$/") && fault("inflated-on-dollar-request") => {
                out.raw(b"Content-Length: 2097152\r\n\r\n{\"jsonrpc\":\"2.0\",");
                out.frame(&err(&id, -32601, "method not found"));
            }
            m if m.starts_with("$/") && fault("big-dollar-reply") => {
                let mut reply = err(&id, -32601, "method not found");
                reply["error"]["data"] = json!("x".repeat(2 * 1024 * 1024));
                out.frame(&reply);
            }
            "lspcheck/probe" if fault("bad-json-reply-after-probe") && follows_odd_input => {
                let mut bytes = out.encode(&err(&id, -32601, "method not found"));
                bytes.extend(out.encode(&err(&Value::Null, -32600, "invalid request")));
                out.raw(&bytes);
            }
            "lspcheck/probe" if fault("trailing-fragment") && (follows_odd_input || id == json!("7")) => {
                let mut reply = out.encode(&err(&id, -32601, "method not found"));
                let bytes = out.encode(&json!({
                    "jsonrpc": "2.0",
                    "method": "window/logMessage",
                    "params": {"type": 4, "message": "still writing"}
                }));
                reply.push(bytes[0]);
                out.raw(&reply);
                unfinished = Some(bytes[1..].to_vec());
            }
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

/// How the stand-in counts character offsets: the negotiated encoding's
/// units, or another's when a fault says so.
#[derive(Clone, Copy)]
enum Units {
    Bytes,
    Utf16,
    CodePoints,
}

impl Units {
    /// The length of `s` in these units.
    fn of(self, s: &str) -> usize {
        match self {
            Units::Bytes => s.len(),
            Units::Utf16 => s.encode_utf16().count(),
            Units::CodePoints => s.chars().count(),
        }
    }
}

/// The byte offset of a position in `text`, counted in `units`, clamping a
/// character past the end of its line to the line's length; `None` for a
/// line the document doesn't have, or a character inside a character.
fn byte_offset(text: &str, units: Units, split: LineSplit, p: &Value) -> Option<usize> {
    let line = p["line"].as_u64()? as usize;
    let character = p["character"].as_u64()? as usize;
    let mut start = 0;
    for (n, l) in lines(text, split).into_iter().enumerate() {
        if n == line {
            let limit = character.min(units.of(l));
            let mut bytes = 0;
            let mut counted = 0;
            for c in l.chars() {
                if counted == limit {
                    return Some(start + bytes);
                }
                counted += match units {
                    Units::Bytes => c.len_utf8(),
                    Units::Utf16 => c.len_utf16(),
                    Units::CodePoints => 1,
                };
                if counted > limit {
                    return None;
                }
                bytes += c.len_utf8();
            }
            return Some(start + bytes);
        }
        start += l.len();
        let rest = &text[start..];
        start += if rest.starts_with("\r\n") { 2 } else { 1 };
    }
    None
}

/// Applies one content change to `text`, the range counted in `units`. A
/// change that doesn't fit is ignored: the stand-in's own texts always fit.
fn apply_change(text: &mut String, units: Units, split: LineSplit, range: &Value, insert: &str) {
    let Some(start) = byte_offset(text, units, split, &range["start"]) else { return };
    let Some(end) = byte_offset(text, units, split, &range["end"]) else { return };
    let (start, end) = (start.min(end), end.max(start));
    let mut out = String::with_capacity(text.len() + insert.len());
    out.push_str(&text[..start]);
    out.push_str(insert);
    out.push_str(&text[end..]);
    *text = out;
}

/// How the stand-in splits a document's lines, per the line-splitting faults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineSplit {
    /// The spec's way: split at `\n`, `\r\n` and `\r`.
    Spec,
    /// Split at `\n` only; a `\r` is an ordinary character.
    LfOnly,
    /// Count `\r\n` as two breaks, leaving an empty line between.
    CrLfTwo,
    /// Split at `\n` and `\r\n`, but not a lone `\r`.
    NoLoneCr,
}

/// The line split the stand-in uses, from the active faults.
fn line_split(faults: &[String]) -> LineSplit {
    if faults.iter().any(|f| f == "lf-only-lines") {
        LineSplit::LfOnly
    } else if faults.iter().any(|f| f == "crlf-two-breaks") {
        LineSplit::CrLfTwo
    } else if faults.iter().any(|f| f == "no-lone-cr") {
        LineSplit::NoLoneCr
    } else {
        LineSplit::Spec
    }
}

/// A document's lines, split per `split`, without their ends.
fn lines(text: &str, split: LineSplit) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let is_break = match split {
            LineSplit::Spec | LineSplit::CrLfTwo => bytes[i] == b'\n' || bytes[i] == b'\r',
            LineSplit::LfOnly => bytes[i] == b'\n',
            LineSplit::NoLoneCr => {
                if bytes[i] == b'\n' {
                    i == 0 || bytes[i - 1] != b'\r'
                } else {
                    bytes[i] == b'\r' && i + 1 < bytes.len() && bytes[i + 1] == b'\n'
                }
            }
        };
        if is_break {
            out.push(&text[start..i]);
            // A `\r\n` is one break (two bytes), except counted as two in CrLfTwo.
            let skip =
                if bytes[i] == b'\r' && i + 1 < bytes.len() && bytes[i + 1] == b'\n' && split != LineSplit::CrLfTwo {
                    2
                } else {
                    1
                };
            i += skip;
            start = i;
        } else {
            i += 1;
        }
    }
    out.push(&text[start..]);
    out
}

/// Each ASCII word (`[A-Za-z]+`): its line, its start and end in `units`,
/// and its text.
fn words(text: &str, units: Units, split: LineSplit) -> Vec<(usize, usize, usize, &str)> {
    let mut out = Vec::new();
    for (n, line) in lines(text, split).into_iter().enumerate() {
        let mut start = None;
        for (i, c) in line.char_indices().chain([(line.len(), ' ')]) {
            match (c.is_ascii_alphabetic(), start) {
                (true, None) => start = Some(i),
                (false, Some(s)) => {
                    out.push((n, units.of(&line[..s]), units.of(&line[..i]), &line[s..i]));
                    start = None;
                }
                _ => {}
            }
        }
    }
    out
}

fn range(sl: usize, sc: usize, el: usize, ec: usize) -> Value {
    json!({"start": {"line": sl, "character": sc}, "end": {"line": el, "character": ec}})
}

/// The answer to a request about an open document.
fn document_answer(
    method: &str,
    params: &Value,
    text: &str,
    units: Units,
    split: LineSplit,
    faults: &[String],
) -> Value {
    let fault = |name: &str| faults.iter().any(|f| f == name);
    let words = words(text, units, split);
    let lines = lines(text, split);
    let at = |p: &Value| (p["line"].as_u64().unwrap_or(0) as usize, p["character"].as_u64().unwrap_or(0) as usize);
    // The word a position touches.
    let word_at = |(line, ch): (usize, usize)| words.iter().find(|(l, s, e, _)| *l == line && *s <= ch && ch <= *e);
    match method {
        "textDocument/documentSymbol" => words
            .iter()
            .map(|(l, s, e, w)| json!({"name": w, "kind": 13, "range": range(*l, *s, *l, *e), "selectionRange": range(*l, *s, *l, *e)}))
            .collect(),
        "textDocument/foldingRange" => {
            // From the end of the first line to the end of the last with text.
            let last = lines.iter().rposition(|l| !l.is_empty()).unwrap_or(0);
            if last == 0 {
                return json!([]);
            }
            let mut folding = json!({"startLine": 0, "startCharacter": units.of(lines[0]), "endLine": last, "endCharacter": units.of(lines[last])});
            if fault("custom-folding-range-kind") {
                folding["kind"] = json!("lspcheck.custom");
            }
            json!([folding])
        }
        "textDocument/documentHighlight" => match word_at(at(&params["position"])) {
            Some((_, _, _, word)) => words
                .iter()
                .filter(|(.., w)| w == word)
                .map(|(l, s, e, _)| json!({"range": range(*l, *s, *l, *e), "kind": 1}))
                .collect(),
            None => json!([]),
        },
        "textDocument/selectionRange" => {
            let positions: Vec<Value> = params["positions"].as_array().cloned().unwrap_or_default();
            let mut out: Vec<Value> = positions
                .iter()
                .map(|p| {
                    let (line, ch) = at(p);
                    let Some(text) = lines.get(line) else { return json!({"range": range(line, ch, line, ch)}) };
                    let parent = json!({"range": range(line, 0, line, units.of(text))});
                    match word_at((line, ch)) {
                        Some((l, s, e, _)) => json!({"range": range(*l, *s, *l, *e), "parent": parent}),
                        None => json!({"range": range(line, ch, line, ch), "parent": parent}),
                    }
                })
                .collect();
            // The selection faults.
            if fault("selection-outside") {
                // The range is the word *before* the position, still on a
                // boundary: it doesn't contain the position.
                for (i, p) in positions.iter().enumerate() {
                    let (line, ch) = at(p);
                    if line >= lines.len() {
                        continue;
                    }
                    let on_line: Vec<(usize, usize)> = words
                        .iter()
                        .filter(|(l, _, _, _)| *l == line)
                        .map(|(_, s, e, _)| (*s, *e))
                        .collect();
                    // A word strictly before the position (ends before it),
                    // the latest such; else a word strictly after it (starts
                    // after it), the earliest such; else an empty range at a
                    // boundary that isn't the position.
                    let before = on_line.iter().filter(|(_, e)| *e < ch).max_by_key(|(_, e)| *e).copied();
                    let after = on_line.iter().filter(|(s, _)| *s > ch).min_by_key(|(s, _)| *s).copied();
                    let (s, e) = before.or(after).unwrap_or(if ch > 0 { (0, 0) } else { (1, 1) });
                    out[i]["range"] = range(line, s, line, e);
                }
            }
            if fault("selection-short") {
                // One entry fewer than the positions: drop the last.
                if !out.is_empty() {
                    out.pop();
                }
            }
            if fault("selection-parent-inside") {
                // The parent is a range strictly inside its child.
                for entry in &mut out {
                    let Some(rng) = entry.get("range") else { continue };
                    let s = rng["start"]["line"].as_u64().unwrap_or(0) as usize;
                    let cs = rng["start"]["character"].as_u64().unwrap_or(0) as usize;
                    let ce = rng["end"]["character"].as_u64().unwrap_or(0) as usize;
                    // A range strictly inside: [cs+1, ce-1] if the child has
                    // width, else an empty range at the child's start.
                    let (ps, pe) = if ce > cs + 1 { (cs + 1, ce - 1) } else { (cs, cs) };
                    entry["parent"] = json!({"range": range(s, ps, s, pe)});
                }
            }
            if fault("empty-selection") {
                // Lawful: an empty range at each position, parent the whole
                // line.
                for (i, p) in positions.iter().enumerate() {
                    let (line, ch) = at(p);
                    let Some(text) = lines.get(line) else { continue };
                    out[i] = json!({
                        "range": range(line, ch, line, ch),
                        "parent": {"range": range(line, 0, line, units.of(text))},
                    });
                }
            }
            json!(out)
        }
        _ => Value::Null,
    }
}

/// A URI the stand-in publishes about without it ever being opened.
const ELSEWHERE: &str = "file:///lspcheck-standin/elsewhere.txt";

/// Publishes a warning for each ASCII word of a document, labelled with its
/// version when there is one, plus what the lawful position faults add.
fn publish_diagnostics<W: Write>(
    out: &mut Writer<W>,
    faults: &[String],
    units: Units,
    uri: &str,
    text: &str,
    version: Option<Value>,
) {
    let fault = |name: &str| faults.iter().any(|f| f == name);
    let split = line_split(faults);
    let mut diagnostics: Vec<Value> = words(text, units, split)
        .iter()
        .map(|(l, s, e, w)| {
            json!({"range": range(*l, *s, *l, *e), "severity": 2, "source": "standin", "message": format!("word: {w}")})
        })
        .collect();
    let lines = lines(text, split);
    // A position on the first line with non-ASCII text, one character in: on a
    // character boundary with non-ASCII text before it, so informative for D5.
    if let Some(n) = lines.iter().position(|l| !l.is_ascii()) {
        let first = lines[n].chars().next().unwrap();
        let at = units.of(first.to_string().as_str());
        diagnostics.push(json!({"range": range(n, at, n, at), "severity": 2, "source": "standin", "message": "a position on a non-ASCII line"}));
    }
    if fault("past-line-end") {
        let end = units.of(lines[0]) + 3;
        diagnostics.push(json!({"range": range(0, 0, 0, end), "severity": 3, "message": "past the line"}));
        diagnostics.push(json!({"range": range(0, 0, lines.len() + 5, 0), "severity": 3, "message": "past the end"}));
    }
    if fault("grapheme-split")
        && let Some((n, at)) = lines.iter().enumerate().find_map(|(n, l)| l.find('\u{301}').map(|at| (n, at)))
    {
        let start = units.of(&lines[n][..at]);
        let end = units.of(&lines[n][..at + '\u{301}'.len_utf8()]);
        diagnostics.push(json!({"range": range(n, start, n, end), "severity": 3, "message": "a lone combining mark"}));
    }
    if fault("positions-in-data") {
        for d in &mut diagnostics {
            d["data"] = json!({"line": 0, "character": 1, "range": range(0, 1, 0, 3)});
        }
    }
    if fault("foreign-locations") {
        if let Some(first) = diagnostics.first_mut() {
            first["relatedInformation"] =
                json!([{"location": {"uri": ELSEWHERE, "range": range(0, 1, 0, 3)}, "message": "elsewhere"}]);
        }
        // With a version lspcheck never sent for it: no position or version
        // in a URI lspcheck didn't open is judged.
        let params = json!({"uri": ELSEWHERE, "version": 999, "diagnostics": [{"range": range(0, 1, 0, 1), "message": "elsewhere"}]});
        out.frame(&json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": params}));
    }
    let mut params = json!({"uri": uri, "diagnostics": diagnostics});
    if let Some(v) = version {
        params["version"] = v;
    }
    out.frame(&json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": params}));
}

/// For needs-workspace-file: whether the workspace root given in `initialize`
/// holds `standin.toml` and each file named by a `require = "PATH"` line in it.
fn workspace_ready(initialize: &Value) -> Result<(), String> {
    let root = initialize
        .pointer("/params/rootUri")
        .and_then(Value::as_str)
        .and_then(uri_path)
        .ok_or("the stand-in needs a rootUri")?;
    let manifest = std::fs::read_to_string(root.join("standin.toml"))
        .map_err(|e| format!("the stand-in needs standin.toml in its workspace: {e}"))?;
    for line in manifest.lines() {
        if let Some(path) = line.trim().strip_prefix("require = \"").and_then(|rest| rest.strip_suffix('"'))
            && !root.join(path).is_file()
        {
            return Err(format!("the stand-in needs {path} in its workspace, as standin.toml says"));
        }
        if let Some(path) = line.trim().strip_prefix("require_directory = \"").and_then(|rest| rest.strip_suffix('"'))
            && !root.join(path).is_dir()
        {
            return Err(format!("the stand-in needs directory {path} in its workspace, as standin.toml says"));
        }
        #[cfg(unix)]
        if let Some(path) = line.trim().strip_prefix("require_executable = \"").and_then(|rest| rest.strip_suffix('"'))
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata = std::fs::metadata(root.join(path)).map_err(|e| format!("can't inspect {path}: {e}"))?;
            if metadata.permissions().mode() & 0o111 == 0 {
                return Err(format!("the stand-in needs executable {path} in its workspace, as standin.toml says"));
            }
        }
    }
    Ok(())
}

/// The path a `file:` URI names, percent-decoded. On Windows the `/` before
/// the drive letter is dropped.
fn uri_path(uri: &str) -> Option<std::path::PathBuf> {
    let bytes = uri.strip_prefix("file://")?.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let h = (bytes[i + 1] as char).to_digit(16)?;
            let l = (bytes[i + 2] as char).to_digit(16)?;
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let path = String::from_utf8(out).ok()?;
    let windows_drive = cfg!(windows) && path.as_bytes().get(2) == Some(&b':');
    Some(std::path::PathBuf::from(if windows_drive { &path[1..] } else { &path[..] }))
}

/// The file a `file:` URI names, read from disk (closed-from-disk); `None`
/// when the URI isn't a `file:` URI, the path isn't UTF-8, or the read fails.
fn disk_text(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let h = (bytes[i + 1] as char).to_digit(16)?;
            let l = (bytes[i + 2] as char).to_digit(16)?;
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    std::fs::read_to_string(String::from_utf8_lossy(&out).into_owned()).ok()
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

fn reply_after_input(replies: &str) {
    let replies: Vec<Value> = serde_json::from_str(replies).unwrap();
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input).unwrap();
    let stdout = std::io::stdout();
    let mut out = Writer { out: stdout.lock(), faults: Vec::new(), holding: false, held: Vec::new() };
    for reply in replies {
        out.frame(&reply);
    }
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
        if fault("extra-properties") {
            let extra = |o: &mut Value| {
                if let Some(o) = o.as_object_mut() {
                    o.insert("lspcheckExtra".into(), json!({"standin": true}));
                }
            };
            for key in ["result", "params"] {
                match v.get_mut(key) {
                    Some(Value::Array(items)) => items.iter_mut().for_each(extra),
                    Some(o) => extra(o),
                    None => {}
                }
            }
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

fn wrong_param_types(method: &str, params: &Value) -> bool {
    match method {
        "textDocument/hover" | "textDocument/definition" => {
            !params.get("textDocument").is_some_and(Value::is_object)
                || !params.get("position").is_some_and(Value::is_object)
        }
        "textDocument/documentSymbol" => !params.get("textDocument").is_some_and(Value::is_object),
        "workspace/symbol" => !params.get("query").is_some_and(Value::is_string),
        _ => false,
    }
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
            // Only F4's shape: a request, $/setTrace, then another frame, all
            // in one read. Any notification would also match `initialized`
            // coalescing with J6's requests, which this fault must not touch.
            // Everything before $/setTrace is kept (the read may also hold the
            // end of an earlier frame), so exactly the second request is lost.
            let mut frames = Vec::new();
            let mut at = 0;
            while let Parsed::Frame((_, body), used) = self.parse(&self.buf[at..]) {
                let set_trace = serde_json::from_slice::<Value>(&body).is_ok_and(|v| {
                    v.get("id").is_none() && v.get("method").and_then(Value::as_str) == Some("$/setTrace")
                });
                frames.push((at, set_trace));
                at += used;
            }
            if let Some(i) = frames.iter().position(|&(_, set_trace)| set_trace)
                && i >= 1
                && i + 1 < frames.len()
            {
                eprintln!("standin: first-frame-per-read dropped a frame");
                self.buf.truncate(frames[i].0);
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

/// Whether `body` is a request whose id is J6's six-digit string. That is what
/// makes drop-concurrent act in protocol order without catching other probes.
fn is_digit_string_request(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body).is_ok_and(|v| {
        v.get("method").is_some_and(Value::is_string)
            && v.get("id")
                .and_then(Value::as_str)
                .is_some_and(|s| s.len() == 6 && s.bytes().all(|b| b.is_ascii_digit()))
    })
}
