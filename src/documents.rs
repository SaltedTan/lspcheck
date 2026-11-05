// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! What the documents scenario judges: the documents lspcheck opened with
//! every version it sent, the texts a position may refer to, D1's verdict
//! over every position the server sent about them, and D2's verdict over
//! every version the server labelled diagnostics with.
//!
//! Pure functions only; the scenario that gathers the evidence is in
//! `checks`. A position is judged only where lspcheck knows exactly which
//! text it refers to, so the verdict can't come from a guess.

use crate::checks::Verdict;
use crate::text::{self, ContentChange, Encoding, Found, FoundKind, Position, PositionStatus, Range};
use serde_json::Value;
use std::time::Duration;

/// The lines of document A, LF only, with a final newline after the last.
///
/// - L0: six emoji, a word, six emoji. The start and end of `word` land
///   inside a character under each of the four common mix-ups (UTF-8 bytes
///   or code points reported as UTF-16, UTF-16 units or code points reported
///   as UTF-8).
/// - L1: a combining mark, 3-byte CJK characters, and typos for prose linters.
/// - L2: a ZWJ sequence, a flag, a word, and a 4-byte character at the end.
/// - L3: ASCII only.
pub const DOC_A_LINES: [&str; 4] = [
    "\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600} word \u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}",
    "cafe\u{301} = \"\u{4E2D}\u{6587}\"  # teh teh",
    "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} \u{1F1E6}\u{1F1FA} name \u{1F600}",
    "x = 1",
];

/// Document A's text.
pub fn doc_a() -> String {
    DOC_A_LINES.iter().map(|l| format!("{l}\n")).collect()
}

/// Where `needle` starts and ends on line `line` of `text`, counted in
/// `encoding`.
pub fn span_in(text: &str, line: usize, needle: &str, encoding: Encoding) -> (Position, Position) {
    let l = text::split_lines(text)[line];
    let at = l.find(needle).expect("the needle is on the line");
    let units = |bytes: usize| text::line_length(&l[..bytes], encoding);
    (Position { line, character: units(at) }, Position { line, character: units(at + needle.len()) })
}

/// Where `needle` starts and ends on line `line` of document A, counted in
/// `encoding`.
pub fn doc_a_span(line: usize, needle: &str, encoding: Encoding) -> (Position, Position) {
    span_in(&doc_a(), line, needle, encoding)
}

/// A version of a document that lspcheck sent, in `didOpen` or `didChange`.
#[derive(Debug, Clone)]
pub struct Version {
    /// When lspcheck began writing it.
    pub at: Duration,
    pub number: i64,
    pub text: String,
}

/// How the server asked for document changes: `change` in `textDocumentSync`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncKind {
    /// `0` or absent: the client must not send `didChange`.
    None,
    /// `1`: the whole text, each time.
    Full,
    /// `2`: ranges and inserted text.
    Incremental,
}

/// The sync kind from the initialize result's `textDocumentSync`: a number
/// or `{change: n}`. Absent, `0` or an unknown value means None. A number is
/// read as an integer however it is spelled, as [`version_number`].
pub fn sync_kind(result: &Value) -> SyncKind {
    let n = result.get("capabilities").and_then(|c| c.get("textDocumentSync")).and_then(|s| match s {
        Value::Number(n) => crate::message::integer_value(n),
        Value::Object(o) => o.get("change").and_then(|v| v.as_number().and_then(crate::message::integer_value)),
        _ => None,
    });
    match n {
        Some(1) => SyncKind::Full,
        Some(2) => SyncKind::Incremental,
        _ => SyncKind::None,
    }
}

/// The two content changes the scenario sends for an incremental sync,
/// counted in `encoding`: c1 replaces the first emoji of line 0 with four
/// emoji, c2 inserts a line of ten emoji after line 0. c2's position is of
/// the text after c1, as the spec requires.
pub fn incremental_changes(before: &str, encoding: Encoding) -> (ContentChange, ContentChange) {
    let line0 = text::split_lines(before)[0];
    // The first character's length in the encoding, through the text model:
    // 2 UTF-16 units, 4 UTF-8 bytes, 1 code point.
    let first = line0.chars().next().expect("the document is not empty").len_utf8();
    let end = text::line_length(&line0[..first], encoding);
    let c1 = ContentChange::Text {
        range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: end } },
        text: "\u{1F600}\u{1F600}\u{1F600}\u{1F600}".into(),
    };
    let c2 = ContentChange::Text {
        range: Range { start: Position { line: 1, character: 0 }, end: Position { line: 1, character: 0 } },
        text: "\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\n".into(),
    };
    (c1, c2)
}

/// The `contentChanges` the scenario sends for a sync of `kind`, and the
/// text they define from `before`, applied in `encoding`. A Full sync sends
/// the whole changed text.
pub fn scenario_change(before: &str, encoding: Encoding, kind: SyncKind) -> (Vec<ContentChange>, String) {
    let (c1, c2) = incremental_changes(before, encoding);
    let mut after = before.to_string();
    for c in [&c1, &c2] {
        after = text::apply_change(&after, encoding, c).expect("the scenario's change fits its own text");
    }
    let changes = match kind {
        SyncKind::Full => vec![ContentChange::Full(after.clone())],
        SyncKind::Incremental => vec![c1, c2],
        SyncKind::None => unreachable!("no change is sent for a sync kind of None"),
    };
    (changes, after)
}

/// The change the scenario sent, and the texts D3 judges against.
#[derive(Debug, Clone)]
pub struct Change {
    /// When lspcheck began writing the `didChange`.
    pub at: Duration,
    /// The text before the change.
    pub before: String,
    /// The text after the change, applied in the negotiated encoding.
    pub after: String,
    /// The alternative texts D3's rule (b) uses: each text, what it models,
    /// and the encoding its positions are counted in (the negotiated one: a
    /// server that mishandles a change still counts positions as agreed).
    pub alternatives: Vec<(String, String, Encoding)>,
}

/// Whether everything `source` says about `doc` can only refer to versions
/// sent from `change_at` on: a reply to a request sent after the change, or
/// diagnostics labelled with such a version. A late reply to an earlier
/// request, or unlabelled diagnostics, may refer to the text before.
pub fn only_after(doc: &OpenDoc, source: &Source, change_at: Duration) -> bool {
    let candidates = candidates(doc, source);
    !candidates.is_empty() && candidates.iter().all(|v| v.at >= change_at)
}

/// The alternative texts for D3's rule (b): the text before the change (an
/// ignored change, a stale snapshot, or the file read from disk, counted in
/// the negotiated encoding), and, for an incremental sync, the change
/// applied with its range counted in each other encoding.
pub fn alternatives(before: &str, encoding: Encoding, kind: SyncKind) -> Vec<(String, String, Encoding)> {
    let mut out = vec![(before.to_string(), "the text before the change".to_string(), encoding)];
    if kind == SyncKind::Incremental {
        let (c1, c2) = incremental_changes(before, encoding);
        for e in [Encoding::Utf8, Encoding::Utf16, Encoding::Utf32] {
            if e == encoding {
                continue;
            }
            let mut t = before.to_string();
            for c in [&c1, &c2] {
                t = apply_lenient(&t, e, c).expect("the scenario's change fits its own text");
            }
            // Such a server still counts its positions in the negotiated
            // encoding; only the change's range was counted wrongly.
            out.push((t, format!("the change applied counting {}", counted_in(e)), encoding));
        }
    }
    out
}

