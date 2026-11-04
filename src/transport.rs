// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! The LSP base protocol's framing, read from raw bytes.
//!
//! [`FrameReader`] never trusts the server. While in step with the stream it
//! parses each header section where the previous frame ended, so extension
//! headers are accepted in any order. When a header section is invalid it
//! reports the bytes and resynchronizes on the next `Content-Length:` or
//! `Content-Type:` field, so later frames are still seen.
//!
//! The header and body size limits are lspcheck's own inspection budget, not
//! protocol rules: a body over the limit is skipped and reported as
//! [`Chunk::Oversized`], never as a framing error.

/// The longest header section inspected, in bytes.
pub const DEFAULT_MAX_HEADER_BYTES: usize = 1024 * 1024;
/// The largest body inspected, in bytes.
pub const DEFAULT_MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// One header field as written, with surrounding whitespace removed from the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub name: String,
    pub value: String,
}

/// What the reader found in the byte stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Chunk {
    /// A complete frame: its headers and exactly `Content-Length` body bytes.
    Frame { headers: Vec<Header>, body: Vec<u8> },
    /// Bytes outside any frame, such as a log line written to stdout.
    Garbage(Vec<u8>),
    /// A header section that isn't valid. The reader skips past it.
    BadHeader { block: Vec<u8>, reason: String },
    /// A well-formed frame whose body is larger than the inspection limit; it
    /// was skipped without being read.
    Oversized { headers: Vec<Header>, content_length: usize, limit: usize },
    /// The stream ended inside a frame: the header promised more body bytes.
    Truncated { headers: Vec<Header>, content_length: usize, received: usize },
    /// A header section that looks valid so far but is longer than the
    /// inspection limit. Its body can't be found, so the reader stops
    /// inspecting the stream from here on.
    HeaderLimit { limit: usize },
}

/// Field names that can anchor resynchronization.
const ANCHORS: [&[u8]; 2] = [b"content-length:", b"content-type:"];

/// An incremental frame parser. Feed it bytes as they arrive.
#[derive(Debug)]
pub struct FrameReader {
    buf: Vec<u8>,
    max_header: usize,
    max_body: usize,
    /// Whether the next byte should start a header section.
    synced: bool,
    pending: Option<(Vec<Header>, usize)>,
    /// An oversized body being skipped: its headers, length, and bytes left.
    skipping: Option<(Vec<Header>, usize, usize)>,
    /// Set after [`Chunk::HeaderLimit`]: everything after is discarded.
    stopped: bool,
}

impl Default for FrameReader {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_HEADER_BYTES, DEFAULT_MAX_BODY_BYTES)
    }
}

impl FrameReader {
    pub fn new(max_header: usize, max_body: usize) -> Self {
        Self { buf: Vec::new(), max_header, max_body, synced: true, pending: None, skipping: None, stopped: false }
    }

    /// Adds bytes and returns every chunk they complete.
    pub fn feed(&mut self, data: &[u8]) -> Vec<Chunk> {
        if self.stopped {
            return Vec::new();
        }
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        loop {
            if let Some((_, _, left)) = &mut self.skipping {
                let n = (*left).min(self.buf.len());
                self.buf.drain(..n);
                *left -= n;
                if *left > 0 {
                    break;
                }
                let (headers, content_length, _) = self.skipping.take().expect("skipping");
                out.push(Chunk::Oversized { headers, content_length, limit: self.max_body });
                self.synced = true;
                continue;
            }
            if let Some((_, len)) = &self.pending {
                let len = *len;
                if self.buf.len() < len {
                    break;
                }
                let (headers, _) = self.pending.take().expect("pending frame");
                let body: Vec<u8> = self.buf.drain(..len).collect();
                out.push(Chunk::Frame { headers, body });
                self.synced = true;
                continue;
            }
            if self.buf.is_empty() {
                break;
            }
            if self.synced {
                match self.try_header_here() {
                    Here::Parsed(headers, len) => self.start_body(headers, len),
                    Here::Wait => break,
                    Here::NotAHeader => self.synced = false,
                    Here::TooLong => {
                        out.push(self.stop());
                        break;
                    }
                }
                continue;
            }
            // Out of step: everything up to the next anchor is outside any frame.
            let Some(start) = find_anchor(&self.buf) else {
                let keep = partial_anchor(&self.buf);
                let cut = self.buf.len() - keep;
                if cut > 0 {
                    out.push(Chunk::Garbage(self.buf.drain(..cut).collect()));
                }
                break;
            };
            if start > 0 {
                out.push(Chunk::Garbage(self.buf.drain(..start).collect()));
            }
            // A header section starts at the anchor; judge it as the in-step path does.
            let window = &self.buf[..self.buf.len().min(self.max_header)];
            match find(window, b"\r\n\r\n") {
                Some(end) => {
                    let block: Vec<u8> = self.buf.drain(..end + 4).collect();
                    match parse_header_block(&block[..end]) {
                        Ok((headers, len)) => self.start_body(headers, len),
                        Err(reason) => out.push(Chunk::BadHeader { block, reason }),
                    }
                }
                None => match invalid_line_end(window) {
                    Some((end, reason)) => {
                        out.push(Chunk::BadHeader { block: self.buf.drain(..end).collect(), reason })
                    }
                    None if self.buf.len() >= self.max_header => {
                        out.push(self.stop());
                        break;
                    }
                    None => break,
                },
            }
        }
        out
    }

