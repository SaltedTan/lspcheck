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
    ("trailing-junk", "send one non-ASCII notification whose body is JSON followed by text, correctly counted"),
    ("bad-bodies", "on initialize, send 70 frames whose body isn't JSON"),
    ("overlong-last-frame", "on exit, send a non-ASCII log message declaring 10,000 more bytes than it has"),
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
    ("split-header", "drop input when a read ends inside a header section, as if each header came in one read"),
    ("split-utf8", "decode each read as UTF-8 on its own, mangling characters split across reads"),
    ("first-frame-per-read", "keep only the first frame of a read that holds three or more"),
    ("char-length-input", "count the Content-Length of incoming frames in characters, not bytes"),
    ("no-content-type", "ignore incoming frames that have a Content-Type header"),
    ("strict-charset", "ignore incoming frames whose charset isn't spelled utf-8"),
    ("exit-on-content-type", "exit when an incoming frame has a Content-Type header"),
    ("ignore-unknown-requests", "never answer requests for unknown methods"),
    ("exit-on-string-id", "exit when a request has a string id"),
    ("answers-reordered", "lawful: answer requests that arrive together in reverse order"),
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
    let mut out = Writer { out: std::io::stdout().lock(), faults: faults.clone(), holding: false, held: Vec::new() };
    if fault("stdout-banner") {
        out.raw(b"standin starting\n");
    }
    let mut input = Input { inner: std::io::stdin().lock(), buf: Vec::new(), faults: faults.clone() };
    let (mut initialized, mut shut_down) = (false, false);
    let mut children: Vec<Child> = Vec::new();
    loop {
        if !input.ready() {
            // About to wait for input: send what was held back, newest first.
            out.release();
        }
        let Some((headers, body)) = input.next() else { break };
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
        if fault("exit-on-string-id") && msg.get("id").is_some_and(Value::is_string) {
            std::process::exit(2);
        }
        let Some(id) = msg.get("id").cloned() else {
            // A notification.
            if method == "textDocument/didOpen" && fault("no-open-close") {
                std::process::exit(4);
            }
            if method == "exit" && !fault("ignore-exit") {
                if fault("big-frame-on-exit") {
                    let text = "x".repeat(2 * 1024 * 1024);
                    out.frame(&json!({"jsonrpc": "2.0", "method": "window/logMessage", "params": {"type": 4, "message": text}}));
                }
                if fault("overlong-last-frame") {
                    let body = r#"{"jsonrpc":"2.0","method":"window/logMessage","params":{"type":4,"message":"é"}}"#;
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
                let body = r#"{"jsonrpc":"2.0","method":"window/logMessage","params":{"type":4,"message":"é"}}junk"#;
                out.raw(format!("Content-Length: {}\r\n\r\n{body}", body.len()).as_bytes());
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
                "capabilities": {"positionEncoding": encoding, "textDocumentSync": 1, "hoverProvider": true},
                "serverInfo": {"name": "lspcheck-standin"}
            });
            if fault("no-open-close") {
                result["capabilities"]["textDocumentSync"] = json!({"openClose": false, "change": 0});
            }
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
            _ if fault("ignore-unknown-requests") => {}
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
            while let Parsed::Frame(_, used) = self.parse(&self.buf[at..]) {
                at += used;
                ends.push(at);
            }
            if ends.len() >= 3 {
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