/// Applies `change` to `text` counted in `encoding`, the way a server
/// counting in `encoding` would land on the range: an offset inside a
/// character floors to the start of that character, one past the end of a
/// line to the line's end. `None` when a line of the range doesn't exist.
fn apply_lenient(text: &str, encoding: Encoding, change: &ContentChange) -> Option<String> {
    let floor = |p: Position| -> Option<usize> {
        let mut start = 0;
        for (n, l) in text::split_lines(text).into_iter().enumerate() {
            if n == p.line {
                let mut bytes = 0;
                let mut units = 0;
                for c in l.chars() {
                    let len = match encoding {
                        Encoding::Utf8 => c.len_utf8(),
                        Encoding::Utf16 => c.len_utf16(),
                        Encoding::Utf32 => 1,
                    };
                    if p.character < units + len {
                        return Some(start + bytes);
                    }
                    units += len;
                    bytes += c.len_utf8();
                }
                return Some(start + bytes);
            }
            start += l.len();
            let rest = &text[start..];
            start += if rest.starts_with("\r\n") { 2 } else { 1 };
        }
        None
    };
    match change {
        ContentChange::Full(t) => Some(t.clone()),
        ContentChange::Text { range, text: insert } => {
            let start = floor(range.start)?;
            let end = floor(range.end)?.max(start);
            Some(format!("{}{}{}", &text[..start], insert, &text[end..]))
        }
    }
}

/// A document lspcheck opened.
#[derive(Debug, Clone)]
pub struct OpenDoc {
    pub uri: String,
    /// Every version sent, oldest first.
    pub versions: Vec<Version>,
}

/// Something the server sent that may carry positions.
#[derive(Debug, Clone)]
pub struct Source {
    /// The method of the request it answers, or of the notification.
    pub method: String,
    pub found: Vec<Found>,
    /// When the request it answers was sent; `None` for a notification.
    pub asked_at: Option<Duration>,
    /// When it was read.
    pub read_at: Duration,
    /// The `version` a `publishDiagnostics` is labelled with, if any.
    pub label: Option<i64>,
}

/// A `textDocument/publishDiagnostics` notification the server sent.
#[derive(Debug, Clone)]
pub struct Published {
    pub uri: String,
    /// The `version` member of the params: `None` when it isn't there.
    pub version: Option<Value>,
    /// When the notification was read.
    pub read_at: Duration,
}

/// A document version's value, when it is a JSON integer that fits, however
/// it is spelled: `42`, `42.0` and `4.2e1` are all 42.
pub fn version_number(v: &Value) -> Option<i64> {
    v.as_number().and_then(crate::message::integer_value)
}

/// Whether two URIs name the same document: equal as written, or equal
/// after percent-decoding and lower-casing a Windows drive letter.
pub fn same_uri(a: &str, b: &str) -> bool {
    a == b || normalized(a) == normalized(b)
}

fn normalized(uri: &str) -> String {
    let bytes = uri.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(h), Some(l)) = (bytes.get(i + 1).and_then(|b| hex(*b)), bytes.get(i + 2).and_then(|b| hex(*b)))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let mut text = String::from_utf8_lossy(&out).into_owned();
    // `file:///C:/...` and `file:///c:/...` name the same file.
    if let Some(rest) = text.strip_prefix("file:///")
        && rest.as_bytes().get(1) == Some(&b':')
        && rest.as_bytes()[0].is_ascii_alphabetic()
    {
        text = format!("file:///{}{}", rest[..1].to_ascii_lowercase(), &rest[1..]);
    }
    text
}

/// The texts a position from `source` about `doc` may refer to. A
/// `publishDiagnostics` labelled with a version lspcheck sent refers to that
/// version. A reply refers to the version current when its request was sent,
/// or to one sent before it was read, should a change have overtaken it. Any
/// other notification may refer to any version sent before it was read.
pub fn candidates<'a>(doc: &'a OpenDoc, source: &Source) -> Vec<&'a Version> {
    let sent = |v: &&Version| v.at <= source.read_at;
    if let Some(n) = source.label
        && let Some(v) = doc.versions.iter().filter(sent).find(|v| v.number == n)
    {
        return vec![v];
    }
    let from = match source.asked_at {
        Some(asked) => doc.versions.iter().rposition(|v| v.at <= asked).unwrap_or(0),
        None => 0,
    };
    doc.versions[from..].iter().filter(sent).collect()
}

/// How a counting is named in details.
fn counted_in(e: Encoding) -> &'static str {
    match e {
        Encoding::Utf8 => "UTF-8 bytes",
        Encoding::Utf16 => "UTF-16 code units",
        Encoding::Utf32 => "code points",
    }
}

const ENCODINGS: [Encoding; 3] = [Encoding::Utf8, Encoding::Utf16, Encoding::Utf32];

/// What the positions about the opened documents showed.
#[derive(Debug, Default)]
pub struct Judged {
    /// Positions found in documents lspcheck opened (lines alone aren't counted).
    pub seen: usize,
    /// Positions in documents lspcheck didn't open, which aren't judged.
    pub elsewhere: usize,
    /// Positions at a character boundary.
    pub valid: usize,
    /// Valid positions with non-ASCII text before them on their line.
    pub informative: usize,
    /// Positions inside a character in every text they may refer to,
    /// described for the detail, with a diagnosis.
    pub invalid: Vec<String>,
    /// The positions in [`Judged::invalid`], in the same order.
    pub invalid_positions: Vec<Position>,
    /// Every position judged as valid or invalid (not clamped, not on a
    /// missing line, not in another document), in the order read. Used to
    /// tell a server that reports `word`'s exact span under another encoding
    /// (a counting fault) from one that reports a different span.
    pub all_positions: Vec<Position>,
    /// Positions past the end of their line, which the spec lets clients clamp.
    pub past_end: usize,
    /// Of those, how many would be in bounds counted another way, by counting.
    pub past_end_fits: Vec<(Encoding, usize)>,
    /// Positions on a line the document doesn't have.
    pub no_line: usize,
    /// Positions read before lspcheck sent any version of their document
    /// (a server may read the saved file), which have no text to refer to.
    pub before_open: usize,
}

/// Judges every position in `sources` that is about a document in `docs`,
/// counted in `encoding`.
pub fn judge_positions(encoding: Encoding, docs: &[OpenDoc], sources: &[Source]) -> Judged {
    judge_positions_where(encoding, docs, sources, |_| true)
}

/// [`judge_positions`], judging only the sources `when` admits.
pub fn judge_positions_where(
    encoding: Encoding,
    docs: &[OpenDoc],
    sources: &[Source],
    when: impl Fn(&Source) -> bool,
) -> Judged {
    let mut j = Judged::default();
    for source in sources.iter().filter(|s| when(s)) {
        for found in &source.found {
            if found.kind == FoundKind::Line {
                continue;
            }
            let Some(doc) = docs.iter().find(|d| same_uri(&d.uri, &found.uri)) else {
                j.elsewhere += found.at.len();
                continue;
            };
            let texts = candidates(doc, source);
            if texts.is_empty() {
                j.before_open += found.at.len();
                continue;
            }
            let ends: &[&str] = if found.kind == FoundKind::Range { &[".start", ".end"] } else { &[""] };
            for (p, end) in found.at.iter().zip(ends) {
                j.seen += 1;
                let path = format!("{}{end}", found.pointer);
                let where_ = format!("{} {}", source.method, path.trim_start_matches('.'));
                judge_one(&mut j, encoding, &texts, *p, &where_);
            }
        }
    }
    j
}

