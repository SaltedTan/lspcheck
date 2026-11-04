// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! The LSP base protocol's framing, read from raw bytes.
//!
//! [`FrameReader`] never trusts the server: it reports bytes outside any frame,
//! header blocks it can't parse, and a frame cut off by the end of the stream,
//! and then resynchronizes on the next header so later frames are still seen.

/// The longest header block accepted, in bytes.
pub const DEFAULT_MAX_HEADER_BYTES: usize = 8 * 1024;
/// The largest body accepted, in bytes.
pub const DEFAULT_MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// One header field as written.
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
    /// A header block that isn't valid. The reader skips past it.
    BadHeader { block: Vec<u8>, reason: String },
    /// The stream ended inside a frame: the header promised more body bytes.
    Truncated {
        headers: Vec<Header>,
        content_length: usize,
        body: Vec<u8>,
    },
}

const HEADER_STARTS: [&[u8]; 2] = [b"content-length:", b"content-type:"];

/// An incremental frame parser. Feed it bytes as they arrive.
#[derive(Debug)]
pub struct FrameReader {
    buf: Vec<u8>,
    max_header: usize,
    max_body: usize,
    pending: Option<(Vec<Header>, usize)>,
}

impl Default for FrameReader {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_HEADER_BYTES, DEFAULT_MAX_BODY_BYTES)
    }
}

impl FrameReader {
    pub fn new(max_header: usize, max_body: usize) -> Self {
        Self {
            buf: Vec::new(),
            max_header,
            max_body,
            pending: None,
        }
    }

    /// Adds bytes and returns every chunk they complete.
    pub fn feed(&mut self, data: &[u8]) -> Vec<Chunk> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        loop {
            if let Some((_, len)) = &self.pending {
                let len = *len;
                if self.buf.len() < len {
                    break;
                }
                let (headers, _) = self.pending.take().expect("pending frame");
                let body: Vec<u8> = self.buf.drain(..len).collect();
                out.push(Chunk::Frame { headers, body });
                continue;
            }
            let Some(start) = find_header_start(&self.buf) else {
                // Keep a tail that could still become the start of a header.
                let keep = partial_header_start(&self.buf);
                let cut = self.buf.len() - keep;
                if cut > 0 {
                    out.push(Chunk::Garbage(self.buf.drain(..cut).collect()));
                }
                break;
            };
            if start > 0 {
                out.push(Chunk::Garbage(self.buf.drain(..start).collect()));
            }
            let Some(end) = find(&self.buf, b"\r\n\r\n") else {
                if self.buf.len() > self.max_header {
                    let block = std::mem::take(&mut self.buf);
                    out.push(Chunk::BadHeader {
                        block,
                        reason: format!("no blank line within {} header bytes", self.max_header),
                    });
                    continue;
                }
                break;
            };
            let block: Vec<u8> = self.buf.drain(..end + 4).collect();
            if end + 4 > self.max_header {
                out.push(Chunk::BadHeader {
                    block,
                    reason: format!("header block is longer than {} bytes", self.max_header),
                });
                continue;
            }
            match parse_header_block(&block[..end]) {
                Ok((_, len)) if len > self.max_body => out.push(Chunk::BadHeader {
                    block: block.clone(),
                    reason: format!(
                        "Content-Length {len} is larger than the {} byte limit",
                        self.max_body
                    ),
                }),
                Ok((headers, len)) => self.pending = Some((headers, len)),
                Err(reason) => out.push(Chunk::BadHeader { block, reason }),
            }
        }
        out
    }

    /// Ends the stream and returns whatever was left unfinished.
    pub fn finish(&mut self) -> Vec<Chunk> {
        let mut out = Vec::new();
        let rest = std::mem::take(&mut self.buf);
        if let Some((headers, content_length)) = self.pending.take() {
            out.push(Chunk::Truncated {
                headers,
                content_length,
                body: rest,
            });
        } else if !rest.is_empty() {
            if find_header_start(&rest) == Some(0) {
                out.push(Chunk::BadHeader {
                    block: rest,
                    reason: "the stream ended inside a header block".into(),
                });
            } else {
                out.push(Chunk::Garbage(rest));
            }
        }
        out
    }
}

/// Frames a body with a `Content-Length` header only.
pub fn encode(body: &[u8]) -> Vec<u8> {
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body);
    out
}