    fn start_body(&mut self, headers: Vec<Header>, len: usize) {
        if len > self.max_body {
            self.skipping = Some((headers, len, len));
        } else {
            self.pending = Some((headers, len));
        }
    }

    fn stop(&mut self) -> Chunk {
        self.stopped = true;
        self.buf.clear();
        Chunk::HeaderLimit { limit: self.max_header }
    }

    /// Tries to read a header section at the start of the buffer. The verdict
    /// depends only on the first `max_header` bytes, never on how they arrived.
    fn try_header_here(&mut self) -> Here {
        let window = &self.buf[..self.buf.len().min(self.max_header)];
        match find(window, b"\r\n\r\n") {
            Some(end) => match parse_header_block(&self.buf[..end]) {
                Ok((headers, len)) => {
                    self.buf.drain(..end + 4);
                    Here::Parsed(headers, len)
                }
                Err(_) => Here::NotAHeader,
            },
            None if invalid_line_end(window).is_some() => Here::NotAHeader,
            None if self.buf.len() >= self.max_header => Here::TooLong,
            None => Here::Wait,
        }
    }

    /// Ends the stream and returns whatever was left unfinished.
    pub fn finish(&mut self) -> Vec<Chunk> {
        let mut out = Vec::new();
        if self.stopped {
            return out;
        }
        let rest = std::mem::take(&mut self.buf);
        if let Some((headers, content_length, left)) = self.skipping.take() {
            out.push(Chunk::Truncated { headers, content_length, received: content_length - left });
        } else if let Some((headers, content_length)) = self.pending.take() {
            out.push(Chunk::Truncated { headers, content_length, received: rest.len() });
        } else if !rest.is_empty() {
            let header_like = (self.synced && invalid_line_end(&rest).is_none()) || find_anchor(&rest) == Some(0);
            if header_like {
                out.push(Chunk::BadHeader { block: rest, reason: "the stream ended inside a header section".into() });
            } else {
                out.push(Chunk::Garbage(rest));
            }
        }
        out
    }
}

enum Here {
    Parsed(Vec<Header>, usize),
    Wait,
    NotAHeader,
    TooLong,
}

/// Frames a body with a `Content-Length` header only.
pub fn encode(body: &[u8]) -> Vec<u8> {
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body);
    out
}