fn judge_one(j: &mut Judged, encoding: Encoding, texts: &[&Version], p: Position, where_: &str) {
    let statuses: Vec<PositionStatus> =
        texts.iter().map(|v| text::position_status(&v.text, encoding, p.line, p.character)).collect();
    if let Some(i) = statuses.iter().position(|s| *s == PositionStatus::Boundary) {
        j.valid += 1;
        j.all_positions.push(p);
        // Only text before it on its line can make counting matter.
        let informative = texts.iter().zip(&statuses).skip(i).any(|(v, s)| {
            *s == PositionStatus::Boundary && {
                let line = text::split_lines(&v.text)[p.line];
                text::offset_to_bytes(line, encoding, p.character).is_ok_and(|b| !line[..b].is_ascii())
            }
        });
        if informative {
            j.informative += 1;
        }
    } else if statuses.iter().all(|s| *s == PositionStatus::InsideCharacter) {
        let v = texts[0];
        let fits: Vec<&str> = ENCODINGS
            .iter()
            .filter(|e| **e != encoding)
            .filter(|e| text::position_status(&v.text, **e, p.line, p.character) == PositionStatus::Boundary)
            .map(|e| counted_in(*e))
            .collect();
        let diagnosis = if fits.is_empty() {
            "on no character boundary counted any other way".to_string()
        } else {
            format!("on a boundary if counted in {}", fits.join(" or "))
        };
        let versions: Vec<String> = texts.iter().map(|v| v.number.to_string()).collect();
        j.invalid.push(format!(
            "{where_} (line {}, character {}) is inside a character of version {} ({diagnosis})",
            p.line,
            p.character,
            versions.join(" or ")
        ));
        j.invalid_positions.push(p);
        j.all_positions.push(p);
    } else if statuses.contains(&PositionStatus::PastEndOfLine) {
        // Not judged: the client clamps it. In bounds counted another way, it
        // hints at that counting, which D1 can't prove.
        j.past_end += 1;
        let line_fits = |e: Encoding| {
            texts
                .iter()
                .any(|v| text::split_lines(&v.text).get(p.line).is_some_and(|l| p.character <= text::line_length(l, e)))
        };
        if let Some(e) = ENCODINGS.into_iter().filter(|e| *e != encoding).find(|e| line_fits(*e)) {
            match j.past_end_fits.iter_mut().find(|(x, _)| *x == e) {
                Some((_, n)) => *n += 1,
                None => j.past_end_fits.push((e, 1)),
            }
        }
    } else {
        // On a line no candidate has (or inside a character in one text and
        // out of bounds in another): not judged.
        j.no_line += 1;
    }
}

/// How many invalid positions a failure names.
const NAMED: usize = 3;

/// D1's verdict once a document was opened. `incomplete` says why lspcheck
/// may have missed something the server sent from the first `didOpen` on.
pub fn judge_d1(encoding: Encoding, j: &Judged, incomplete: Option<&str>) -> (Verdict, String) {
    let units = counted_in(encoding);
    if !j.invalid.is_empty() {
        // A failure that was seen stands, whatever else was missed.
        let more = j.invalid.len().saturating_sub(NAMED);
        let tail = if more > 0 { format!("; and {more} more") } else { String::new() };
        let first: Vec<&str> = j.invalid.iter().take(NAMED).map(String::as_str).collect();
        return (
            Verdict::Fail,
            format!(
                "{} position(s) fall inside a character, counted in {units} (the negotiated encoding): {}{tail}",
                j.invalid.len(),
                first.join("; ")
            ),
        );
    }
    let counts = counts(j);
    if encoding == Encoding::Utf32 {
        return (
            Verdict::NotExercised,
            format!("the server chose utf-32, in which no offset can fall inside a character; {counts}"),
        );
    }
    // What lspcheck missed may have held positions, informative ones
    // included, so with nothing informative judged it can't say "no evidence".
    if let Some(why) = incomplete {
        return (
            Verdict::Error,
            format!("inconclusive, because {why}: it may have held a position inside a character; {counts}"),
        );
    }
    if j.informative == 0 {
        return (
            Verdict::NotExercised,
            format!("no position with non-ASCII text before it on its line was judged; {counts}"),
        );
    }
    (
        Verdict::Pass,
        format!("{} informative position(s) on character boundaries, counted in {units}; {counts}", j.informative),
    )
}

/// The counts a D1 detail gives.
fn counts(j: &Judged) -> String {
    let mut out = format!(
        "{} position(s) seen in the opened document, {} past the end of a line, {} on a line it doesn't have, {} in other documents",
        j.seen, j.past_end, j.no_line, j.elsewhere
    );
    if j.before_open > 0 {
        out.push_str(&format!(", {} read before it was opened", j.before_open));
    }
    for (e, n) in &j.past_end_fits {
        out.push_str(&format!(
            "; {n} past the end of a line would fit counted in {}, a hint (not a finding) of that counting",
            counted_in(*e)
        ));
    }
    out
}

/// D3's verdict, judged at the end over the positions that can only refer to
/// the changed text (see [`only_after`]).
///
/// A position is invalid after the change only if it is inside a character
/// in every text it may refer to, the same rule as D1. D3 fails only when an
/// invalid position is explained by a text the change should have produced
/// and by no reading of the changed text: it falls on a boundary of an
/// alternative text (the text before the change, or the change applied with
/// its range counted in another encoding, both counted in the negotiated
/// encoding), and on no boundary within its line of version 42 under any
/// other encoding, UTF-32 included. A server that applied the change but
/// counts in another encoding is explained by that reading, so it is D1's
/// finding. Correct positions seen before the change can't prove a server
/// counts correctly everywhere (a coincidence, an echoed position or mixed
/// counting all look the same), so they are never used to blame the change.
/// `incomplete` says why lspcheck may have missed something the server sent
/// from the change to the end. The skip (no `didOpen`, no change sent) and
/// the registration and undelivered-change errors are the scenario's.
pub fn judge_d3(encoding: Encoding, after: &Judged, change: &Change, incomplete: Option<&str>) -> (Verdict, String) {
    let units = counted_in(encoding);
    let mut fits: Vec<(usize, &str)> = Vec::new();
    for (i, p) in after.invalid_positions.iter().enumerate() {
        let reread = ENCODINGS.iter().any(|e| {
            *e != encoding && text::position_status(&change.after, *e, p.line, p.character) == PositionStatus::Boundary
        });
        if reread {
            continue;
        }
        if let Some(name) = change
            .alternatives
            .iter()
            .find(|(t, _, e)| text::position_status(t, *e, p.line, p.character) == PositionStatus::Boundary)
            .map(|(_, name, _)| name.as_str())
        {
            fits.push((i, name));
        }
    }
    if !fits.is_empty() {
        // A failure that was seen stands, whatever else was missed.
        let more = fits.len().saturating_sub(NAMED);
        let tail = if more > 0 { format!("; and {more} more") } else { String::new() };
        let first: Vec<String> =
            fits.iter().take(NAMED).map(|(i, name)| format!("{} (fits {name})", after.invalid[*i])).collect();
        return (
            Verdict::Fail,
            format!(
                "{} position(s) read after the change fall inside a character of version 42, counted in {units}, \
                 on no boundary of it counted any other way, and on a boundary of a text the change should have \
                 produced: {}{tail}",
                fits.len(),
                first.join("; ")
            ),
        );
    }
    if !after.invalid.is_empty() {
        return (
            Verdict::Error,
            format!(
                "{} position(s) read after the change fall inside a character of version 42, counted in {units}, \
                 but can't tell a change-handling fault from a counting fault; see D1",
                after.invalid.len()
            ),
        );
    }
    if encoding == Encoding::Utf32 {
        return (
            Verdict::NotExercised,
            format!(
                "the server chose utf-32, in which no offset can fall inside a character; {} position(s) read after the change",
                after.seen
            ),
        );
    }
    // What lspcheck missed may have held positions, informative ones included.
    if let Some(why) = incomplete {
        return (
            Verdict::Error,
            format!(
                "inconclusive, because {why}: it may have held a position inside a character of the changed text; \
                 {} position(s) read after the change",
                after.seen
            ),
        );
    }
    if after.informative == 0 {
        return (
            Verdict::NotExercised,
            format!(
                "no position with non-ASCII text before it on its line was judged after the change; {} position(s) read after the change",
                after.seen
            ),
        );
    }
    (
        Verdict::Pass,
        format!(
            "{} informative position(s) on character boundaries after the change, counted in {units}; {} position(s) read after the change",
            after.informative, after.seen
        ),
    )
}

