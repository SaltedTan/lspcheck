// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! The text model behind the document checks: the spec's line split, the
//! three position encodings, the test of whether an offset is at a character
//! boundary, the application of content changes, and the table that finds
//! the positions a reply refers to.
//!
//! Pure functions only: no I/O, no async, no session. A wrong answer here is
//! a wrong verdict, so every function is unit tested against the
//! specification's own examples.

use serde_json::Value;

/// The negotiated position encoding: the server's `positionEncoding`, or
/// UTF-16 when it omitted the capability, as the spec defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// UTF-8 code units: bytes.
    Utf8,
    /// UTF-16 code units: a surrogate pair counts as two.
    Utf16,
    /// UTF-32 code units: Unicode code points. No offset can fall inside a
    /// character in this encoding.
    Utf32,
}

impl Encoding {
    /// The encoding a server chose, from its `positionEncoding` capability.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "utf-8" => Some(Self::Utf8),
            "utf-16" => Some(Self::Utf16),
            "utf-32" => Some(Self::Utf32),
            _ => None,
        }
    }
}

/// Splits a document into lines at the spec's ends of line, `\n`, `\r\n` and
/// `\r`, keeping each line's text without its terminator. An empty document
/// is one empty line; a final terminator leaves a final empty line.
pub fn split_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\n' || bytes[i] == b'\r' {
            lines.push(&text[start..i]);
            i += 1;
            if bytes[i - 1] == b'\r' && i < bytes.len() && bytes[i] == b'\n' {
                i += 1;
            }
            start = i;
        } else {
            i += 1;
        }
    }
    lines.push(&text[start..]);
    lines
}

/// The length of a line in the given encoding: bytes for UTF-8, UTF-16 code
/// units for UTF-16, code points for UTF-32.
pub fn line_length(line: &str, encoding: Encoding) -> usize {
    match encoding {
        Encoding::Utf8 => line.len(),
        Encoding::Utf16 => line.encode_utf16().count(),
        Encoding::Utf32 => line.chars().count(),
    }
}

/// Why an offset can't be converted to a byte offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OffsetError {
    /// The offset falls inside a character in the encoding: between the
    /// halves of a UTF-16 surrogate pair, or inside a multi-byte UTF-8
    /// sequence.
    InsideCharacter,
    /// The offset is greater than the line's length in the encoding.
    PastEnd,
}

/// Converts a character offset in `encoding` to the byte offset of the same
/// place in the line. Only offsets at a character boundary convert: one
/// inside a character is [`OffsetError::InsideCharacter`], one past the
/// line's length in the encoding is [`OffsetError::PastEnd`].
pub fn offset_to_bytes(line: &str, encoding: Encoding, offset: usize) -> Result<usize, OffsetError> {
    let mut bytes = 0;
    let mut units = 0;
    for c in line.chars() {
        if units == offset {
            return Ok(bytes);
        }
        units += match encoding {
            Encoding::Utf8 => c.len_utf8(),
            Encoding::Utf16 => c.len_utf16(),
            Encoding::Utf32 => 1,
        };
        if units > offset {
            return Err(OffsetError::InsideCharacter);
        }
        bytes += c.len_utf8();
    }
    if offset == units { Ok(bytes) } else { Err(OffsetError::PastEnd) }
}

/// Where a position falls in a document, in the given encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionStatus {
    /// At a character boundary of an existing line. Splitting a grapheme
    /// cluster (a combining mark, a ZWJ sequence) is a boundary: offsets
    /// count code units, not user-perceived characters.
    Boundary,
    /// Inside a character: between the halves of a UTF-16 surrogate pair, or
    /// inside a multi-byte UTF-8 sequence.
    InsideCharacter,
    /// Past the end of an existing line. The spec lets clients clamp such a
    /// position, so it is not a rule.
    PastEndOfLine,
    /// A line the document doesn't have.
    NoLine,
}

/// Where `(line, character)` falls in `text`, counted in `encoding`.
pub fn position_status(text: &str, encoding: Encoding, line: usize, character: usize) -> PositionStatus {
    match split_lines(text).get(line) {
        None => PositionStatus::NoLine,
        Some(line) => match offset_to_bytes(line, encoding, character) {
            Ok(_) => PositionStatus::Boundary,
            Err(OffsetError::InsideCharacter) => PositionStatus::InsideCharacter,
            Err(OffsetError::PastEnd) => PositionStatus::PastEndOfLine,
        },
    }
}

/// A position in a document: a line and a character offset in the
/// negotiated encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub line: usize,
    pub character: usize,
}

/// A range of a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

/// One `TextDocumentContentChangeEvent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentChange {
    /// The whole text is replaced; the encoding is irrelevant.
    Full(String),
    /// `range` is replaced by `text`, both counted in the given encoding.
    Text { range: Range, text: String },
}

/// Why a content change can't be applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeError {
    /// A line of the range the document doesn't have.
    NoLine(usize),
    /// A character of the range inside a character in the encoding.
    InsideCharacter,
    /// The range's end precedes its start.
    Inverted,
}

/// The byte offset of a position in the whole text: the line's start plus
/// the character's offset in the line, clamping a character past the end of
/// its line to the line's length: LSP 3.17 says a character greater than
/// the line length defaults back to the line length. Only applying a change
/// clamps; an inspected position past the end of its line is reported as
/// such.
fn position_byte_clamped(text: &str, encoding: Encoding, p: Position) -> Result<usize, ChangeError> {
    let mut start = 0;
    for (line, l) in split_lines(text).into_iter().enumerate() {
        if line == p.line {
            let character = p.character.min(line_length(l, encoding));
            let off = offset_to_bytes(l, encoding, character).map_err(|_| ChangeError::InsideCharacter)?;
            return Ok(start + off);
        }
        start += l.len();
        // The terminator: `\r\n` is two bytes, `\n` and `\r` one.
        let rest = &text[start..];
        start += if rest.starts_with("\r\n") { 2 } else { 1 };
    }
    Err(ChangeError::NoLine(p.line))
}

/// Applies one content change to a text, with the range counted in
/// `encoding`. A character past the end of its line is clamped to the
/// line's length, as the spec lets clients do. A change that still doesn't
/// fit is an error, never a panic: a line the document doesn't have, a
/// character inside a character, or an end before the start.
pub fn apply_change(text: &str, encoding: Encoding, change: &ContentChange) -> Result<String, ChangeError> {
    match change {
        ContentChange::Full(new) => Ok(new.clone()),
        ContentChange::Text { range, text: insert } => {
            if (range.end.line, range.end.character) < (range.start.line, range.start.character) {
                return Err(ChangeError::Inverted);
            }
            let start_byte = position_byte_clamped(text, encoding, range.start)?;
            let end_byte = position_byte_clamped(text, encoding, range.end)?;
            let mut out = String::with_capacity(text.len() + insert.len());
            out.push_str(&text[..start_byte]);
            out.push_str(insert);
            out.push_str(&text[end_byte..]);
            Ok(out)
        }
    }
}