/// Frames a body with a `Content-Length` and a `Content-Type` header.
pub fn encode_with_type(body: &[u8], content_type: &str) -> Vec<u8> {
    let mut out = format!(
        "Content-Length: {}\r\nContent-Type: {content_type}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn starts_with_ignore_case(data: &[u8], prefix: &[u8]) -> bool {
    data.len() >= prefix.len() && data[..prefix.len()].eq_ignore_ascii_case(prefix)
}

fn find_header_start(buf: &[u8]) -> Option<usize> {
    (0..buf.len()).find(|&i| {
        HEADER_STARTS
            .iter()
            .any(|h| starts_with_ignore_case(&buf[i..], h))
    })
}

/// The length of the longest suffix of `buf` that is a proper prefix of a header name.
fn partial_header_start(buf: &[u8]) -> usize {
    let longest = HEADER_STARTS.iter().map(|h| h.len()).max().unwrap_or(0);
    (1..longest.min(buf.len() + 1))
        .rev()
        .find(|&n| {
            let tail = &buf[buf.len() - n..];
            HEADER_STARTS
                .iter()
                .any(|h| h.len() > n && h[..n].eq_ignore_ascii_case(tail))
        })
        .unwrap_or(0)
}

fn parse_header_block(block: &[u8]) -> Result<(Vec<Header>, usize), String> {
    let text = std::str::from_utf8(block).map_err(|_| "header block is not ASCII".to_string())?;
    if !text.is_ascii() {
        return Err("header block is not ASCII".into());
    }
    let mut headers = Vec::new();
    let mut length = None;
    for line in text.split("\r\n") {
        if line.contains('\n') || line.contains('\r') {
            return Err(format!("header line {line:?} contains a bare CR or LF"));
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(format!("header line {line:?} has no colon"));
        };
        if name.is_empty() || !name.bytes().all(is_token_byte) {
            return Err(format!("header name {name:?} is not a token"));
        }
        let value = value.trim_matches([' ', '\t']);
        if name.eq_ignore_ascii_case("content-length") {
            if length.is_some() {
                return Err("Content-Length appears twice".into());
            }
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!("Content-Length {value:?} is not a decimal number"));
            }
            length = Some(
                value
                    .parse::<usize>()
                    .map_err(|_| format!("Content-Length {value} is too large"))?,
            );
        }
        headers.push(Header {
            name: name.to_string(),
            value: value.to_string(),
        });
    }
    match length {
        Some(len) => Ok((headers, len)),
        None => Err("the header block has no Content-Length".into()),
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

    #[test]
    fn reads_frames_split_at_every_byte_boundary() {
        let mut stream = encode(br#"{"a":1}"#);
        stream.extend(encode_with_type(
            "{\"b\":\"\u{1F600}é\"}".as_bytes(),
            "application/vscode-jsonrpc; charset=utf-8",
        ));
        for split in 0..=stream.len() {
            let mut r = FrameReader::default();
            let mut out = r.feed(&stream[..split]);
            out.extend(r.feed(&stream[split..]));
            out.extend(r.finish());
            assert_eq!(out.len(), 2, "split at {split}: {out:?}");
            assert_eq!(
                frames(&out),
                vec![
                    br#"{"a":1}"#.as_slice(),
                    "{\"b\":\"\u{1F600}é\"}".as_bytes()
                ]
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
    fn reports_bytes_outside_frames_and_resynchronizes() {
        let stream = [
            b"starting server\n".to_vec(),
            encode(b"{}"),
            b"oops".to_vec(),
            encode(b"[]"),
        ]
        .concat();
        let mut r = FrameReader::default();
        let mut out = r.feed(&stream);
        out.extend(r.finish());
        assert_eq!(
            out,
            vec![
                Chunk::Garbage(b"starting server\n".to_vec()),
                Chunk::Frame {
                    headers: vec![Header {
                        name: "Content-Length".into(),
                        value: "2".into()
                    }],
                    body: b"{}".to_vec()
                },
                Chunk::Garbage(b"oops".to_vec()),
                Chunk::Frame {
                    headers: vec![Header {
                        name: "Content-Length".into(),
                        value: "2".into()
                    }],
                    body: b"[]".to_vec()
                },
            ]
        );
    }

    #[test]
    fn keeps_a_partial_header_name_until_more_arrives() {
        let mut r = FrameReader::default();
        assert_eq!(
            r.feed(b"log\nConte"),
            vec![Chunk::Garbage(b"log\n".to_vec())]
        );
        let out = r.feed(b"nt-Length: 2\r\n\r\n{}");
        assert_eq!(frames(&out), vec![b"{}".as_slice()]);
    }

    #[test]
    fn rejects_bad_header_blocks() {
        for (block, reason) in [
            ("Content-Type: x\r\n\r\n", "no Content-Length"),
            ("Content-Length: 1x\r\n\r\n", "not a decimal number"),
            ("Content-Length: -1\r\n\r\n", "not a decimal number"),
            (
                "Content-Length: 1\r\nContent-Length: 1\r\n\r\n",
                "appears twice",
            ),
            ("Content-Length: 1\r\nX Y: z\r\n\r\n", "not a token"),
            ("Content-Length: 1\r\nbroken\r\n\r\n", "no colon"),
            ("Content-Length: 99999999999\r\n\r\n", "larger than"),
        ] {
            let mut r = FrameReader::default();
            let out = r.feed(block.as_bytes());
            match out.as_slice() {
                [Chunk::BadHeader { reason: r, .. }] => {
                    assert!(r.contains(reason), "{block:?}: {r}")
                }
                other => panic!("{block:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn header_names_are_case_insensitive() {
        let mut r = FrameReader::default();
        let out = r.feed(b"content-length:  2 \r\n\r\n{}");
        assert_eq!(frames(&out), vec![b"{}".as_slice()]);
    }

    #[test]
    fn reports_a_frame_cut_off_by_the_end_of_the_stream() {
        let mut r = FrameReader::default();
        assert!(r.feed(b"Content-Length: 10\r\n\r\n{\"a\"").is_empty());
        match r.finish().as_slice() {
            [
                Chunk::Truncated {
                    content_length: 10,
                    body,
                    ..
                },
            ] => assert_eq!(body, b"{\"a\""),
            other => panic!("{other:?}"),
        }
        let mut r = FrameReader::default();
        assert!(r.feed(b"Content-Length: 10\r\n").is_empty());
        assert!(matches!(r.finish().as_slice(), [Chunk::BadHeader { .. }]));
    }

    #[test]
    fn a_long_header_without_a_blank_line_is_rejected() {
        let mut r = FrameReader::new(64, 1024);
        let mut data = b"Content-Length: 2\r\n".to_vec();
        data.extend(std::iter::repeat_n(b'x', 100));
        assert!(matches!(
            r.feed(&data).as_slice(),
            [Chunk::BadHeader { .. }]
        ));
    }
}