/// The lawful publishes a D2 detail counts, after its main clause.
fn unjudged_counts(unversioned: usize, unjudged: usize) -> String {
    let mut out = String::new();
    if unversioned > 0 {
        out.push_str(&format!("; {unversioned} without a version, which is lawful"));
    }
    if unjudged > 0 {
        out.push_str(&format!("; {unjudged} with a version that isn't an integer, which D2 doesn't judge"));
    }
    out
}

/// D2's verdict: an integer `version` in `textDocument/publishDiagnostics`
/// for a document lspcheck opened is one lspcheck sent for that URI (in
/// `didOpen`, later `didChange`) before the notification was read. An older
/// version is lawful, and so is none at all; `null` or a non-integer one
/// isn't judged here (a type error for K2), nor is any URI lspcheck didn't
/// open. `incomplete` says why lspcheck may have missed a notification the
/// server sent from the `didOpen` on.
pub fn judge_d2(docs: &[OpenDoc], published: &[Published], incomplete: Option<&str>) -> (Verdict, String) {
    let mut fails: Vec<String> = Vec::new();
    let (mut checked, mut unversioned, mut unjudged) = (0usize, 0usize, 0usize);
    for p in published {
        let Some(doc) = docs.iter().find(|d| same_uri(&d.uri, &p.uri)) else { continue };
        let Some(n) = p.version.as_ref().and_then(version_number) else {
            match p.version {
                None => unversioned += 1,
                Some(_) => unjudged += 1,
            }
            continue;
        };
        // The same "sent" test as `candidates`: what lspcheck had written
        // for this URI by the time the notification was read.
        let sent: Vec<i64> = doc.versions.iter().filter(|v| v.at <= p.read_at).map(|v| v.number).collect();
        if sent.contains(&n) {
            checked += 1;
            continue;
        }
        let had = sent.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(" and ");
        let had = if had.is_empty() { "none".to_string() } else { had };
        fails.push(format!(
            "{} carries version {}, and lspcheck had sent {had} for it before the notification was read",
            p.uri, n
        ));
    }
    if !fails.is_empty() {
        // A failure that was seen stands, whatever else was missed.
        let more = fails.len().saturating_sub(NAMED);
        let tail = if more > 0 { format!("; and {more} more") } else { String::new() };
        let first: Vec<&str> = fails.iter().take(NAMED).map(String::as_str).collect();
        return (
            Verdict::Fail,
            format!(
                "{} diagnostics notification(s) carry a version lspcheck hadn't sent for the document: {}{tail}",
                fails.len(),
                first.join("; ")
            ),
        );
    }
    let extras = unjudged_counts(unversioned, unjudged);
    if let Some(why) = incomplete {
        return (
            Verdict::Error,
            format!(
                "inconclusive, because {why}: a versioned diagnostics notification may have been missed; \
                 {checked} versioned diagnostics notification(s) arrived{extras}"
            ),
        );
    }
    if checked == 0 {
        return (
            Verdict::NotExercised,
            format!("no versioned diagnostics notification for an opened document arrived{extras}"),
        );
    }
    (
        Verdict::Pass,
        format!("{checked} versioned diagnostics notification(s) named a version lspcheck had sent{extras}"),
    )
}

#[cfg(test)]
mod tests {

    use super::*;

    const URI: &str = "file:///w/doc.txt";

    fn doc(versions: &[(u64, i64, &str)]) -> OpenDoc {
        OpenDoc {
            uri: URI.into(),
            versions: versions
                .iter()
                .map(|(at, n, t)| Version { at: Duration::from_millis(*at), number: *n, text: t.to_string() })
                .collect(),
        }
    }

    fn range(uri: &str, (sl, sc): (usize, usize), (el, ec): (usize, usize)) -> Found {
        Found {
            uri: uri.into(),
            pointer: "[0].range".into(),
            kind: FoundKind::Range,
            at: vec![Position { line: sl, character: sc }, Position { line: el, character: ec }],
        }
    }

    fn reply(found: Vec<Found>, asked: u64, read: u64) -> Source {
        Source {
            method: "textDocument/documentSymbol".into(),
            found,
            asked_at: Some(Duration::from_millis(asked)),
            read_at: Duration::from_millis(read),
            label: None,
        }
    }

    fn diagnostics(found: Vec<Found>, read: u64, label: Option<i64>) -> Source {
        Source {
            method: "textDocument/publishDiagnostics".into(),
            found,
            asked_at: None,
            read_at: Duration::from_millis(read),
            label,
        }
    }

    #[test]
    fn document_a_is_lf_only_and_its_mixups_land_inside_characters() {
        let text = doc_a();
        assert!(!text.contains('\r') && text.ends_with("x = 1\n"));
        assert_eq!(text::split_lines(&text).len(), DOC_A_LINES.len() + 1);
        // No line separator other than the spec's, and no BOM.
        assert!(!text.chars().any(|c| matches!(c, '\u{2028}' | '\u{2029}' | '\u{85}' | '\u{c}' | '\u{feff}')));
        // For each mix-up (counted one way, reported as another), a boundary
        // of `word` on L0 lands inside a character.
        for (reported, counted) in [
            (Encoding::Utf16, Encoding::Utf8),
            (Encoding::Utf16, Encoding::Utf32),
            (Encoding::Utf8, Encoding::Utf16),
            (Encoding::Utf8, Encoding::Utf32),
        ] {
            let (start, end) = doc_a_span(0, "word", counted);
            let inside = [start, end].iter().any(|p| {
                text::position_status(&text, reported, p.line, p.character) == PositionStatus::InsideCharacter
            });
            assert!(inside, "{counted:?} reported as {reported:?}");
        }
        // Counted correctly, they are boundaries.
        for e in ENCODINGS {
            let (start, end) = doc_a_span(0, "word", e);
            for p in [start, end] {
                assert_eq!(text::position_status(&text, e, p.line, p.character), PositionStatus::Boundary);
            }
        }
    }