/// What [`positions`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoundKind {
    /// A `Position`.
    Position,
    /// A `Range`; both ends count.
    Range,
    /// A line number alone, such as a `FoldingRange`'s: not judged in v0.1.
    Line,
}

/// One position found in a result or a notification's params.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The document URI the position refers to.
    pub uri: String,
    /// A short path describing where it was found, for failure details.
    pub pointer: String,
    pub kind: FoundKind,
}

/// The positions a reply (or `textDocument/publishDiagnostics`'s params)
/// refers to, by hand, one method at a time.
///
/// Only the paths in the design's table are walked. `data` fields, command
/// arguments and other free-form values are never descended into, and a
/// shape that doesn't match is skipped silently: malformed values are K2's
/// finding, not a position verdict. `result` is the request's result, or
/// `None` for a notification, whose params carry the positions.
pub fn positions(method: &str, params: &Value, result: Option<&Value>) -> Vec<Found> {
    let mut out = Vec::new();
    let request_doc = params.get("textDocument").and_then(|t| t.get("uri")).and_then(|v| as_str(Some(v)));
    match method {
        "textDocument/publishDiagnostics" => {
            if let (Some(uri), Some(diag)) =
                (params.get("uri").and_then(|v| as_str(Some(v))), params.get("diagnostics"))
            {
                diagnostics(&mut out, uri, "diagnostics", diag);
            }
        }
        "textDocument/diagnostic" => {
            if let (Some(uri), Some(report)) = (request_doc, result) {
                pull_report(&mut out, uri, "", report);
            }
        }
        "textDocument/documentSymbol" => {
            if let (Some(uri), Some(syms)) = (request_doc, result) {
                symbols(&mut out, uri, "", syms);
            }
        }
        "textDocument/foldingRange" => {
            if let (Some(uri), Some(arr)) = (request_doc, result.and_then(|r| r.as_array())) {
                for (i, fr) in arr.iter().enumerate() {
                    let p = format!("[{i}]");
                    for name in ["startLine", "endLine"] {
                        if fr.get(name).and_then(|v| as_u64(Some(v))).is_some() {
                            out.push(Found {
                                uri: uri.to_string(),
                                pointer: format!("{p}.{name}"),
                                kind: FoundKind::Line,
                            });
                        }
                    }
                    // A `FoldingRange` with characters is a position on its
                    // line; without them it is a line alone.
                    for (line_name, char_name) in [("startLine", "startCharacter"), ("endLine", "endCharacter")] {
                        if fr.get(line_name).and_then(|v| as_u64(Some(v))).is_some()
                            && fr.get(char_name).and_then(|v| as_u64(Some(v))).is_some()
                        {
                            out.push(Found {
                                uri: uri.to_string(),
                                pointer: format!("{p}.{char_name}"),
                                kind: FoundKind::Position,
                            });
                        }
                    }
                }
            }
        }
        "textDocument/selectionRange" => {
            if let (Some(uri), Some(arr)) = (request_doc, result) {
                selection_ranges(&mut out, uri, "", arr);
            }
        }
        "textDocument/documentHighlight"
        | "textDocument/documentLink"
        | "textDocument/codeLens"
        | "textDocument/documentColor"
        | "textDocument/formatting"
        | "textDocument/rangeFormatting" => {
            if let (Some(uri), Some(arr)) = (request_doc, result.and_then(|r| r.as_array())) {
                for (i, v) in arr.iter().enumerate() {
                    if as_range(v.get("range")).is_some() {
                        push_range(&mut out, uri, &format!("[{i}].range"));
                    }
                }
            }
        }
        "textDocument/hover" => {
            if let (Some(uri), Some(r)) = (request_doc, result)
                && as_range(r.get("range")).is_some()
            {
                push_range(&mut out, uri, "range");
            }
        }
        "textDocument/inlayHint" => {
            if let (Some(uri), Some(arr)) = (request_doc, result.and_then(|r| r.as_array())) {
                for (i, h) in arr.iter().enumerate() {
                    let p = format!("[{i}]");
                    if as_pos(h.get("position")).is_some() {
                        out.push(Found {
                            uri: uri.to_string(),
                            pointer: format!("{p}.position"),
                            kind: FoundKind::Position,
                        });
                    }
                    for (j, te) in h.get("textEdits").and_then(|v| v.as_array()).into_iter().flatten().enumerate() {
                        if as_range(te.get("range")).is_some() {
                            push_range(&mut out, uri, &format!("{p}.textEdits[{j}].range"));
                        }
                    }
                    // `label` is a string or an array of label parts; only
                    // parts with a `location` are walked. A single object is
                    // not a shape the spec defines, so it is not walked.
                    if let Some(parts) = h.get("label").and_then(|v| v.as_array()) {
                        for (k, part) in parts.iter().enumerate() {
                            let loc = part.get("location");
                            if let Some(luri) = loc.and_then(|l| l.get("uri")).and_then(|v| v.as_str())
                                && as_range(loc.and_then(|l| l.get("range"))).is_some()
                            {
                                push_range(&mut out, luri, &format!("{p}.label[{k}].location"));
                            }
                        }
                    }
                }
            }
        }
        "textDocument/definition"
        | "textDocument/declaration"
        | "textDocument/typeDefinition"
        | "textDocument/implementation"
        | "textDocument/references" => {
            if let (Some(uri), Some(r)) = (request_doc, result) {
                let mut items: Vec<&Value> = Vec::new();
                if let Some(arr) = r.as_array() {
                    items = arr.iter().collect();
                } else {
                    items.push(r);
                }
                for (i, l) in items.iter().enumerate() {
                    let p = format!("[{i}]");
                    if l.get("targetUri").is_some() {
                        // A `LocationLink`: the target ranges name the
                        // target document, the origin range the request's.
                        if let Some(turi) = as_str(l.get("targetUri")) {
                            if as_range(l.get("targetRange")).is_some() {
                                push_range(&mut out, turi, &format!("{p}.targetRange"));
                            }
                            if as_range(l.get("targetSelectionRange")).is_some() {
                                push_range(&mut out, turi, &format!("{p}.targetSelectionRange"));
                            }
                        }
                        if as_range(l.get("originSelectionRange")).is_some() {
                            push_range(&mut out, uri, &format!("{p}.originSelectionRange"));
                        }
                    } else if let (Some(uri), Some(_)) = (as_str(l.get("uri")), as_range(l.get("range"))) {
                        push_range(&mut out, uri, &format!("{p}.range"));
                    }
                }
            }
        }
        "textDocument/linkedEditingRange" => {
            // `LinkedEditingRanges | null`: `{"ranges": [Range, ...],
            // "wordPattern"?: string}`; each range is about the request's
            // document.
            if let (Some(uri), Some(ranges)) =
                (request_doc, result.and_then(|r| r.get("ranges")).and_then(|v| v.as_array()))
            {
                for (i, v) in ranges.iter().enumerate() {
                    if as_range(Some(v)).is_some() {
                        push_range(&mut out, uri, &format!("ranges[{i}]"));
                    }
                }
            }
        }
        "textDocument/prepareRename" => {
            // `Range`, `{range, placeholder}`, `{defaultBehavior}` or
            // `null`; never an array.
            if let (Some(uri), Some(r)) = (request_doc, result) {
                if as_range(Some(r)).is_some() {
                    push_range(&mut out, uri, "");
                } else if as_range(r.get("range")).is_some() {
                    push_range(&mut out, uri, "range");
                }
            }
        }
        // Every other method, and `semanticTokens/full` (decoded from
        // `data`, a later PR): no positions are walked.
        _ => {}
    }
    out
}