/// Frames a body with a `Content-Length` and a `Content-Type` header.
pub fn encode_with_type(body: &[u8], content_type: &str) -> Vec<u8> {
    let mut out = format!("Content-Length: {}\r\nContent-Type: {content_type}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body);
    out
}

/// The charset a `Content-Type` value declares, lowercased, if any. Invalid
/// values (which the reader rejects) declare nothing.
pub fn charset(content_type: &str) -> Option<String> {
    let (_, params) = parse_media_type(content_type.as_bytes()).ok()?;
    params.into_iter().find(|(name, _)| name.eq_ignore_ascii_case("charset")).map(|(_, v)| v.to_ascii_lowercase())
}

/// Parses an RFC 9110 media type: `type/subtype *( OWS ";" OWS [ name=value ] )`,
/// where a value is a token or a quoted string. Returns the type and the
/// parameters with quoted values unescaped.
pub fn parse_media_type(v: &[u8]) -> Result<(String, Vec<(String, String)>), String> {
    let shown = || String::from_utf8_lossy(v).into_owned();
    let mut i = 0;
    let token = |i: &mut usize| {
        let start = *i;
        while *i < v.len() && is_token_byte(v[*i]) {
            *i += 1;
        }
        (*i > start).then(|| String::from_utf8_lossy(&v[start..*i]).into_owned())
    };
    let ows = |i: &mut usize| {
        while *i < v.len() && (v[*i] == b' ' || v[*i] == b'\t') {
            *i += 1;
        }
    };
    let ty = token(&mut i).ok_or_else(|| format!("Content-Type {:?} doesn't start with a media type", shown()))?;
    if v.get(i) != Some(&b'/') {
        return Err(format!("Content-Type {:?} has no '/' in its media type", shown()));
    }
    i += 1;
    let sub = token(&mut i).ok_or_else(|| format!("Content-Type {:?} has no media subtype", shown()))?;
    let mut params: Vec<(String, String)> = Vec::new();
    loop {
        ows(&mut i);
        if i == v.len() {
            break;
        }
        if v[i] != b';' {
            return Err(format!("Content-Type {:?} has unexpected text after the media type", shown()));
        }
        i += 1;
        ows(&mut i);
        if i == v.len() || v[i] == b';' {
            continue;
        }
        let name = token(&mut i).ok_or_else(|| format!("Content-Type {:?} has a parameter without a name", shown()))?;
        if v.get(i) != Some(&b'=') {
            return Err(format!("Content-Type {:?} has a parameter without '='", shown()));
        }
        i += 1;
        let value = if v.get(i) == Some(&b'"') {
            i += 1;
            let mut out = Vec::new();
            loop {
                match v.get(i) {
                    None => return Err(format!("Content-Type {:?} has an unterminated quoted string", shown())),
                    Some(b'"') => {
                        i += 1;
                        break;
                    }
                    Some(b'\\') => match v.get(i + 1) {
                        Some(&c) if c == b'\t' || c == b' ' || (0x21..=0x7e).contains(&c) || c >= 0x80 => {
                            out.push(c);
                            i += 2;
                        }
                        _ => return Err(format!("Content-Type {:?} has a bad escape in a quoted string", shown())),
                    },
                    Some(&c) if c == b'\t' || c == b' ' || c == 0x21 || (0x23..=0x7e).contains(&c) || c >= 0x80 => {
                        out.push(c);
                        i += 1;
                    }
                    Some(_) => return Err(format!("Content-Type {:?} has a bad byte in a quoted string", shown())),
                }
            }
            String::from_utf8_lossy(&out).into_owned()
        } else {
            token(&mut i).ok_or_else(|| format!("Content-Type {:?} has a parameter without a value", shown()))?
        };
        if name.eq_ignore_ascii_case("charset") && params.iter().any(|(n, _)| n.eq_ignore_ascii_case("charset")) {
            return Err(format!("Content-Type {:?} declares charset twice", shown()));
        }
        params.push((name, value));
    }
    Ok((format!("{ty}/{sub}"), params))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn starts_with_ignore_case(data: &[u8], prefix: &[u8]) -> bool {
    data.len() >= prefix.len() && data[..prefix.len()].eq_ignore_ascii_case(prefix)
}

fn find_anchor(buf: &[u8]) -> Option<usize> {
    (0..buf.len()).find(|&i| ANCHORS.iter().any(|h| starts_with_ignore_case(&buf[i..], h)))
}

/// The length of the longest suffix of `buf` that is a proper prefix of an anchor.
fn partial_anchor(buf: &[u8]) -> usize {
    let longest = ANCHORS.iter().map(|h| h.len()).max().unwrap_or(0);
    (1..longest.min(buf.len() + 1))
        .rev()
        .find(|&n| {
            let tail = &buf[buf.len() - n..];
            ANCHORS.iter().any(|h| h.len() > n && h[..n].eq_ignore_ascii_case(tail))
        })
        .unwrap_or(0)
}

/// If some line in `buf` can't be part of a header section, how many bytes
/// to discard to get past it, and why. An incomplete last line counts only
/// once its name has a byte that can't be in a token.
fn invalid_line_end(buf: &[u8]) -> Option<(usize, String)> {
    let mut start = 0;
    while let Some(nl) = buf[start..].iter().position(|&b| b == b'\n') {
        let line = &buf[start..start + nl];
        let end = start + nl + 1;
        let Some(line) = line.strip_suffix(b"\r") else {
            return Some((end, format!("header line {:?} ends with a bare LF", String::from_utf8_lossy(line))));
        };
        if line.is_empty() {
            return None;
        }
        if let Err(reason) = parse_field(line) {
            return Some((end, reason));
        }
        start = end;
    }
    let rest = &buf[start..];
    if rest.is_empty() || rest == b"\r" {
        return None;
    }
    let name_end = rest.iter().position(|&b| b == b':').unwrap_or(rest.len());
    let bad = rest[..name_end].iter().position(|&b| !is_token_byte(b))?;
    Some((start + bad + 1, format!("header name {:?} is not a token", String::from_utf8_lossy(&rest[..name_end]))))
}

fn parse_field(line: &[u8]) -> Result<Header, String> {
    let shown = String::from_utf8_lossy(line);
    let Some(colon) = line.iter().position(|&b| b == b':') else {
        return Err(format!("header line {shown:?} has no colon"));
    };
    let (name, value) = (&line[..colon], &line[colon + 1..]);
    if name.is_empty() || !name.iter().all(|&b| is_token_byte(b)) {
        return Err(format!("header name {:?} is not a token", String::from_utf8_lossy(name)));
    }
    // RFC 9110 section 5.5: field values are visible characters, spaces and tabs.
    if let Some(&bad) = value.iter().find(|&&b| !(b == b' ' || b == b'\t' || (0x21..=0x7e).contains(&b) || b >= 0x80)) {
        return Err(format!(
            "header {:?} has the control byte 0x{bad:02x} in its value",
            String::from_utf8_lossy(name)
        ));
    }
    let value = String::from_utf8_lossy(value).trim_matches([' ', '\t']).to_string();
    if name.eq_ignore_ascii_case(b"content-type") {
        parse_media_type(value.as_bytes())?;
    }
    Ok(Header { name: String::from_utf8_lossy(name).into_owned(), value })
}

fn parse_header_block(block: &[u8]) -> Result<(Vec<Header>, usize), String> {
    let mut headers = Vec::new();
    let mut length = None;
    // `block` excludes the final CRLF CRLF, so every line but the last ends with CR.
    let lines: Vec<&[u8]> = block.split(|&b| b == b'\n').collect();
    for (i, raw) in lines.iter().enumerate() {
        let line = if i + 1 < lines.len() {
            raw.strip_suffix(b"\r")
                .ok_or_else(|| format!("header line {:?} ends with a bare LF", String::from_utf8_lossy(raw)))?
        } else {
            raw
        };
        if line.contains(&b'\r') {
            return Err(format!("header line {:?} contains a bare CR", String::from_utf8_lossy(line)));
        }
        let header = parse_field(line)?;
        if header.name.eq_ignore_ascii_case("content-length") {
            if length.is_some() {
                return Err("Content-Length appears twice".into());
            }
            let v = &header.value;
            if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!("Content-Length {v:?} is not a decimal number"));
            }
            length = Some(v.parse::<usize>().map_err(|_| format!("Content-Length {v} is too large"))?);
        }
        headers.push(header);
    }
    match length {
        Some(len) => Ok((headers, len)),
        None => Err("the header section has no Content-Length".into()),
    }
}

fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(chunks: &[Chunk]) -> Vec<&[u8]> {
        chunks
            .iter()
            .filter_map(|c| match c {
                Chunk::Frame { body, .. } => Some(body.as_slice()),
                _ => None,
            })
            .collect()
    }

    fn read_all(stream: &[u8]) -> Vec<Chunk> {
        let mut r = FrameReader::default();
        let mut out = r.feed(stream);
        out.extend(r.finish());
        out
    }

    #[test]
    fn reads_frames_split_at_every_byte_boundary() {
        let mut stream = encode(br#"{"a":1}"#);
        stream
            .extend(encode_with_type("{\"b\":\"\u{1F600}é\"}".as_bytes(), "application/vscode-jsonrpc; charset=utf-8"));
        stream.extend(b"X-Trace: 1\r\nContent-Length: 2\r\n\r\n[]");
        for split in 0..=stream.len() {
            let mut r = FrameReader::default();
            let mut out = r.feed(&stream[..split]);
            out.extend(r.feed(&stream[split..]));
            out.extend(r.finish());
            assert_eq!(out.len(), 3, "split at {split}: {out:?}");
            assert_eq!(
                frames(&out),
                vec![br#"{"a":1}"#.as_slice(), "{\"b\":\"\u{1F600}é\"}".as_bytes(), b"[]".as_slice()]
            );
        }
    }

    #[test]
    fn reads_one_byte_at_a_time() {
        let stream = [encode(b"{}"), encode(b"[]")].concat();
        let mut r = FrameReader::default();
        let mut out = Vec::new();
        for b in &stream {
            out.extend(r.feed(std::slice::from_ref(b)));
        }
        out.extend(r.finish());
        assert_eq!(frames(&out), vec![b"{}".as_slice(), b"[]".as_slice()]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn extension_headers_are_accepted_in_any_order() {
        for stream in
            [&b"X-Trace: abc\r\nContent-Length: 2\r\n\r\n{}"[..], b"Content-Length: 2\r\nX-Trace: abc\r\n\r\n{}"]
        {
            let out = read_all(stream);
            assert_eq!(frames(&out), vec![b"{}".as_slice()], "{out:?}");
            assert_eq!(out.len(), 1);
        }
    }

    #[test]
    fn reports_bytes_outside_frames_and_resynchronizes() {
        let stream = [b"starting server\n".to_vec(), encode(b"{}"), b"oops".to_vec(), encode(b"[]")].concat();
        let cl = |n: &str| vec![Header { name: "Content-Length".into(), value: n.into() }];
        assert_eq!(
            read_all(&stream),
            vec![
                Chunk::Garbage(b"starting server\n".to_vec()),
                Chunk::Frame { headers: cl("2"), body: b"{}".to_vec() },
                Chunk::Garbage(b"oops".to_vec()),
                Chunk::Frame { headers: cl("2"), body: b"[]".to_vec() },
            ]
        );
    }

    #[test]
    fn a_log_line_is_reported_before_the_next_frame_arrives() {
        let mut r = FrameReader::default();
        assert_eq!(r.feed(b"server ready\nConte"), vec![Chunk::Garbage(b"server ready\n".to_vec())]);
        let out = r.feed(b"nt-Length: 2\r\n\r\n{}");
        assert_eq!(frames(&out), vec![b"{}".as_slice()]);
    }

    #[test]
    fn rejects_bad_header_sections() {
        for (block, reason) in [
            ("Content-Type: a/b\r\n\r\n", "no Content-Length"),
            ("Content-Length: 1x\r\n\r\n", "not a decimal number"),
            ("Content-Length: -1\r\n\r\n", "not a decimal number"),
            ("Content-Length: 1\r\nContent-Length: 1\r\n\r\n", "appears twice"),
            ("Content-Length: 1\r\nContent-Type: a/\u{0}b\r\n\r\n", "control byte 0x00"),
            ("Content-Length: 1\r\nContent-Type: a/b\u{7f}\r\n\r\n", "control byte 0x7f"),
        ] {
            match read_all(block.as_bytes()).as_slice() {
                [Chunk::BadHeader { reason: r, .. }, ..] => {
                    assert!(r.contains(reason), "{block:?}: {r}")
                }
                other => panic!("{block:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn an_invalid_section_before_a_valid_one_is_reported_not_merged() {
        let out = read_all(b"X Y: z\r\nContent-Length: 2\r\n\r\n{}");
        assert_eq!(out[0], Chunk::Garbage(b"X Y: z\r\n".to_vec()));
        assert_eq!(frames(&out), vec![b"{}".as_slice()]);
    }

    #[test]
    fn oversized_bodies_are_skipped_without_losing_sync() {
        let mut r = FrameReader::new(1024, 4);
        let stream = [encode(b"[1,2,3]"), encode(b"{}")].concat();
        let mut out = Vec::new();
        for b in &stream {
            out.extend(r.feed(std::slice::from_ref(b)));
        }
        out.extend(r.finish());
        assert!(matches!(out[0], Chunk::Oversized { content_length: 7, limit: 4, .. }), "{out:?}");
        assert_eq!(frames(&out), vec![b"{}".as_slice()]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn header_names_are_case_insensitive_and_values_trimmed() {
        let out = read_all(b"content-length:  2 \r\n\r\n{}");
        assert_eq!(frames(&out), vec![b"{}".as_slice()]);
    }

    #[test]
    fn reports_a_frame_cut_off_by_the_end_of_the_stream() {
        let mut r = FrameReader::default();
        assert!(r.feed(b"Content-Length: 10\r\n\r\n{\"a\"").is_empty());
        assert!(matches!(r.finish().as_slice(), [Chunk::Truncated { content_length: 10, received: 4, .. }]));
        let mut r = FrameReader::default();
        assert!(r.feed(b"Content-Length: 10\r\n").is_empty());
        assert!(matches!(r.finish().as_slice(), [Chunk::BadHeader { .. }]));
    }

    #[test]
    fn a_header_past_the_inspection_limit_is_a_limit_wherever_the_bytes_split() {
        for first in [&b"Content-Length: 2\r\nX-Pad: "[..], b"X-Pad: "] {
            let mut stream = first.to_vec();
            stream.extend(std::iter::repeat_n(b'x', 100));
            if first.starts_with(b"X") {
                stream.extend(b"\r\nContent-Length: 2");
            }
            stream.extend(b"\r\n\r\n{}");
            for split in 0..=stream.len() {
                let mut r = FrameReader::new(64, 1024);
                let mut out = r.feed(&stream[..split]);
                out.extend(r.feed(&stream[split..]));
                out.extend(r.finish());
                assert_eq!(out, vec![Chunk::HeaderLimit { limit: 64 }], "split at {split}");
            }
            // Within the limit, the same section is a frame.
            let out = read_all(&stream);
            assert_eq!(frames(&out), vec![b"{}".as_slice()]);
        }
    }

    #[test]
    fn content_type_values_must_be_media_types() {
        for (value, ok) in [
            ("application/vscode-jsonrpc; charset=utf-8", true),
            ("application/vscode-jsonrpc;charset=\"utf-8\"", true),
            ("application/vscode-jsonrpc ; charset=utf8 ; x=\"a\\\"b\"", true),
            ("application/vscode-jsonrpc;", true),
            ("not a media type; charset=\"utf-8", false),
            ("application/vscode-jsonrpc; charset=\"utf-8", false),
            ("application", false),
            ("application/vscode-jsonrpc; charset", false),
            ("application/vscode-jsonrpc; charset=utf-8; charset=utf-8", false),
        ] {
            assert_eq!(parse_media_type(value.as_bytes()).is_ok(), ok, "{value}");
            let stream = format!("Content-Length: 2\r\nContent-Type: {value}\r\n\r\n{{}}");
            let out = read_all(stream.as_bytes());
            assert_eq!(frames(&out).len() == 1 && out.len() == 1, ok, "{value}: {out:?}");
        }
    }

    #[test]
    fn charset_parameters() {
        assert_eq!(charset("application/vscode-jsonrpc; charset=utf-8").as_deref(), Some("utf-8"));
        assert_eq!(charset("application/vscode-jsonrpc;Charset=\"UTF8\"").as_deref(), Some("utf8"));
        assert_eq!(charset("not a media type; charset=\"utf-8"), None);
        assert_eq!(charset("application/vscode-jsonrpc"), None);
    }
}