    #[test]
    fn uris_match_exactly_or_after_normalizing() {
        assert!(same_uri("file:///a/b.txt", "file:///a/b.txt"));
        assert!(same_uri("file:///a/b%20c.txt", "file:///a/b c.txt"));
        assert!(same_uri("file:///C:/x/doc.txt", "file:///c%3A/x/doc.txt"));
        assert!(!same_uri("file:///a/b.txt", "file:///a/c.txt"));
        // Only the drive letter's case is folded.
        assert!(!same_uri("file:///a/B.txt", "file:///a/b.txt"));
        // A malformed escape is kept as written.
        assert!(same_uri("file:///a/%zz", "file:///a/%zz"));
    }

    #[test]
    fn candidates_follow_versions_and_labels() {
        let d = doc(&[(10, 41, "a"), (100, 42, "b")]);
        let nums = |s: &Source| candidates(&d, s).iter().map(|v| v.number).collect::<Vec<_>>();
        // A reply to a request sent at version 41 and read before the change.
        assert_eq!(nums(&reply(vec![], 20, 50)), [41]);
        // Read after the change: either.
        assert_eq!(nums(&reply(vec![], 20, 150)), [41, 42]);
        // Sent after the change.
        assert_eq!(nums(&reply(vec![], 120, 150)), [42]);
        // A labelled notification refers to its version; an unlabelled one, or
        // one with a version never sent, to any version sent before it.
        assert_eq!(nums(&diagnostics(vec![], 150, Some(41))), [41]);
        assert_eq!(nums(&diagnostics(vec![], 150, None)), [41, 42]);
        assert_eq!(nums(&diagnostics(vec![], 150, Some(7))), [41, 42]);
        assert_eq!(nums(&diagnostics(vec![], 50, Some(42))), [41]);
        // Before anything was sent: nothing, and its positions aren't judged.
        assert!(nums(&diagnostics(vec![], 5, None)).is_empty());
        let early = diagnostics(vec![range(URI, (0, 1), (0, 1))], 5, None);
        let j = judge_positions(Encoding::Utf16, std::slice::from_ref(&d), &[early]);
        assert_eq!((j.seen, j.before_open, j.invalid.len()), (0, 2, 0));
    }

    #[test]
    fn positions_are_sorted_into_judged_and_not_judged() {
        let text = doc_a();
        let d = [doc(&[(10, 41, &text)])];
        let (ws, we) = doc_a_span(0, "word", Encoding::Utf16);
        let (bs, be) = doc_a_span(0, "word", Encoding::Utf8);
        let sources = [
            // Valid and informative (emoji before it).
            reply(vec![range(URI, (ws.line, ws.character), (we.line, we.character))], 20, 30),
            // Valid but ASCII before it: not informative.
            reply(vec![range(URI, (3, 0), (3, 1))], 20, 30),
            // Bytes reported as UTF-16: inside a character.
            diagnostics(vec![range(URI, (bs.line, bs.character), (be.line, be.character))], 40, Some(41)),
            // Past the end of L3 (5 units long) and on a line A doesn't have.
            diagnostics(vec![range(URI, (3, 8), (99, 0))], 40, None),
            // In a document lspcheck didn't open.
            diagnostics(vec![range("file:///w/other.txt", (0, 1), (0, 3))], 40, None),
        ];
        let j = judge_positions(Encoding::Utf16, &d, &sources);
        assert_eq!((j.seen, j.valid, j.informative, j.past_end, j.no_line, j.elsewhere), (8, 4, 2, 1, 1, 2));
        assert_eq!(j.invalid.len(), 2, "{:?}", j.invalid);
        assert!(j.invalid[0].starts_with("textDocument/publishDiagnostics [0].range.start (line 0, character 25)"));
        assert!(j.invalid[0].contains("on a boundary if counted in UTF-8 bytes"), "{}", j.invalid[0]);
        // L3 is 5 units long in every counting, so the past-end position fits none.
        assert!(j.past_end_fits.is_empty());
    }

    #[test]
    fn grapheme_splits_are_boundaries() {
        let text = doc_a();
        let d = [doc(&[(10, 41, &text)])];
        // Between `e` and U+0301, and inside the ZWJ family sequence after
        // the first person (two UTF-16 units in).
        let sources = [reply(vec![range(URI, (1, 4), (2, 2))], 20, 30)];
        let j = judge_positions(Encoding::Utf16, &d, &sources);
        assert!(j.invalid.is_empty(), "{:?}", j.invalid);
        assert_eq!(j.valid, 2);
    }

    #[test]
    fn a_position_is_invalid_only_inside_a_character_of_every_candidate() {
        // Offset 1 is inside the emoji of version 41 and a boundary of 42.
        let d = [doc(&[(10, 41, "\u{1F600}"), (100, 42, "ab")])];
        let late = reply(vec![range(URI, (0, 1), (0, 1))], 20, 150);
        assert!(judge_positions(Encoding::Utf16, &d, std::slice::from_ref(&late)).invalid.is_empty());
        let on_time = reply(vec![range(URI, (0, 1), (0, 1))], 20, 50);
        assert_eq!(judge_positions(Encoding::Utf16, &d, &[on_time]).invalid.len(), 2);
    }

    #[test]
    fn past_end_positions_that_fit_another_counting_are_hinted() {
        let text = doc_a();
        let d = [doc(&[(10, 41, &text)])];
        // L0 is 30 UTF-16 units and 54 bytes long.
        let j = judge_positions(Encoding::Utf16, &d, &[reply(vec![range(URI, (0, 40), (0, 54))], 20, 30)]);
        assert_eq!(j.past_end, 2);
        assert_eq!(j.past_end_fits, [(Encoding::Utf8, 2)]);
        let (v, detail) = judge_d1(Encoding::Utf16, &j, None);
        assert_eq!(v, Verdict::NotExercised);
        assert!(detail.contains("would fit counted in UTF-8 bytes"), "{detail}");
    }

    #[test]
    fn d1_verdicts() {
        let informative = Judged { seen: 2, valid: 2, informative: 1, ..Judged::default() };
        let bare = Judged { seen: 2, valid: 2, ..Judged::default() };
        let invalid = Judged { seen: 1, invalid: vec!["x".into()], ..Judged::default() };
        let missed = Some("a frame was skipped");
        assert_eq!(judge_d1(Encoding::Utf16, &informative, None).0, Verdict::Pass);
        // A pass becomes an error when evidence was missed.
        assert_eq!(judge_d1(Encoding::Utf16, &informative, missed).0, Verdict::Error);
        // A failure that was seen stands.
        assert_eq!(judge_d1(Encoding::Utf16, &invalid, missed).0, Verdict::Fail);
        // Nothing informative, or UTF-32: not exercised.
        assert_eq!(judge_d1(Encoding::Utf16, &bare, None).0, Verdict::NotExercised);
        // Missed evidence could have held positions, so it's an error, not "none".
        assert_eq!(judge_d1(Encoding::Utf8, &Judged::default(), missed).0, Verdict::Error);
        assert_eq!(judge_d1(Encoding::Utf32, &informative, None).0, Verdict::NotExercised);
        // A failure names the first few positions and counts the rest.
        let many = Judged { invalid: (0..5).map(|i| format!("p{i}")).collect(), ..Judged::default() };
        let (_, detail) = judge_d1(Encoding::Utf8, &many, None);
        assert!(detail.starts_with("5 position(s) fall inside a character, counted in UTF-8 bytes"), "{detail}");
        assert!(detail.contains("p0; p1; p2; and 2 more"), "{detail}");
    }