/// A `diagnostics` array: each entry's `range` and the `location`s of its
/// `relatedInformation`.
fn diagnostics(out: &mut Vec<Found>, uri: &str, prefix: &str, arr: &Value) {
    for (i, d) in arr.as_array().into_iter().flatten().enumerate() {
        let p = format!("{prefix}[{i}]");
        if as_range(d.get("range")).is_some() {
            push_range(out, uri, &format!("{p}.range"));
        }
        for (j, rel) in d.get("relatedInformation").and_then(|v| v.as_array()).into_iter().flatten().enumerate() {
            // Each entry is `{ location: Location }`.
            let loc = rel.get("location");
            if let Some(luri) = loc.and_then(|l| l.get("uri")).and_then(|v| v.as_str())
                && as_range(loc.and_then(|l| l.get("range"))).is_some()
            {
                push_range(out, luri, &format!("{p}.relatedInformation[{j}].location"));
            }
        }
    }
}

/// A pull-diagnostic report: a `Diagnostic` array, or a
/// `DocumentDiagnosticReport` with `items` and a `relatedDocuments` map whose
/// keys name the documents of the nested reports.
fn pull_report(out: &mut Vec<Found>, uri: &str, prefix: &str, report: &Value) {
    if report.is_array() {
        diagnostics(out, uri, prefix, report);
        return;
    }
    if let Some(items) = report.get("items") {
        diagnostics(out, uri, &format!("{prefix}items"), items);
    }
    if let Some(related) = report.get("relatedDocuments").and_then(|v| v.as_object()) {
        for (uri, sub) in related {
            pull_report(out, uri, &format!("{prefix}relatedDocuments."), sub);
        }
    }
}

/// A `documentSymbol` result: `DocumentSymbol`s (recursively under
/// `children`) and `SymbolInformation`s (by their `location`).
fn symbols(out: &mut Vec<Found>, uri: &str, prefix: &str, arr: &Value) {
    for (i, s) in arr.as_array().into_iter().flatten().enumerate() {
        let p = format!("{prefix}[{i}]");
        if let Some(loc) = s.get("location") {
            // A `SymbolInformation`: its `location` names the document.
            if let Some(luri) = as_str(loc.get("uri"))
                && as_range(loc.get("range")).is_some()
            {
                push_range(out, luri, &format!("{p}.location"));
            }
        } else {
            // A `DocumentSymbol`: it belongs to the requested document.
            if as_range(s.get("range")).is_some() {
                push_range(out, uri, &format!("{p}.range"));
            }
            if as_range(s.get("selectionRange")).is_some() {
                push_range(out, uri, &format!("{p}.selectionRange"));
            }
            if let Some(children) = s.get("children") {
                symbols(out, uri, &format!("{p}.children"), children);
            }
        }
    }
}

/// A `selectionRange` result: an array at the top level, each entry's
/// `parent` a single object, recursively.
fn selection_ranges(out: &mut Vec<Found>, uri: &str, prefix: &str, value: &Value) {
    let items: Vec<(&Value, String)> = match value.as_array() {
        Some(arr) => arr.iter().enumerate().map(|(i, s)| (s, format!("[{i}]"))).collect(),
        None => vec![(value, String::new())],
    };
    for (s, idx) in items {
        let p = format!("{prefix}{idx}");
        if as_range(s.get("range")).is_some() {
            push_range(out, uri, &format!("{p}.range"));
        }
        if let Some(parent) = s.get("parent") {
            selection_ranges(out, uri, &format!("{p}.parent"), parent);
        }
    }
}

fn push_range(out: &mut Vec<Found>, uri: &str, pointer: &str) {
    out.push(Found { uri: uri.to_string(), pointer: pointer.to_string(), kind: FoundKind::Range });
}

fn as_u64(v: Option<&Value>) -> Option<usize> {
    v.and_then(|v| v.as_u64()).map(|n| n as usize)
}

fn as_str(v: Option<&Value>) -> Option<&str> {
    v.and_then(|v| v.as_str())
}

/// A `Position`: an object with non-negative integer `line` and `character`.
fn as_pos(v: Option<&Value>) -> Option<(usize, usize)> {
    let v = v?;
    Some((as_u64(v.get("line"))?, as_u64(v.get("character"))?))
}