    fn publish(uri: &str, version: Value, read: u64) -> Published {
        Published { uri: uri.into(), version: Some(version), read_at: Duration::from_millis(read) }
    }

    fn unversioned(uri: &str, read: u64) -> Published {
        Published { uri: uri.into(), version: None, read_at: Duration::from_millis(read) }
    }

    #[test]
    fn d2_accepts_every_version_sent_before_the_publish_was_read() {
        let d = [doc(&[(10, 41, "a"), (100, 42, "b")])];
        // The version it was opened with, still lawful when read after a
        // later version was sent: an older version is lawful.
        let (v, detail) =
            judge_d2(&d, &[publish(URI, serde_json::json!(41), 50), publish(URI, serde_json::json!(41), 150)], None);
        assert_eq!(v, Verdict::Pass);
        assert!(
            detail.starts_with("2 versioned diagnostics notification(s) named a version lspcheck had sent"),
            "{detail}"
        );
    }

    #[test]
    fn d2_fails_on_a_version_sent_only_after_the_publish_was_read() {
        // Version 42 exists, but only from 100 ms on; at 50 the document had 41.
        let d = [doc(&[(10, 41, "a"), (100, 42, "b")])];
        let (v, detail) = judge_d2(&d, &[publish(URI, serde_json::json!(42), 50)], None);
        assert_eq!(v, Verdict::Fail);
        assert!(
            detail.contains("carries version 42, and lspcheck had sent 41 for it before the notification was read"),
            "{detail}"
        );
        // Read before any version was sent, it names that too.
        let (v, detail) = judge_d2(&d, &[publish(URI, serde_json::json!(41), 5)], None);
        assert_eq!(v, Verdict::Fail);
        assert!(detail.contains("and lspcheck had sent none for it"), "{detail}");
    }

    #[test]
    fn d2_fails_on_a_version_the_document_never_had() {
        let d = [doc(&[(10, 41, "a"), (100, 42, "b")])];
        let (v, detail) = judge_d2(&d, &[publish(URI, serde_json::json!(1), 150)], None);
        assert_eq!(v, Verdict::Fail);
        assert!(
            detail.starts_with("1 diagnostics notification(s) carry a version lspcheck hadn't sent for the document:"),
            "{detail}"
        );
        assert!(detail.contains("carries version 1, and lspcheck had sent 41 and 42"), "{detail}");
        // A failure seen stands even when evidence is incomplete.
        assert_eq!(
            judge_d2(&d, &[publish(URI, serde_json::json!(1), 150)], Some("a frame was skipped")).0,
            Verdict::Fail
        );
        // Several failures are named a few at a time.
        let many: Vec<Published> = (0..5).map(|i| publish(URI, serde_json::json!(i), 150)).collect();
        let (v, detail) = judge_d2(&d, &many, None);
        assert_eq!(v, Verdict::Fail);
        assert!(
            detail.starts_with("5 diagnostics notification(s) carry a version lspcheck hadn't sent for the document:"),
            "{detail}"
        );
        assert!(detail.contains("carries version 0, and lspcheck had sent 41 and 42"), "{detail}");
        assert!(
            detail.ends_with(
                "version 2, and lspcheck had sent 41 and 42 for it before the notification was read; and 2 more"
            ),
            "{detail}"
        );
    }

    #[test]
    fn d2_verdicts_without_a_version_it_can_judge() {
        let d = [doc(&[(10, 41, "a"), (100, 42, "b")])];
        let missed = Some("a frame was skipped");
        // A pass over missed evidence is inconclusive, as for D1.
        let (v, detail) = judge_d2(&d, &[publish(URI, serde_json::json!(41), 50)], missed);
        assert_eq!(v, Verdict::Error);
        assert!(
            detail.starts_with(
                "inconclusive, because a frame was skipped: a versioned diagnostics notification may have been missed"
            ),
            "{detail}"
        );
        assert!(detail.contains("1 versioned diagnostics notification(s) arrived"), "{detail}");
        // No versioned publish for an opened URI: not exercised, with the
        // unversioned ones counted.
        let (v, detail) = judge_d2(&d, &[unversioned(URI, 50)], None);
        assert_eq!(v, Verdict::NotExercised);
        assert!(
            detail.contains("no versioned diagnostics notification for an opened document arrived; 1 without a version, which is lawful"),
            "{detail}"
        );
        // `null`, a string and a float aren't judged: a type error for K2.
        for bad in [Value::Null, serde_json::json!("41"), serde_json::json!(41.5)] {
            let (v, detail) = judge_d2(&d, &[publish(URI, bad, 50)], None);
            assert_eq!(v, Verdict::NotExercised, "{detail}");
            assert!(detail.contains("1 with a version that isn't an integer, which D2 doesn't judge"), "{detail}");
        }
        // Beside a lawful publish they don't keep it from passing.
        let (v, detail) = judge_d2(
            &d,
            &[publish(URI, Value::Null, 50), unversioned(URI, 60), publish(URI, serde_json::json!(41), 70)],
            None,
        );
        assert_eq!(v, Verdict::Pass);
        assert!(detail.contains("named a version lspcheck had sent; 1 without a version, which is lawful; 1 with a version that isn't an integer"), "{detail}");
        // A URI lspcheck never opened isn't judged, whatever its version.
        let (v, detail) = judge_d2(&d, &[publish("file:///w/other.txt", serde_json::json!(999), 50)], None);
        assert_eq!(v, Verdict::NotExercised);
        assert_eq!(detail, "no versioned diagnostics notification for an opened document arrived");
    }

    #[test]
    fn d2_judges_integer_versions_however_they_are_spelled() {
        let d = [doc(&[(10, 41, "a")])];
        let parsed = |text: &str| serde_json::from_str::<Value>(text).unwrap();
        // 41.0 and 4.1e1 are version 41, which was sent.
        for text in ["41.0", "4.1e1", "410e-1"] {
            let (v, detail) = judge_d2(&d, &[publish(URI, parsed(text), 50)], None);
            assert_eq!(v, Verdict::Pass, "{text}: {detail}");
        }
        // 42.0 and 4.2e1 are version 42, which wasn't.
        for text in ["42.0", "4.2e1"] {
            let (v, detail) = judge_d2(&d, &[publish(URI, parsed(text), 50)], None);
            assert_eq!(v, Verdict::Fail, "{text}: {detail}");
            assert!(detail.contains("carries version 42"), "{text}: {detail}");
        }
        assert_eq!(version_number(&parsed("41.5")), None);
    }

    #[test]
    fn d3_matches_uris_after_normalizing_them() {
        // `file:///C:/` and `file:///c:/` (and a %-escaped colon) name the
        // same document.
        let d = [doc(&[(10, 41, "a")])]
            .into_iter()
            .map(|mut d| {
                d.uri = "file:///C:/w/doc.txt".into();
                d
            })
            .collect::<Vec<_>>();
        let (v, _) = judge_d2(&d, &[publish("file:///c%3A/w/doc.txt", serde_json::json!(41), 50)], None);
        assert_eq!(v, Verdict::Pass);
        let (v, _) = judge_d2(&d, &[publish("file:///c:/w/doc.txt", serde_json::json!(999), 50)], None);
        assert_eq!(v, Verdict::Fail);
    }