/// A `Range`: an object with `start` and `end` positions.
fn as_range(v: Option<&Value>) -> Option<((usize, usize), (usize, usize))> {
    let v = v?;
    Some((as_pos(v.get("start"))?, as_pos(v.get("end"))?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first line of the `documents` scenario's document A: six emoji, a
    /// word, six emoji, ending in a 4-byte character.
    const L0: &str = "\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600} word \u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}";
    /// Document B of the same scenario: a lone CR, a CR LF, then LF lines.
    const DOC_B: &str = "x\r\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\r\n word\n\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\n name\n";

    /// The word's boundaries in each counting: code points, UTF-16 units and
    /// UTF-8 bytes.
    const WORD: [(usize, usize); 3] = [(7, 11), (13, 17), (25, 29)];

    #[test]
    fn the_spec_example_puts_b_at_three() {
        // The spec's pre-3.17 example: in `a𐐀b`, `b` is at 3 in UTF-16.
        let line = "a\u{10400}b";
        assert_eq!(line_length(line, Encoding::Utf16), 4);
        assert_eq!(offset_to_bytes(line, Encoding::Utf16, 3), Ok(5));
        assert_eq!(offset_to_bytes(line, Encoding::Utf8, 5), Ok(5));
        assert_eq!(line_length(line, Encoding::Utf8), 6);
        assert_eq!(line_length(line, Encoding::Utf32), 3);
    }

    #[test]
    fn lines_split_at_the_spec_ends_of_line() {
        assert_eq!(split_lines(""), [""]);
        assert_eq!(split_lines("abc"), ["abc"]);
        assert_eq!(split_lines("a\nb"), ["a", "b"]);
        assert_eq!(split_lines("a\r\nb"), ["a", "b"]);
        assert_eq!(split_lines("a\rb"), ["a", "b"]);
        assert_eq!(split_lines("a\nb\rc\r\nd"), ["a", "b", "c", "d"]);
        // A final terminator leaves a final empty line; empty lines stay.
        assert_eq!(split_lines("a\n"), ["a", ""]);
        assert_eq!(split_lines("\n\n"), ["", "", ""]);
        assert_eq!(split_lines("a\r\n\r\nb\n"), ["a", "", "b", ""]);
    }

    #[test]
    fn line_lengths_in_each_encoding() {
        let line = "café \u{10400}\u{1F600}";
        assert_eq!(line_length(line, Encoding::Utf8), 14);
        assert_eq!(line_length(line, Encoding::Utf16), 9);
        assert_eq!(line_length(line, Encoding::Utf32), 7);
        assert_eq!(line_length("", Encoding::Utf16), 0);
    }

    #[test]
    fn offsets_convert_to_bytes_at_boundaries_only() {
        let line = "a\u{10400}b\u{1F600}";
        // UTF-8: every byte boundary converts; the rest is inside a
        // character, and past the end is past the end.
        for (offset, want) in [(0, 0), (1, 1), (5, 5), (6, 6), (10, 10)] {
            assert_eq!(offset_to_bytes(line, Encoding::Utf8, offset), Ok(want), "{offset}");
        }
        for offset in [2, 3, 4, 7, 8, 9] {
            assert_eq!(offset_to_bytes(line, Encoding::Utf8, offset), Err(OffsetError::InsideCharacter), "{offset}");
        }
        assert_eq!(offset_to_bytes(line, Encoding::Utf8, 11), Err(OffsetError::PastEnd));
        // UTF-16: the surrogate pair is two units; between them is inside a
        // character.
        for (offset, want) in [(0, 0), (1, 1), (3, 5), (4, 6), (6, 10)] {
            assert_eq!(offset_to_bytes(line, Encoding::Utf16, offset), Ok(want), "{offset}");
        }
        for offset in [2, 5] {
            assert_eq!(offset_to_bytes(line, Encoding::Utf16, offset), Err(OffsetError::InsideCharacter), "{offset}");
        }
        assert_eq!(offset_to_bytes(line, Encoding::Utf16, 7), Err(OffsetError::PastEnd));
        // UTF-32: code points; no offset can fall inside a character.
        for (offset, want) in [(0, 0), (1, 1), (2, 5), (3, 6), (4, 10)] {
            assert_eq!(offset_to_bytes(line, Encoding::Utf32, offset), Ok(want), "{offset}");
        }
        assert_eq!(offset_to_bytes(line, Encoding::Utf32, 5), Err(OffsetError::PastEnd));
    }

    #[test]
    fn combining_marks_and_zwj_sequences_split_at_boundaries() {
        // `café`: the combining mark is its own code unit, so the offset
        // between `e` and U+0301 is a boundary in every encoding.
        let line = "caf\u{0301}";
        for enc in [Encoding::Utf8, Encoding::Utf16, Encoding::Utf32] {
            assert_eq!(position_status(line, enc, 0, 3), PositionStatus::Boundary, "{enc:?}");
        }
        // A ZWJ family sequence: every code-point boundary inside it is a
        // boundary, not inside a character; the offsets between a surrogate
        // pair's halves are inside a character.
        let line = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{1F3FE}";
        let mut bounds = vec![0];
        for c in line.chars() {
            bounds.push(bounds.last().expect("a bound") + c.len_utf16());
        }
        for offset in &bounds {
            assert_eq!(position_status(line, Encoding::Utf16, 0, *offset), PositionStatus::Boundary, "{offset}");
        }
        for offset in 1..line_length(line, Encoding::Utf16) {
            if !bounds.contains(&offset) {
                assert_eq!(
                    position_status(line, Encoding::Utf16, 0, offset),
                    PositionStatus::InsideCharacter,
                    "{offset}"
                );
            }
        }
        // A variation selector after an emoji: the offset after the emoji is
        // a boundary.
        let line = "\u{1F600}\u{FE0F}";
        assert_eq!(position_status(line, Encoding::Utf16, 0, 2), PositionStatus::Boundary);
        assert_eq!(position_status(line, Encoding::Utf8, 0, 4), PositionStatus::Boundary);
    }

    #[test]
    fn the_word_boundaries_are_boundaries_in_their_own_encoding() {
        let [cp, u16, u8] = WORD;
        assert_eq!(position_status(L0, Encoding::Utf32, 0, cp.0), PositionStatus::Boundary);
        assert_eq!(position_status(L0, Encoding::Utf32, 0, cp.1), PositionStatus::Boundary);
        assert_eq!(position_status(L0, Encoding::Utf16, 0, u16.0), PositionStatus::Boundary);
        assert_eq!(position_status(L0, Encoding::Utf16, 0, u16.1), PositionStatus::Boundary);
        assert_eq!(position_status(L0, Encoding::Utf8, 0, u8.0), PositionStatus::Boundary);
        assert_eq!(position_status(L0, Encoding::Utf8, 0, u8.1), PositionStatus::Boundary);
    }

    #[test]
    fn the_four_encoding_mixups_land_inside_a_character() {
        // Each of the four common confusions: the word's boundaries counted
        // one way, reported in another encoding. Every one of them falls
        // inside a character of L0.
        let [cp, u16, u8] = WORD;
        // UTF-8 bytes reported under UTF-16.
        assert_eq!(position_status(L0, Encoding::Utf16, 0, u8.0), PositionStatus::InsideCharacter);
        assert_eq!(position_status(L0, Encoding::Utf16, 0, u8.1), PositionStatus::InsideCharacter);
        // Code points reported under UTF-16.
        assert_eq!(position_status(L0, Encoding::Utf16, 0, cp.0), PositionStatus::InsideCharacter);
        assert_eq!(position_status(L0, Encoding::Utf16, 0, cp.1), PositionStatus::InsideCharacter);
        // UTF-16 units reported under UTF-8.
        assert_eq!(position_status(L0, Encoding::Utf8, 0, u16.0), PositionStatus::InsideCharacter);
        assert_eq!(position_status(L0, Encoding::Utf8, 0, u16.1), PositionStatus::InsideCharacter);
        // Code points reported under UTF-8.
        assert_eq!(position_status(L0, Encoding::Utf8, 0, cp.0), PositionStatus::InsideCharacter);
        assert_eq!(position_status(L0, Encoding::Utf8, 0, cp.1), PositionStatus::InsideCharacter);
    }

    #[test]
    fn the_l0_property_holds() {
        // The scenario relies on L0: for each of the four confusions, at
        // least one of the word's boundaries lands inside a character.
        let [cp, u16, u8] = WORD;
        let confusions: [(Encoding, (usize, usize)); 4] = [
            (Encoding::Utf16, (u8.0, u8.1)),
            (Encoding::Utf16, (cp.0, cp.1)),
            (Encoding::Utf8, (u16.0, u16.1)),
            (Encoding::Utf8, (cp.0, cp.1)),
        ];
        for (enc, (start, end)) in confusions {
            assert!(
                matches!(position_status(L0, enc, 0, start), PositionStatus::InsideCharacter)
                    || matches!(position_status(L0, enc, 0, end), PositionStatus::InsideCharacter),
                "{enc:?} {start} {end}"
            );
        }
    }

    #[test]
    fn past_end_and_missing_lines_are_not_boundaries() {
        let text = "ab\ncd";
        assert_eq!(position_status(text, Encoding::Utf16, 1, 2), PositionStatus::Boundary);
        assert_eq!(position_status(text, Encoding::Utf16, 1, 3), PositionStatus::PastEndOfLine);
        assert_eq!(position_status(text, Encoding::Utf16, 2, 0), PositionStatus::NoLine);
        assert_eq!(position_status(text, Encoding::Utf16, 99, 0), PositionStatus::NoLine);
        // A line past the end of a document with a final newline is a real
        // line: the empty one.
        assert_eq!(position_status("ab\n", Encoding::Utf16, 1, 0), PositionStatus::Boundary);
        assert_eq!(position_status("", Encoding::Utf16, 0, 0), PositionStatus::Boundary);
        assert_eq!(position_status("", Encoding::Utf16, 1, 0), PositionStatus::NoLine);
    }

    #[test]
    fn document_b_splits_the_spec_way() {
        let lines = split_lines(DOC_B);
        assert_eq!(lines, ["x", "😀😀😀😀😀😀😀😀", " word", "😀😀😀😀😀😀😀😀", " name", ""]);
        // The word line, in the spec's split, has its odd offsets on
        // boundaries.
        assert_eq!(position_status(DOC_B, Encoding::Utf16, 2, 1), PositionStatus::Boundary);
        assert_eq!(position_status(DOC_B, Encoding::Utf16, 2, 5), PositionStatus::Boundary);
    }

    #[test]
    fn document_b_split_wrongly_puts_the_word_on_an_emoji_line() {
        // Each wrong split moves the word line onto an emoji line, where the
        // odd offsets 1 and 5 fall inside a character:
        // - lone CR not split, or split at `\n` only: the word is line 1;
        // - CR LF counted as two breaks: the word is line 3.
        let emoji = "\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}";
        for offset in [1, 5] {
            assert_eq!(position_status(emoji, Encoding::Utf16, 0, offset), PositionStatus::InsideCharacter);
        }
        // The word line itself keeps them on boundaries.
        for offset in [1, 5] {
            assert_eq!(position_status(" word", Encoding::Utf16, 0, offset), PositionStatus::Boundary);
        }
    }

    #[test]
    fn a_full_change_replaces_the_whole_text() {
        let text = "a\r\nb\rc";
        assert_eq!(apply_change(text, Encoding::Utf16, &ContentChange::Full("new".into())), Ok("new".into()));
    }

    #[test]
    fn a_change_replaces_a_four_byte_character_in_each_encoding() {
        let text = "a\u{1F600}b";
        // The emoji is (0,1)-(0,3) in UTF-16, (0,1)-(0,5) in UTF-8 and
        // (0,1)-(0,2) in UTF-32.
        for (enc, range) in [
            (
                Encoding::Utf16,
                Range { start: Position { line: 0, character: 1 }, end: Position { line: 0, character: 3 } },
            ),
            (
                Encoding::Utf8,
                Range { start: Position { line: 0, character: 1 }, end: Position { line: 0, character: 5 } },
            ),
            (
                Encoding::Utf32,
                Range { start: Position { line: 0, character: 1 }, end: Position { line: 0, character: 2 } },
            ),
        ] {
            let change = ContentChange::Text { range, text: "xy".into() };
            assert_eq!(apply_change(text, enc, &change), Ok("axyb".into()), "{enc:?}");
        }
    }

    #[test]
    fn changes_apply_across_lines_and_at_the_end() {
        let text = "ab\r\ncd\nef";
        // A range spanning the CR LF terminator.
        let change = ContentChange::Text {
            range: Range { start: Position { line: 0, character: 1 }, end: Position { line: 1, character: 1 } },
            text: "X".into(),
        };
        assert_eq!(apply_change(text, Encoding::Utf16, &change), Ok("aXd\nef".into()));
        // An append at the end, at `(lastLine, len)`.
        let change = ContentChange::Text {
            range: Range { start: Position { line: 2, character: 2 }, end: Position { line: 2, character: 2 } },
            text: "!".into(),
        };
        assert_eq!(apply_change(text, Encoding::Utf16, &change), Ok("ab\r\ncd\nef!".into()));
        // An insert at `(lineCount, 0)` after a final newline.
        let text = "ab\n";
        let change = ContentChange::Text {
            range: Range { start: Position { line: 1, character: 0 }, end: Position { line: 1, character: 0 } },
            text: "cd".into(),
        };
        assert_eq!(apply_change(text, Encoding::Utf16, &change), Ok("ab\ncd".into()));
        // The scenario's change: replacing the first emoji of L0 with four
        // emoji, in UTF-16.
        let change = ContentChange::Text {
            range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 2 } },
            text: "\u{1F600}\u{1F600}\u{1F600}\u{1F600}".into(),
        };
        let want = "\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600} word \u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}";
        assert_eq!(apply_change(L0, Encoding::Utf16, &change), Ok(want.to_string()));
    }

    #[test]
    fn a_range_that_does_not_fit_is_an_error_not_a_panic() {
        let text = "ab\ncd";
        let p = |line, character| Position { line, character };
        let cases: [(Range, ChangeError); 3] = [
            (Range { start: p(0, 0), end: p(9, 0) }, ChangeError::NoLine(9)),
            (Range { start: p(9, 0), end: p(9, 0) }, ChangeError::NoLine(9)),
            (Range { start: p(1, 0), end: p(0, 1) }, ChangeError::Inverted),
        ];
        for (range, want) in cases {
            let change = ContentChange::Text { range, text: "x".into() };
            assert_eq!(apply_change(text, Encoding::Utf16, &change), Err(want), "{range:?}");
        }
        // A character inside a character (between the halves of a UTF-16
        // surrogate pair) is an error, not a clamp.
        let text = "a\u{10400}b";
        let range = Range { start: Position { line: 0, character: 2 }, end: Position { line: 0, character: 3 } };
        assert_eq!(
            apply_change(text, Encoding::Utf16, &ContentChange::Text { range, text: "x".into() }),
            Err(ChangeError::InsideCharacter)
        );
        // A range past the end in one encoding can fit in another: the
        // same range is judged in the given encoding only, and a character
        // past the end clamps to the line's length.
        let text = "x\u{00E9}";
        let range = Range { start: Position { line: 0, character: 1 }, end: Position { line: 0, character: 3 } };
        // In UTF-16 the line is two units long, so the end clamps to it;
        // in UTF-8 it is exactly three bytes long. Both delete the `é`.
        assert_eq!(
            apply_change(text, Encoding::Utf16, &ContentChange::Text { range, text: "".into() }),
            Ok("x".into())
        );
        assert_eq!(apply_change(text, Encoding::Utf8, &ContentChange::Text { range, text: "".into() }), Ok("x".into()));
    }

    #[test]
    fn a_change_clamps_a_character_past_the_line_end() {
        // LSP 3.17: a character greater than the line length defaults back
        // to the line length, when applying a change.
        let insert = ContentChange::Text {
            range: Range { start: Position { line: 0, character: 100 }, end: Position { line: 0, character: 100 } },
            text: "X".into(),
        };
        assert_eq!(apply_change("ab", Encoding::Utf16, &insert), Ok("abX".into()));
        let replace = ContentChange::Text {
            range: Range { start: Position { line: 0, character: 1 }, end: Position { line: 0, character: 100 } },
            text: "X".into(),
        };
        assert_eq!(apply_change("ab", Encoding::Utf16, &replace), Ok("aX".into()));
        // A line past the end of the document is still an error, and so is
        // a start after the end.
        let past_line = ContentChange::Text {
            range: Range { start: Position { line: 5, character: 0 }, end: Position { line: 5, character: 0 } },
            text: "X".into(),
        };
        assert_eq!(apply_change("ab", Encoding::Utf16, &past_line), Err(ChangeError::NoLine(5)));
        let inverted = ContentChange::Text {
            range: Range { start: Position { line: 0, character: 100 }, end: Position { line: 0, character: 1 } },
            text: "X".into(),
        };
        assert_eq!(apply_change("ab", Encoding::Utf16, &inverted), Err(ChangeError::Inverted));
    }

    #[test]
    fn converting_to_bytes_and_back_is_identity_on_boundaries() {
        // Over generated lines: every boundary offset converts to a byte
        // offset that is itself a boundary, and back to the same offset.
        let alphabets =
            ["", "abc", "a\u{00E9}b", "café", "a\u{10400}b", "\u{1F600}\u{1F600} word \u{1F600}", "x\r", "ab\n"];
        for line in alphabets {
            for enc in [Encoding::Utf8, Encoding::Utf16, Encoding::Utf32] {
                for offset in boundary_offsets(line, enc) {
                    let bytes = offset_to_bytes(line, enc, offset).expect("a boundary offset converts");
                    // The byte offset is a UTF-8 boundary of the line.
                    assert!(line.is_char_boundary(bytes), "{line:?} {enc:?} {offset} -> {bytes}");
                    // The prefix up to it has exactly `offset` units in the
                    // encoding: the conversion landed where it should.
                    assert_eq!(line_length(&line[..bytes], enc), offset, "{line:?} {enc:?} {offset} -> {bytes}");
                    // And converting it back, in UTF-8, gives the same place.
                    assert_eq!(offset_to_bytes(line, Encoding::Utf8, bytes), Ok(bytes));
                }
            }
        }
    }

    #[test]
    fn applying_then_reverting_a_change_is_the_identity() {
        // Over generated texts and encodings: a change that deletes a range,
        // then one that restores it, gives the original text back.
        let texts = ["", "abc", "a\r\nb\rc", "café \u{10400}\u{1F600}\nword\n", DOC_B];
        for text in texts {
            for enc in [Encoding::Utf8, Encoding::Utf16, Encoding::Utf32] {
                let lines = split_lines(text);
                let n = lines.len();
                // The boundary offsets of each line in the encoding.
                let bounds: Vec<Vec<usize>> = lines.iter().map(|l| boundary_offsets(l, enc)).collect();
                for a in 0..n {
                    for b in a..n.min(a + 2) {
                        let sa = |c: usize| {
                            position_byte_clamped(text, enc, Position { line: a, character: c })
                                .unwrap_or_else(|e| panic!("{text:?} {enc:?} a={a} c={c} {e:?}"))
                        };
                        let sb = |c: usize| {
                            position_byte_clamped(text, enc, Position { line: b, character: c })
                                .unwrap_or_else(|e| panic!("{text:?} {enc:?} b={b} c={c} {e:?}"))
                        };
                        for ca in &bounds[a] {
                            for cb in &bounds[b] {
                                if (b, *cb) < (a, *ca) {
                                    continue;
                                }
                                let range = Range {
                                    start: Position { line: a, character: *ca },
                                    end: Position { line: b, character: *cb },
                                };
                                // Delete the range, then put the deleted
                                // text back: the original text comes back.
                                let deleted = &text[sa(*ca)..sb(*cb)];
                                let changed = apply_change(text, enc, &ContentChange::Text { range, text: "Z".into() })
                                    .expect("a fitting range applies");
                                // The inserted `Z` sits at the range's start.
                                let restore = Range {
                                    start: Position { line: a, character: *ca },
                                    end: Position { line: a, character: *ca + 1 },
                                };
                                let back = apply_change(
                                    &changed,
                                    enc,
                                    &ContentChange::Text { range: restore, text: deleted.into() },
                                )
                                .expect("the restore fits");
                                assert_eq!(back, text, "{text:?} {enc:?} {range:?}");
                            }
                        }
                    }
                }
            }
        }
    }

    /// The boundary offsets of a line in an encoding: 0 and the end of each
    /// character, in that encoding's units.
    fn boundary_offsets(line: &str, enc: Encoding) -> Vec<usize> {
        let mut out = vec![0];
        for c in line.chars() {
            out.push(
                out[out.len() - 1]
                    + match enc {
                        Encoding::Utf8 => c.len_utf8(),
                        Encoding::Utf16 => c.len_utf16(),
                        Encoding::Utf32 => 1,
                    },
            );
        }
        out
    }

    fn doc_params(uri: &str) -> Value {
        serde_json::from_str(&format!(r#"{{"textDocument":{{"uri":"{uri}"}}}}"#)).expect("valid JSON")
    }

    fn pos_json(line: usize, character: usize) -> Value {
        serde_json::from_str(&format!(r#"{{"line":{line},"character":{character}}}"#)).expect("valid JSON")
    }

    fn range_json(sl: usize, sc: usize, el: usize, ec: usize) -> Value {
        serde_json::from_str(&format!(
            r#"{{"start":{{"line":{sl},"character":{sc}}},"end":{{"line":{el},"character":{ec}}}}}"#
        ))
        .expect("valid JSON")
    }

    fn uris(out: &[Found]) -> Vec<(&str, &str, FoundKind)> {
        out.iter().map(|f| (f.uri.as_str(), f.pointer.as_str(), f.kind)).collect()
    }

    #[test]
    fn publish_diagnostics_walks_ranges_and_related_information() {
        let params: Value = serde_json::from_str(
            r#"{
                "uri": "file:///a.txt",
                "version": 41,
                "diagnostics": [
                    { "range": { "start": { "line": 0, "character": 1 }, "end": { "line": 0, "character": 2 } },
                      "relatedInformation": [
                        { "location": { "uri": "file:///b.txt", "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 3 } } } }
                      ]
                    },
                    { "data": { "line": 9, "character": 9 }, "range": { "start": { "line": 2, "character": 0 }, "end": { "line": 2, "character": 1 } } }
                ]
            }"#,
        )
        .expect("valid JSON");
        let out = positions("textDocument/publishDiagnostics", &params, None);
        assert_eq!(
            uris(&out),
            [
                ("file:///a.txt", "diagnostics[0].range", FoundKind::Range),
                ("file:///b.txt", "diagnostics[0].relatedInformation[0].location", FoundKind::Range),
                ("file:///a.txt", "diagnostics[1].range", FoundKind::Range),
            ]
        );
        // `data` is never walked.
        assert!(!out.iter().any(|f| f.pointer.contains("data")));
    }

    #[test]
    fn pull_diagnostics_walk_items_and_related_documents() {
        let params = doc_params("file:///a.txt");
        let result: Value = serde_json::from_str(
            r#"{
                "kind": "full",
                "items": [
                    { "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } } }
                ],
                "relatedDocuments": {
                    "file:///b.txt": {
                        "kind": "full",
                        "items": [
                            { "range": { "start": { "line": 3, "character": 0 }, "end": { "line": 3, "character": 2 } } }
                        ]
                    }
                }
            }"#,
        )
        .expect("valid JSON");
        let out = positions("textDocument/diagnostic", &params, Some(&result));
        assert_eq!(
            uris(&out),
            [
                ("file:///a.txt", "items[0].range", FoundKind::Range),
                ("file:///b.txt", "relatedDocuments.items[0].range", FoundKind::Range),
            ]
        );
    }

    #[test]
    fn document_symbols_walk_ranges_recursively() {
        let params = doc_params("file:///a.txt");
        let result: Value = serde_json::from_str(
            r#"[
                { "name": "outer", "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 9, "character": 0 } },
                  "selectionRange": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 5 } },
                  "children": [
                    { "name": "inner", "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 2, "character": 0 } },
                      "selectionRange": { "start": { "line": 1, "character": 1 }, "end": { "line": 1, "character": 2 } } }
                  ]
                },
                { "name": "sym", "location": { "uri": "file:///b.txt", "range": { "start": { "line": 4, "character": 0 }, "end": { "line": 4, "character": 3 } } } }
            ]"#,
        )
        .expect("valid JSON");
        let out = positions("textDocument/documentSymbol", &params, Some(&result));
        assert_eq!(
            uris(&out),
            [
                ("file:///a.txt", "[0].range", FoundKind::Range),
                ("file:///a.txt", "[0].selectionRange", FoundKind::Range),
                ("file:///a.txt", "[0].children[0].range", FoundKind::Range),
                ("file:///a.txt", "[0].children[0].selectionRange", FoundKind::Range),
                ("file:///b.txt", "[1].location", FoundKind::Range),
            ]
        );
    }

    #[test]
    fn folding_ranges_walk_lines_and_characters() {
        let params = doc_params("file:///a.txt");
        let result: Value = serde_json::from_str(
            r#"[
                { "startLine": 0, "endLine": 3 },
                { "startLine": 4, "endLine": 4, "startCharacter": 1, "endCharacter": 9 }
            ]"#,
        )
        .expect("valid JSON");
        let out = positions("textDocument/foldingRange", &params, Some(&result));
        assert_eq!(
            uris(&out),
            [
                ("file:///a.txt", "[0].startLine", FoundKind::Line),
                ("file:///a.txt", "[0].endLine", FoundKind::Line),
                ("file:///a.txt", "[1].startLine", FoundKind::Line),
                ("file:///a.txt", "[1].endLine", FoundKind::Line),
                ("file:///a.txt", "[1].startCharacter", FoundKind::Position),
                ("file:///a.txt", "[1].endCharacter", FoundKind::Position),
            ]
        );
    }

    #[test]
    fn selection_ranges_walk_parents() {
        let params = doc_params("file:///a.txt");
        let result: Value = serde_json::from_str(
            r#"[
                { "range": { "start": { "line": 0, "character": 1 }, "end": { "line": 0, "character": 2 } },
                  "parent": { "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 9 } } } }
            ]"#,
        )
        .expect("valid JSON");
        let out = positions("textDocument/selectionRange", &params, Some(&result));
        assert_eq!(
            uris(&out),
            [("file:///a.txt", "[0].range", FoundKind::Range), ("file:///a.txt", "[0].parent.range", FoundKind::Range),]
        );
    }

    #[test]
    fn range_arrays_walk_their_ranges() {
        let params = doc_params("file:///a.txt");
        let result: Value = serde_json::from_str(
            r#"[{ "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } } }]"#,
        )
        .expect("valid JSON");
        for method in [
            "textDocument/documentHighlight",
            "textDocument/documentLink",
            "textDocument/codeLens",
            "textDocument/documentColor",
            "textDocument/formatting",
            "textDocument/rangeFormatting",
        ] {
            let out = positions(method, &params, Some(&result));
            assert_eq!(uris(&out), [("file:///a.txt", "[0].range", FoundKind::Range)], "{method}");
        }
    }

    #[test]
    fn hover_walks_its_range() {
        let params = doc_params("file:///a.txt");
        let result: Value =
            serde_json::from_str(r#"{ "contents": "x", "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } } }"#)
                .expect("valid JSON");
        let out = positions("textDocument/hover", &params, Some(&result));
        assert_eq!(uris(&out), [("file:///a.txt", "range", FoundKind::Range)]);
        // A null hover has no positions.
        assert!(positions("textDocument/hover", &params, Some(&Value::Null)).is_empty());
    }

    #[test]
    fn inlay_hints_walk_positions_edits_and_labels() {
        let params = doc_params("file:///a.txt");
        let result: Value = serde_json::from_str(
            r#"[
                { "position": { "line": 0, "character": 2 },
                  "textEdits": [ { "range": { "start": { "line": 0, "character": 2 }, "end": { "line": 0, "character": 2 } } } ],
                  "label": [
                    { "value": "a", "location": { "uri": "file:///b.txt", "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 1 } } } }
                  ]
                },
                { "position": { "line": 1, "character": 0 },
                  "label": [ { "value": "b", "location": { "uri": "file:///c.txt", "range": { "start": { "line": 2, "character": 0 }, "end": { "line": 2, "character": 1 } } } } ] }
            ]"#,
        )
        .expect("valid JSON");
        let out = positions("textDocument/inlayHint", &params, Some(&result));
        assert_eq!(
            uris(&out),
            [
                ("file:///a.txt", "[0].position", FoundKind::Position),
                ("file:///a.txt", "[0].textEdits[0].range", FoundKind::Range),
                ("file:///b.txt", "[0].label[0].location", FoundKind::Range),
                ("file:///a.txt", "[1].position", FoundKind::Position),
                ("file:///c.txt", "[1].label[0].location", FoundKind::Range),
            ]
        );
    }

    #[test]
    fn locations_walk_location_and_location_link() {
        let params = doc_params("file:///a.txt");
        let result: Value = serde_json::from_str(
            r#"[
                { "uri": "file:///b.txt", "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } } },
                { "targetUri": "file:///c.txt",
                  "targetRange": { "start": { "line": 3, "character": 0 }, "end": { "line": 3, "character": 4 } },
                  "targetSelectionRange": { "start": { "line": 3, "character": 1 }, "end": { "line": 3, "character": 2 } },
                  "originSelectionRange": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } } }
            ]"#,
        )
        .expect("valid JSON");
        for method in [
            "textDocument/definition",
            "textDocument/declaration",
            "textDocument/typeDefinition",
            "textDocument/implementation",
            "textDocument/references",
        ] {
            let out = positions(method, &params, Some(&result));
            assert_eq!(
                uris(&out),
                [
                    ("file:///b.txt", "[0].range", FoundKind::Range),
                    ("file:///c.txt", "[1].targetRange", FoundKind::Range),
                    ("file:///c.txt", "[1].targetSelectionRange", FoundKind::Range),
                    ("file:///a.txt", "[1].originSelectionRange", FoundKind::Range),
                ],
                "{method}"
            );
        }
        // A single `Location`, not an array.
        let result: Value =
            serde_json::from_str(r#"{ "uri": "file:///b.txt", "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } } }"#)
                .expect("valid JSON");
        let out = positions("textDocument/definition", &params, Some(&result));
        assert_eq!(uris(&out), [("file:///b.txt", "[0].range", FoundKind::Range)]);
    }

    #[test]
    fn linked_editing_walks_its_ranges() {
        let params = doc_params("file:///a.txt");
        // `LinkedEditingRanges`: `{"ranges": [Range, ...]}`.
        let result: Value = serde_json::from_str(
            r#"{ "ranges": [ { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } },
                            { "start": { "line": 2, "character": 3 }, "end": { "line": 2, "character": 4 } } ] }"#,
        )
        .expect("valid JSON");
        let out = positions("textDocument/linkedEditingRange", &params, Some(&result));
        assert_eq!(
            uris(&out),
            [("file:///a.txt", "ranges[0]", FoundKind::Range), ("file:///a.txt", "ranges[1]", FoundKind::Range),]
        );
        // `null` has no positions.
        assert!(positions("textDocument/linkedEditingRange", &params, Some(&Value::Null)).is_empty());
    }

    #[test]
    fn prepare_rename_walks_a_range_or_a_wrapped_range() {
        let params = doc_params("file:///a.txt");
        let single: Value =
            serde_json::from_str(r#"{ "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } }"#)
                .expect("valid JSON");
        let out = positions("textDocument/prepareRename", &params, Some(&single));
        assert_eq!(uris(&out), [("file:///a.txt", "", FoundKind::Range)]);
        // `{range, placeholder}`.
        let wrapped: Value = serde_json::from_str(
            r#"{ "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } }, "placeholder": "x" }"#,
        )
        .expect("valid JSON");
        let out = positions("textDocument/prepareRename", &params, Some(&wrapped));
        assert_eq!(uris(&out), [("file:///a.txt", "range", FoundKind::Range)]);
        // `{defaultBehavior}` and `null` have no positions.
        let behavior: Value = serde_json::from_str(r#"{ "defaultBehavior": 2 }"#).expect("valid JSON");
        assert!(positions("textDocument/prepareRename", &params, Some(&behavior)).is_empty());
        assert!(positions("textDocument/prepareRename", &params, Some(&Value::Null)).is_empty());
    }

    #[test]
    fn malformed_shapes_are_skipped_silently() {
        let params = doc_params("file:///a.txt");
        // Missing members, negative values, wrong types: nothing is found,
        // nothing is an error.
        let result: Value = serde_json::from_str(
            r#"[
                { "range": { "start": { "line": 0 }, "end": { "line": 1, "character": 0 } } },
                { "range": { "start": { "line": -1, "character": 0 }, "end": { "line": 1, "character": 0 } } },
                { "range": { "start": { "line": "0", "character": 0 }, "end": { "line": 1, "character": 0 } } },
                { "range": "not a range" }
            ]"#,
        )
        .expect("valid JSON");
        assert!(positions("textDocument/documentHighlight", &params, Some(&result)).is_empty());
        // A result that is not the expected shape at all.
        assert!(positions("textDocument/hover", &params, Some(&Value::from(3))).is_empty());
        // A method not in the table walks nothing, whatever it carries.
        let sneaky = pos_json(0, 1);
        assert!(positions("textDocument/completion", &params, Some(&sneaky)).is_empty());
        assert!(positions("unknown/method", &params, Some(&range_json(0, 0, 1, 1))).is_empty());
        // A `data` field is never walked, even when it holds ranges.
        let result: Value =
            serde_json::from_str(r#"[{ "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } }, "data": { "range": { "start": { "line": 5, "character": 5 }, "end": { "line": 5, "character": 6 } } } }]"#)
                .expect("valid JSON");
        let out = positions("textDocument/documentHighlight", &params, Some(&result));
        assert_eq!(uris(&out), [("file:///a.txt", "[0].range", FoundKind::Range)]);
        // A bare `Diagnostic` array is not a `DocumentDiagnosticReport`. It
        // is still walked: its items are about the request's document, so
        // the attribution is certain.
        let result: Value = serde_json::from_str(
            r#"[{ "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } } }]"#,
        )
        .expect("valid JSON");
        let out = positions("textDocument/diagnostic", &params, Some(&result));
        assert_eq!(uris(&out), [("file:///a.txt", "[0].range", FoundKind::Range)]);
        // A single-object inlay-hint `label` is not a shape the spec
        // defines, so it is not walked.
        let result: Value = serde_json::from_str(
            r#"[{ "position": { "line": 0, "character": 0 }, "label": { "value": "b", "location": { "uri": "file:///c.txt", "range": { "start": { "line": 2, "character": 0 }, "end": { "line": 2, "character": 1 } } } } }]"#,
        )
        .expect("valid JSON");
        let out = positions("textDocument/inlayHint", &params, Some(&result));
        assert_eq!(uris(&out), [("file:///a.txt", "[0].position", FoundKind::Position)]);
        // A `prepareRename` result is never an array, so one is not walked.
        let result: Value = serde_json::from_str(
            r#"[{ "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } }]"#,
        )
        .expect("valid JSON");
        assert!(positions("textDocument/prepareRename", &params, Some(&result)).is_empty());
    }

    #[test]
    fn encoding_names_parse() {
        assert_eq!(Encoding::from_name("utf-8"), Some(Encoding::Utf8));
        assert_eq!(Encoding::from_name("utf-16"), Some(Encoding::Utf16));
        assert_eq!(Encoding::from_name("utf-32"), Some(Encoding::Utf32));
        assert_eq!(Encoding::from_name("utf7"), None);
        assert_eq!(Encoding::from_name("UTF-16"), None);
    }
}