    fn judged(seen: usize, informative: usize, invalid: &[&str]) -> Judged {
        Judged {
            seen,
            informative,
            invalid: invalid.iter().map(|s| s.to_string()).collect(),
            invalid_positions: invalid.iter().map(|_| Position { line: 0, character: 13 }).collect(),
            ..Judged::default()
        }
    }

    /// A change from `a` to `b`, with the alternatives D3's rule (b) uses.
    fn change() -> Change {
        Change {
            at: Duration::from_millis(100),
            before: "a".into(),
            after: "b".into(),
            alternatives: vec![
                ("a".into(), "the text before the change".into(), Encoding::Utf16),
                ("c".into(), "the change applied counting code points".into(), Encoding::Utf32),
            ],
        }
    }

    #[test]
    fn d3_fails_when_the_position_fits_an_alternative_text() {
        // (b): no informative positions before the change, and the invalid
        // position is not a counting fault: `after` is two emoji, so (0,3) is
        // inside the second in UTF-16, inside the first in UTF-8, and past
        // the end in code points. It is a boundary of the alternative text
        // "abcd", which the change should have produced.
        let mut after = judged(2, 0, &["p"]);
        after.invalid_positions = vec![Position { line: 0, character: 3 }];
        let mut c = change();
        c.after = "\u{1F600}\u{1F600}".into();
        c.alternatives = vec![("abcd".into(), "the text before the change".into(), Encoding::Utf16)];
        let (v, detail) = judge_d3(Encoding::Utf16, &after, &c, None);
        assert_eq!(v, Verdict::Fail);
        assert!(
            detail.starts_with("1 position(s) read after the change fall inside a character of version 42, counted in UTF-16 code units, on no boundary of it counted any other way, and on a boundary of a text the change should have produced"),
            "{detail}"
        );
        assert!(detail.contains("(fits the text before the change)"), "{detail}");
        // A failure seen stands even when evidence after the change is missed.
        assert_eq!(judge_d3(Encoding::Utf16, &after, &c, Some("a frame was skipped")).0, Verdict::Fail);
        // A counting fault: the server applied the change correctly (its text
        // is the changed text) but counted in code points, so it reports
        // `word`'s span as it is in the changed text counted in code points.
        // Here `after` is three emoji, " word ", three emoji, so `word` is at
        // code points 4-7; the server reports that span, and the invalid
        // position (0,4) is explained by the code-point reading: a counting
        // fault, D1's, not a change-handling fault.
        let mut after2 = judged(2, 0, &["p"]);
        after2.invalid_positions = vec![Position { line: 0, character: 4 }];
        after2.all_positions = vec![Position { line: 0, character: 4 }, Position { line: 0, character: 7 }];
        let mut c = change();
        c.after = "\u{1F600}\u{1F600}\u{1F600} word \u{1F600}\u{1F600}\u{1F600}".into();
        c.alternatives = vec![];
        let (v, detail) = judge_d3(Encoding::Utf16, &after2, &c, None);
        assert_eq!(v, Verdict::Error, "{detail}");
        assert!(detail.contains("can't tell a change-handling fault from a counting fault; see D1"), "{detail}");
        // The same position, with `word` reported at some other span: (0,4)
        // is still on a boundary of the changed text counted in code points,
        // so a server that applied the change and counts code points explains
        // it, and (b) doesn't hold.
        let mut after3 = judged(2, 0, &["p"]);
        after3.invalid_positions = vec![Position { line: 0, character: 4 }];
        after3.all_positions = vec![Position { line: 0, character: 5 }, Position { line: 0, character: 8 }];
        let mut c = change();
        c.after = "\u{1F600}\u{1F600}\u{1F600} word \u{1F600}\u{1F600}\u{1F600}".into();
        c.alternatives = vec![(
            "\u{1F600}\u{1F600}\u{1F600}\u{1F600} word".into(),
            "the text before the change".into(),
            Encoding::Utf16,
        )];
        let (v, detail) = judge_d3(Encoding::Utf16, &after3, &c, None);
        assert_eq!(v, Verdict::Error, "{detail}");
        // The reviewer's case: the change applied correctly, then a range
        // (1,0)-(1,1) on the inserted emoji line counted in code points, with
        // no `word` span reported. (1,1) fits the old, ASCII-prefixed line 1,
        // but it is a code-point boundary of the changed text: not (b).
        let mut after4 = judged(2, 0, &["p"]);
        after4.invalid_positions = vec![Position { line: 1, character: 1 }];
        let mut c = change();
        c.after = "x\n\u{1F600}\u{1F600}\n".into();
        c.alternatives = vec![("x\nab\n".into(), "the text before the change".into(), Encoding::Utf16)];
        let (v, detail) = judge_d3(Encoding::Utf16, &after4, &c, None);
        assert_eq!(v, Verdict::Error, "{detail}");
    }

    #[test]
    fn d3_passes_when_everything_after_the_change_is_on_boundaries() {
        // A position valid in 42 but not in 41 (the change moved it) is not
        // a fault: D3 judges only positions invalid in the changed text.
        let after = judged(4, 2, &[]);
        let (v, detail) = judge_d3(Encoding::Utf16, &after, &change(), None);
        assert_eq!(v, Verdict::Pass);
        assert!(
            detail.starts_with(
                "2 informative position(s) on character boundaries after the change, counted in UTF-16 code units"
            ),
            "{detail}"
        );
        // Missed evidence after the change makes a pass inconclusive.
        let (v, detail) = judge_d3(Encoding::Utf16, &after, &change(), Some("a frame was skipped"));
        assert_eq!(v, Verdict::Error);
        assert!(detail.contains("it may have held a position inside a character of the changed text"), "{detail}");
    }

    #[test]
    fn d3_is_not_exercised_without_evidence() {
        // UTF-32: no offset can fall inside a character.
        let (v, detail) = judge_d3(Encoding::Utf32, &judged(4, 2, &[]), &change(), None);
        assert_eq!(v, Verdict::NotExercised);
        assert!(detail.contains("the server chose utf-32"), "{detail}");
        // No informative position judged after the change.
        let (v, detail) = judge_d3(Encoding::Utf16, &judged(4, 0, &[]), &change(), None);
        assert_eq!(v, Verdict::NotExercised);
        assert!(
            detail.contains("no position with non-ASCII text before it on its line was judged after the change"),
            "{detail}"
        );
        // Missed evidence after the change, with nothing informative, is an
        // error: the missed frame may have held an informative position.
        let (v, _) = judge_d3(Encoding::Utf16, &judged(4, 0, &[]), &change(), Some("a frame was skipped"));
        assert_eq!(v, Verdict::Error);
    }

    #[test]
    fn d3_names_the_first_few_invalid_positions_with_their_fits() {
        let after = judged(5, 2, &["p0", "p1", "p2", "p3", "p4"]);
        // (0,13): past the end of seven emoji in code points, inside one in
        // UTF-16 and UTF-8, and a boundary of the ASCII text before.
        let mut c = change();
        c.after = "\u{1F600}".repeat(7);
        c.alternatives = vec![("x".repeat(20), "the text before the change".into(), Encoding::Utf16)];
        let (v, detail) = judge_d3(Encoding::Utf16, &after, &c, None);
        assert_eq!(v, Verdict::Fail);
        assert!(
            detail.starts_with("5 position(s) read after the change fall inside a character of version 42"),
            "{detail}"
        );
        assert!(detail.contains("p0 (fits the text before the change); p1"), "{detail}");
        assert!(detail.ends_with("; and 2 more"), "{detail}");
    }

    #[test]
    fn d3_judges_only_what_can_refer_only_to_the_changed_text() {
        // Version 41 sent at 10 ms, the change to 42 at 100 ms.
        let d = doc(&[(10, 41, "a"), (100, 42, "b")]);
        let at = Duration::from_millis(100);
        assert!(only_after(&d, &reply(vec![], 120, 130), at), "a reply to a request sent after the change");
        assert!(only_after(&d, &diagnostics(vec![], 130, Some(42)), at), "diagnostics labelled 42");
        assert!(!only_after(&d, &reply(vec![], 50, 130), at), "a late reply to a request sent before it");
        assert!(!only_after(&d, &diagnostics(vec![], 130, None), at), "unlabelled diagnostics read after it");
        assert!(!only_after(&d, &diagnostics(vec![], 130, Some(41)), at), "stale diagnostics labelled 41");
        assert!(!only_after(&d, &reply(vec![], 20, 50), at), "a reply read before the change");
    }

    #[test]
    fn the_scenario_change_and_its_alternatives() {
        let before = doc_a();
        // The incremental change, in each encoding, defines the same text.
        for e in [Encoding::Utf8, Encoding::Utf16, Encoding::Utf32] {
            let (changes, after) = scenario_change(&before, e, SyncKind::Incremental);
            assert_eq!(changes.len(), 2);
            assert_eq!(after, DOC_A_AFTER, "the change must not depend on the encoding");
        }
        // c1's range is (0,0)-(0,2) in UTF-16, (0,0)-(0,4) in UTF-8 and
        // (0,0)-(0,1) in UTF-32, computed from the text model.
        let (c1, c2) = incremental_changes(&before, Encoding::Utf16);
        let ContentChange::Text { range, text: t } = c1 else { unreachable!() };
        assert_eq!((range.start, range.end), (Position { line: 0, character: 0 }, Position { line: 0, character: 2 }));
        assert_eq!(t.chars().count(), 4);
        let (c1, _) = incremental_changes(&before, Encoding::Utf8);
        let ContentChange::Text { range, .. } = c1 else { unreachable!() };
        assert_eq!(range.end, Position { line: 0, character: 4 });
        let (c1, _) = incremental_changes(&before, Encoding::Utf32);
        let ContentChange::Text { range, .. } = c1 else { unreachable!() };
        assert_eq!(range.end, Position { line: 0, character: 1 });
        // c2 inserts a line of ten emoji after line 0, of the text after c1.
        let ContentChange::Text { range, text: t } = c2 else { unreachable!() };
        assert_eq!((range.start, range.end), (Position { line: 1, character: 0 }, Position { line: 1, character: 0 }));
        assert_eq!(t, "\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\n");
        // A Full sync sends the whole changed text.
        let (changes, after) = scenario_change(&before, Encoding::Utf16, SyncKind::Full);
        assert_eq!(changes, vec![ContentChange::Full(after.clone())]);
        assert_eq!(after, DOC_A_AFTER);
        // The alternatives: the text before the change, and the change
        // applied counting in each other encoding.
        let alts = alternatives(&before, Encoding::Utf16, SyncKind::Incremental);
        assert_eq!(alts.len(), 3);
        assert_eq!(alts[0], (before.clone(), "the text before the change".to_string(), Encoding::Utf16));
        assert_eq!(alts[1].1, "the change applied counting UTF-8 bytes");
        // Positions in every alternative are counted in the negotiated
        // encoding: only the change's range was miscounted.
        assert_eq!(alts[1].2, Encoding::Utf16);
        assert_eq!(alts[2].1, "the change applied counting code points");
        assert_eq!(alts[2].2, Encoding::Utf16);
        // Counting c1's range in code points replaces two emoji, not one.
        let (c1, c2) = incremental_changes(&before, Encoding::Utf16);
        let mut t = before.clone();
        t = apply_lenient(&t, Encoding::Utf32, &c1).unwrap();
        t = apply_lenient(&t, Encoding::Utf32, &c2).unwrap();
        assert_eq!(t, alts[2].0);
        assert!(t.starts_with(&("\u{1F600}".repeat(8) + " word")), "counting code points replaces two emoji");
        // A position inside an emoji at the start of line 0 is inside a
        // character of every alternative too: no alternative explains it.
        for (alt, name, e) in &alts {
            assert_eq!(text::position_status(alt, *e, 0, 1), PositionStatus::InsideCharacter, "{name}");
        }
        // A Full sync has no range to miscount: only the text before.
        assert_eq!(alternatives(&before, Encoding::Utf16, SyncKind::Full).len(), 1);
    }

    /// Document A after the scenario's change: nine emoji, ` word `, six
    /// emoji on line 0, then a line of ten emoji, then A's lines 1 to 3.
    const DOC_A_AFTER: &str = "\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600} word \u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\n\
                               \u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\n\
                               cafe\u{301} = \"\u{4E2D}\u{6587}\"  # teh teh\n\
                               \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} \u{1F1E6}\u{1F1FA} name \u{1F600}\n\
                               x = 1\n";

    #[test]
    fn sync_kind_reads_the_advertised_change() {
        let caps = |sync: &str| {
            serde_json::from_str(&format!(r#"{{"capabilities": {{"textDocumentSync": {sync}}}}}"#)).unwrap()
        };
        assert_eq!(sync_kind(&caps(r#"2"#)), SyncKind::Incremental);
        assert_eq!(sync_kind(&caps(r#"1"#)), SyncKind::Full);
        assert_eq!(sync_kind(&caps(r#"0"#)), SyncKind::None);
        assert_eq!(sync_kind(&caps(r#"{"openClose": true, "change": 2}"#)), SyncKind::Incremental);
        assert_eq!(sync_kind(&caps(r#"{"openClose": true, "change": 0}"#)), SyncKind::None);
        assert_eq!(sync_kind(&caps(r#"{"openClose": true}"#)), SyncKind::None);
        assert_eq!(sync_kind(&serde_json::json!({"capabilities": {}})), SyncKind::None);
        assert_eq!(sync_kind(&serde_json::json!({"capabilities": {"textDocumentSync": 7}})), SyncKind::None);
        // Integer values however they are spelled: `2.0` and `2e0` are 2,
        // `1.0` is 1, `0.0` is 0, and `2.5` is not an integer.
        assert_eq!(sync_kind(&caps(r#"2.0"#)), SyncKind::Incremental);
        assert_eq!(sync_kind(&caps(r#"1.0"#)), SyncKind::Full);
        assert_eq!(sync_kind(&caps(r#"2e0"#)), SyncKind::Incremental);
        assert_eq!(sync_kind(&caps(r#"0.0"#)), SyncKind::None);
        assert_eq!(sync_kind(&caps(r#"2.5"#)), SyncKind::None);
        assert_eq!(sync_kind(&caps(r#"{"openClose": true, "change": 2.0}"#)), SyncKind::Incremental);
        assert_eq!(sync_kind(&caps(r#"{"openClose": true, "change": 1.0}"#)), SyncKind::Full);
        assert_eq!(sync_kind(&caps(r#"{"openClose": true, "change": 2.5}"#)), SyncKind::None);
    }
}
