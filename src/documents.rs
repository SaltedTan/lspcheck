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

/// The lines of document B, the line-ending document: a lone CR, a CR LF,
/// then LF lines, with a final newline after the last.
///
/// - B0: `x`, ends in a lone `\r`.
/// - B1: eight emoji, ends in `\r\n`.
/// - B2: `word` after nine spaces, ends in `\n`.
/// - B3: eight emoji, ends in `\n`.
/// - B4: `name` after eleven spaces, ends in `\n`.
///
/// Each wrong line split moves the `word` line (and, for two of the three,
/// the `name` line) onto an emoji line, where the odd offsets 9 and 13 fall
/// inside a character in UTF-8 and UTF-16 alike.
pub const DOC_B_LINES: [&str; 5] = [
    "x",
    "\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}",
    // `word` at 9..13 and `name` at 11..15: odd offsets past the end of an
    // 8-emoji line in code points, inside a character of it in UTF-8 and
    // UTF-16. A wrong split puts them on such a line, where no counting of
    // the spec's lines explains them.
    "         word",
    "\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}",
    "           name",
];

/// Document B's text, with its ends of line: B0 ends in a lone `\r`, B1 in
/// `\r\n`, and B2 to B4 in `\n`.
pub fn doc_b() -> String {
    let mut out = String::new();
    for (i, l) in DOC_B_LINES.iter().enumerate() {
        out.push_str(l);
        out.push_str(match i {
            0 => "\r",
            1 => "\r\n",
            _ => "\n",
        });
    }
    out
}

/// A line split the spec doesn't allow, that a server may use anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrongSplit {
    /// Split at `\n` only; a `\r` is an ordinary character.
    LfOnly,
    /// Split at `\n` and `\r\n`, but not a lone `\r`.
    NoLoneCr,
    /// Count `\r\n` as two breaks, so it leaves an empty line between.
    CrLfTwo,
}

/// The name of a wrong split, for a failure detail.
pub fn wrong_split_name(s: WrongSplit) -> &'static str {
    match s {
        WrongSplit::LfOnly => "a line split at `\\n` only",
        WrongSplit::NoLoneCr => "a line split at `\\n` and `\\r\\n`, not a lone `\\r`",
        WrongSplit::CrLfTwo => "`\\r\\n` counted as two breaks",
    }
}

/// The three wrong line splits, in the order a detail names them.
pub const WRONG_SPLITS: [WrongSplit; 3] = [WrongSplit::LfOnly, WrongSplit::NoLoneCr, WrongSplit::CrLfTwo];

/// Splits `text` into lines the way `split` does, without each line's end.
pub fn split_lines_wrong(text: &str, split: WrongSplit) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let is_break = match split {
            WrongSplit::LfOnly => bytes[i] == b'\n',
            // A `\r\n` is one break (at the `\r`); a `\n` not preceded by a `\r`
            // is one; a lone `\r` is not a break.
            WrongSplit::NoLoneCr => {
                (bytes[i] == b'\n' && (i == 0 || bytes[i - 1] != b'\r'))
                    || (bytes[i] == b'\r' && i + 1 < bytes.len() && bytes[i + 1] == b'\n')
            }
            // Every `\r` and `\n` is a break, so a `\r\n` leaves an empty line.
            WrongSplit::CrLfTwo => bytes[i] == b'\n' || bytes[i] == b'\r',
        };
        if is_break {
            out.push(&text[start..i]);
            // Under NoLoneCr a `\r\n` is one break: skip both bytes.
            i += if split == WrongSplit::NoLoneCr && bytes[i] == b'\r' { 2 } else { 1 };
            start = i;
        } else {
            i += 1;
        }
    }
    out.push(&text[start..]);
    out
}

/// Where `(line, character)` falls in `text` split the way `split` does,
/// counted in `encoding`.
pub fn position_status_wrong(
    text: &str,
    split: WrongSplit,
    encoding: Encoding,
    line: usize,
    character: usize,
) -> PositionStatus {
    match split_lines_wrong(text, split).get(line) {
        None => PositionStatus::NoLine,
        Some(line) => match text::offset_to_bytes(line, encoding, character) {
            Ok(_) => PositionStatus::Boundary,
            Err(text::OffsetError::InsideCharacter) => PositionStatus::InsideCharacter,
            Err(text::OffsetError::PastEnd) => PositionStatus::PastEndOfLine,
        },
    }
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
    candidates(doc, source).is_some_and(|c| !c.is_empty() && c.iter().all(|v| v.at >= change_at))
}

/// Forgets what lspcheck read about `uri` from `closed_at` on, when it began
/// closing that document: from then the server may answer about it from
/// disk. Only the positions and publishes about `uri` go; a source keeps
/// its positions about other documents (a diagnostic for an open document
/// may point at a closed one in its related information).
pub fn forget_after_close(sources: &mut [Source], published: &mut Vec<Published>, uri: &str, closed_at: Duration) {
    for s in sources.iter_mut().filter(|s| s.read_at >= closed_at) {
        s.found.retain(|f| !same_uri(&f.uri, uri));
    }
    published.retain(|p| !(p.read_at >= closed_at && same_uri(&p.uri, uri)));
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

/// How many of a document's latest texts the mutation scenario keeps for D1
/// and D6. Older versions keep only their numbers and times.
pub const KEPT_TEXTS: usize = 64;

/// A document lspcheck opened.
#[derive(Debug, Clone, Default)]
pub struct OpenDoc {
    pub uri: String,
    /// The versions sent whose texts lspcheck keeps, oldest first.
    pub versions: Vec<Version>,
    /// The versions sent before those, whose texts lspcheck no longer keeps:
    /// when it began writing each, and its number, oldest first.
    pub dropped: Vec<(Duration, i64)>,
}

impl OpenDoc {
    /// A document opened at `version`.
    pub fn opened(uri: String, version: Version) -> OpenDoc {
        OpenDoc { uri, versions: vec![version], dropped: Vec::new() }
    }

    /// Adds the version just sent, keeping the texts of the latest `keep`
    /// only: an older version keeps its number and time, so D2 still knows
    /// it was sent and D1 that a position may refer to a text it lacks.
    pub fn push(&mut self, version: Version, keep: usize) {
        self.versions.push(version);
        if self.versions.len() > keep {
            let old = self.versions.remove(0);
            self.dropped.push((old.at, old.number));
        }
    }

    /// Every version sent, oldest first: when lspcheck began writing it, and
    /// its number.
    pub fn sent(&self) -> impl Iterator<Item = (Duration, i64)> + '_ {
        self.dropped.iter().copied().chain(self.versions.iter().map(|v| (v.at, v.number)))
    }
}

/// Document B, the line-ending document: its URI and the window it was open.
/// It is never changed, so it has one version and D5 judges against its text.
#[derive(Debug, Clone)]
pub struct DocB {
    pub uri: String,
    /// When lspcheck began writing its `didOpen`.
    pub opened_at: Duration,
    /// When lspcheck began writing its `didClose`, if it sent one.
    pub closed_at: Option<Duration>,
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
    pub label: Option<(String, i64)>,
    /// Where in its scenario it was read, for a failure's detail (the
    /// mutation scenario's case); `None` when the scenario needs none.
    pub place: Option<String>,
}

/// A `textDocument/publishDiagnostics` notification the server sent.
#[derive(Debug, Clone)]
pub struct Published {
    pub uri: String,
    /// The `version` member of the params: `None` when it isn't there.
    pub version: Option<Value>,
    /// When the notification was read.
    pub read_at: Duration,
    /// Where in its scenario it was read, as [`Source::place`].
    pub place: Option<String>,
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

/// The texts a position from `source` about `doc` may refer to (see
/// [`candidates_for`]); `None` when one of them is a text lspcheck no
/// longer keeps.
pub fn candidates<'a>(doc: &'a OpenDoc, source: &Source) -> Option<Vec<&'a Version>> {
    let label = source.label.as_ref().filter(|(uri, _)| same_uri(uri, &doc.uri)).map(|(_, version)| *version);
    candidates_for(doc, label, source.asked_at, source.read_at)
}

/// The texts something about `doc` read at `read_at` may refer to. A
/// `publishDiagnostics` labelled with a version lspcheck sent refers to that
/// version. A reply (to a request sent at `asked_at`) refers to the version
/// current when its request was sent, or to one sent before it was read,
/// should a change have overtaken it. Any other notification may refer to
/// any version sent before it was read. `None` when one of those versions'
/// texts was dropped ([`OpenDoc::dropped`]): judging the others alone could
/// blame a position on a text it doesn't refer to.
pub fn candidates_for(
    doc: &OpenDoc,
    label: Option<i64>,
    asked_at: Option<Duration>,
    read_at: Duration,
) -> Option<Vec<&Version>> {
    // Every version sent, dropped ones first, by index.
    let dropped = doc.dropped.len();
    let count = dropped + doc.versions.len();
    let sent_at = |i: usize| {
        if i < dropped { doc.dropped[i] } else { (doc.versions[i - dropped].at, doc.versions[i - dropped].number) }
    };
    let text = |i: usize| i.checked_sub(dropped).map(|k| &doc.versions[k]);
    let sent = |i: &usize| sent_at(*i).0 <= read_at;
    if let Some(n) = label
        && let Some(i) = (0..count).filter(sent).find(|i| sent_at(*i).1 == n)
    {
        return text(i).map(|v| vec![v]);
    }
    let from = match asked_at {
        Some(asked) => (0..count).rposition(|i| sent_at(i).0 <= asked).unwrap_or(0),
        None => 0,
    };
    (from..count).filter(sent).map(text).collect()
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
    /// Positions that may refer to a text lspcheck no longer keeps, which
    /// aren't judged.
    pub evicted: usize,
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
        judge_source(&mut j, encoding, docs, source);
    }
    j
}

/// Adds what every position in `source` about a document in `docs` shows,
/// counted in `encoding`, to `j`.
pub fn judge_source(j: &mut Judged, encoding: Encoding, docs: &[OpenDoc], source: &Source) {
    for found in &source.found {
        if found.kind == FoundKind::Line {
            continue;
        }
        let Some(doc) = docs.iter().find(|d| same_uri(&d.uri, &found.uri)) else {
            j.elsewhere += found.at.len();
            continue;
        };
        let Some(texts) = candidates(doc, source) else {
            j.evicted += found.at.len();
            continue;
        };
        if texts.is_empty() {
            j.before_open += found.at.len();
            continue;
        }
        let ends: &[&str] = if found.kind == FoundKind::Range { &[".start", ".end"] } else { &[""] };
        for (p, end) in found.at.iter().zip(ends) {
            j.seen += 1;
            let path = format!("{}{end}", found.pointer);
            let mut where_ = format!("{} {}", source.method, path.trim_start_matches('.'));
            if let Some(place) = &source.place {
                where_ = format!("{place}: {where_}");
            }
            judge_one(j, encoding, &texts, *p, &where_);
        }
    }
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
    if j.evicted > 0 {
        out.push_str(&format!(
            ", {} that may refer to a version older than the {KEPT_TEXTS} whose texts lspcheck keeps, not judged",
            j.evicted
        ));
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

/// D5's verdict, judged at the end over the positions read while document B
/// was open, against B's text (which is never changed).
///
/// B's lines are split the spec's way, at `\n`, `\r\n` and `\r`. A position
/// about B is invalid when it falls inside a character of B in the
/// negotiated encoding. D5 fails only when such a position is explained by a
/// line split the spec doesn't allow, and by no reading of B: it is on no
/// boundary of B counted in any other encoding (a counting fault is D1's),
/// and it is on a boundary of B under one of the three wrong splits, counted
/// in the negotiated encoding. A position invalid under the spec's split but
/// explained by no wrong split is an `error`, since it can't be told from a
/// counting fault. `incomplete` says why lspcheck may have missed something
/// the server sent while B was open.
pub fn judge_d5(encoding: Encoding, text: &str, j: &Judged, incomplete: Option<&str>) -> (Verdict, String) {
    let units = counted_in(encoding);
    let mut fits: Vec<(usize, String)> = Vec::new();
    for (i, p) in j.invalid_positions.iter().enumerate() {
        // A boundary of B in any other encoding, under the spec's split, is a
        // counting fault (D1's), not a line-splitting fault.
        let reread = ENCODINGS.iter().any(|e| {
            *e != encoding && text::position_status(text, *e, p.line, p.character) == PositionStatus::Boundary
        });
        if reread {
            continue;
        }
        // Every wrong split it fits: on B's ASCII lines several may, and
        // the detail shouldn't single one out.
        let names: Vec<&str> = WRONG_SPLITS
            .iter()
            .filter(|s| position_status_wrong(text, **s, encoding, p.line, p.character) == PositionStatus::Boundary)
            .map(|s| wrong_split_name(*s))
            .collect();
        if !names.is_empty() {
            fits.push((i, names.join(", or ")));
        }
    }
    if !fits.is_empty() {
        // A failure that was seen stands, whatever else was missed.
        let more = fits.len().saturating_sub(NAMED);
        let tail = if more > 0 { format!("; and {more} more") } else { String::new() };
        let first: Vec<String> =
            fits.iter().take(NAMED).map(|(i, name)| format!("{} (fits {name})", j.invalid[*i])).collect();
        return (
            Verdict::Fail,
            format!(
                "{} position(s) about the line-ending document fall inside a character of it, counted in {units}, \
                 on no boundary of it counted any other way, and on a boundary of it under a line split the spec \
                 doesn't allow: {}{tail}",
                fits.len(),
                first.join("; ")
            ),
        );
    }
    if !j.invalid.is_empty() {
        return (
            Verdict::Error,
            format!(
                "{} position(s) about the line-ending document fall inside a character of it, counted in {units}, \
                 but can't tell a line-splitting fault from a counting fault; see D1",
                j.invalid.len()
            ),
        );
    }
    if encoding == Encoding::Utf32 {
        return (
            Verdict::NotExercised,
            format!(
                "the server chose utf-32, in which no offset can fall inside a character; {} position(s) about the line-ending document",
                j.seen
            ),
        );
    }
    // What lspcheck missed may have held positions, informative ones included.
    if let Some(why) = incomplete {
        return (
            Verdict::Error,
            format!(
                "inconclusive, because {why}: it may have held a position inside a character of the line-ending document; \
                 {} position(s) about it",
                j.seen
            ),
        );
    }
    // B's first line is ASCII, so no position on it is informative; the count
    // is exactly the informative positions on lines 1 to 4, the lines after a
    // `\r` or `\r\n`.
    if j.informative == 0 {
        return (
            Verdict::NotExercised,
            format!(
                "no position with non-ASCII text before it on its line was judged about the line-ending document; {} position(s) about it",
                j.seen
            ),
        );
    }
    (
        Verdict::Pass,
        format!(
            "{} informative position(s) on character boundaries about the line-ending document, counted in {units}; {} position(s) about it",
            j.informative, j.seen
        ),
    )
}

// --- D6: selection-range containment ---------------------------------------

/// The bound on a selection range's parent chain: a chain walked this deep is
/// cut short and counted, not walked further.
const SELECTION_CHAIN_BOUND: usize = 1000;

/// What D6 saw in one `textDocument/selectionRange` reply with an array
/// result.
#[derive(Debug, Clone, Default)]
pub struct SelectionReply {
    /// The entries with a `range`, judged for containment.
    pub entries: usize,
    /// The failures, described.
    pub failures: Vec<String>,
    /// Entries that are not objects, have no `range`, or carry a `parent`
    /// that isn't an object: a type error for a later check, not judged here.
    pub malformed: usize,
    /// Ranges on a line the text doesn't have, not judged.
    pub no_line: usize,
    /// Parent chains cut short at the bound.
    pub cut_chains: usize,
    /// Containment comparisons actually made (a position in its range, or a
    /// child in its parent): the evidence a pass needs.
    pub compared: usize,
    /// Replies whose containment wasn't judged because a text they may
    /// describe is one lspcheck no longer keeps (their entry count was).
    pub evicted: usize,
}

/// The `positions` of a `textDocument/selectionRange` request's params.
pub fn request_positions(params: &Value) -> Vec<Position> {
    params
        .get("positions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| {
            Some(Position {
                line: p.get("line")?.as_u64()? as usize,
                character: p.get("character")?.as_u64()? as usize,
            })
        })
        .collect()
}

/// A `Range` from a JSON value: `start` and `end` positions, each a non-
/// negative integer `line` and `character`.
fn as_range(v: Option<&Value>) -> Option<Range> {
    let v = v?;
    let pos = |name: &str| -> Option<Position> {
        let p = v.get(name)?;
        Some(Position { line: p.get("line")?.as_u64()? as usize, character: p.get("character")?.as_u64()? as usize })
    };
    Some(Range { start: pos("start")?, end: pos("end")? })
}

/// A position clamped to its line's length, as a client would; `None` for a
/// line the text doesn't have.
fn clamped(lines: &[&str], encoding: Encoding, p: Position) -> Option<Position> {
    let line = lines.get(p.line)?;
    Some(Position { line: p.line, character: p.character.min(text::line_length(line, encoding)) })
}

/// Whether `outer` contains `inner`, both clamped to their lines' lengths in
/// `encoding`; `None` when a line of either range doesn't exist (not judged).
fn contains_range(lines: &[&str], encoding: Encoding, outer: Range, inner: Range) -> Option<bool> {
    let os = clamped(lines, encoding, outer.start)?;
    let oe = clamped(lines, encoding, outer.end)?;
    let is = clamped(lines, encoding, inner.start)?;
    let ie = clamped(lines, encoding, inner.end)?;
    Some((os.line, os.character) <= (is.line, is.character) && (ie.line, ie.character) <= (oe.line, oe.character))
}

/// A text a selection reply may describe, with its lines.
type Described<'a> = (&'a str, Vec<&'a str>);

/// Whether `outer` contains `inner` in a text the reply may describe, each
/// with its own clamping: `Some(true)` when it does in one, `Some(false)`
/// when it doesn't in any (and every line exists in each), `None` otherwise
/// (not judged), as D1 judges a position against its candidate texts.
fn contains_in_any(texts: &[Described], encoding: Encoding, outer: Range, inner: Range) -> Option<bool> {
    let each: Vec<Option<bool>> =
        texts.iter().map(|(_, lines)| contains_range(lines, encoding, outer, inner)).collect();
    if each.contains(&Some(true)) {
        Some(true)
    } else if !each.is_empty() && each.iter().all(|c| *c == Some(false)) {
        Some(false)
    } else {
        None
    }
}

/// The hint a D6 failure gets when the same comparison holds under a line
/// split the spec doesn't allow, in a text the reply may describe: names
/// every such split, as D5 does. Empty when none explains the failure.
fn wrong_split_hint(texts: &[Described], encoding: Encoding, outer: Range, inner: Range) -> String {
    let names: Vec<&str> = WRONG_SPLITS
        .iter()
        .filter(|s| {
            texts.iter().any(|(text, _)| {
                let lines = split_lines_wrong(text, **s);
                contains_range(&lines, encoding, outer, inner) == Some(true)
            })
        })
        .map(|s| wrong_split_name(*s))
        .collect();
    if names.is_empty() { String::new() } else { format!(" (holds under {}; see D5)", names.join(", or ")) }
}

/// A range, for a failure detail.
fn fmt_range(r: Range) -> String {
    format!("({},{})-({},{})", r.start.line, r.start.character, r.end.line, r.end.character)
}

/// Walks one entry's parent chain, judging the first node's range against the
/// position and every parent's range against its child's, in the texts the
/// reply may describe (`version` names them).
fn judge_chain(
    texts: &[Described],
    encoding: Encoding,
    version: &str,
    entry_idx: usize,
    position: Option<Position>,
    node: &Value,
    r: &mut SelectionReply,
) {
    let mut depth = 0;
    let mut current = node;
    loop {
        // The current node must be an object with a valid `range`.
        let Some(rng) = current.as_object().and_then(|o| as_range(o.get("range"))) else {
            r.malformed += 1;
            return;
        };
        if depth == 0 {
            r.entries += 1;
            // The first node's range must contain the position.
            if let Some(p) = position {
                match contains_in_any(texts, encoding, rng, Range { start: p, end: p }) {
                    Some(true) => r.compared += 1,
                    Some(false) => r.failures.push(format!(
                        "selectionRange at version {version}, entry {entry_idx}: range {} doesn't contain the position ({}, {}){}",
                        fmt_range(rng),
                        p.line,
                        p.character,
                        wrong_split_hint(texts, encoding, rng, Range { start: p, end: p })
                    )),
                    None => r.no_line += 1,
                }
            }
        }
        // Move to the parent.
        let Some(parent) = current.get("parent") else { return };
        // The parent must be an object with a valid `range`.
        if !parent.is_object() || as_range(parent.get("range")).is_none() {
            r.malformed += 1;
            return;
        }
        let parent_range = as_range(parent.get("range")).expect("checked above");
        // The parent's range must contain the child's.
        match contains_in_any(texts, encoding, parent_range, rng) {
            Some(true) => r.compared += 1,
            Some(false) => r.failures.push(format!(
                "selectionRange at version {version}, entry {entry_idx}: parent {} doesn't contain {}{}",
                fmt_range(parent_range),
                fmt_range(rng),
                wrong_split_hint(texts, encoding, parent_range, rng)
            )),
            None => r.no_line += 1,
        }
        current = parent;
        depth += 1;
        if depth >= SELECTION_CHAIN_BOUND {
            r.cut_chains += 1;
            return;
        }
    }
}

/// Judges one `textDocument/selectionRange` reply's array result against the
/// request's `positions`, in `text` counted in `encoding`. The array length
/// must equal the number of positions; each entry's range must contain its
/// position, and every parent's range must contain its child's, all the way
/// up the chain. `None` when the result isn't an array (a type error for a
/// later check, not judged here).
pub fn judge_selection_reply(
    text: &str,
    encoding: Encoding,
    version: i64,
    positions: &[Position],
    result: &Value,
) -> Option<SelectionReply> {
    judge_selection(Some(&[text]), encoding, &version.to_string(), positions, result)
}

/// [`judge_selection_reply`] with D1's candidate rule: `texts` are the
/// versions the reply may describe ([`candidates_for`]), and a containment
/// fails only when it fails in every one, each with its own clamping (a reply
/// may lawfully describe a later, shorter text). With `None`, one of them is
/// a text lspcheck no longer keeps: only the entry count is judged, since one
/// entry per position doesn't depend on the text. `version` names the
/// request's version in that case.
pub fn judge_selection_candidates(
    texts: Option<&[&Version]>,
    encoding: Encoding,
    version: i64,
    positions: &[Position],
    result: &Value,
) -> Option<SelectionReply> {
    match texts {
        Some(texts) => {
            let numbers: Vec<String> = texts.iter().map(|v| v.number.to_string()).collect();
            let texts: Vec<&str> = texts.iter().map(|v| v.text.as_str()).collect();
            judge_selection(Some(&texts), encoding, &numbers.join(" or "), positions, result)
        }
        None => judge_selection(None, encoding, &format!("{version} or later"), positions, result),
    }
}

fn judge_selection(
    texts: Option<&[&str]>,
    encoding: Encoding,
    version: &str,
    positions: &[Position],
    result: &Value,
) -> Option<SelectionReply> {
    let entries = result.as_array()?;
    let mut r = SelectionReply::default();
    if entries.len() != positions.len() {
        r.failures.push(format!(
            "selectionRange at version {version}: {} entries for {} positions",
            entries.len(),
            positions.len()
        ));
    }
    let Some(texts) = texts else {
        r.evicted += 1;
        return Some(r);
    };
    let texts: Vec<Described> = texts.iter().map(|t| (*t, text::split_lines(t))).collect();
    for (i, entry) in entries.iter().enumerate() {
        judge_chain(&texts, encoding, version, i, positions.get(i).copied(), entry, &mut r);
    }
    Some(r)
}

/// The extras a D6 detail gives, after its main clause.
fn selection_extras(malformed: usize, no_line: usize, cut_chains: usize, evicted: usize) -> String {
    let mut out = String::new();
    if malformed > 0 {
        out.push_str(&format!(", {malformed} malformed entr(y/ies), which D6 doesn't judge"));
    }
    if no_line > 0 {
        out.push_str(&format!(", {no_line} range(s) on a line the text doesn't have, not judged"));
    }
    if cut_chains > 0 {
        out.push_str(&format!(", {cut_chains} parent chain(s) cut short at the bound"));
    }
    out.push_str(&selection_evicted(evicted));
    out
}

/// What a D6 detail says of replies about texts lspcheck no longer keeps.
fn selection_evicted(evicted: usize) -> String {
    if evicted == 0 {
        return String::new();
    }
    format!(
        ", {evicted} reply(s) that may describe a version older than the {KEPT_TEXTS} whose texts lspcheck keeps, \
         whose containment wasn't judged"
    )
}

/// D6's verdict over the selectionRange replies judged. A failure seen stands
/// even when evidence is incomplete; a pass over missed evidence is an error;
/// no reply with an array result is not exercised.
pub fn judge_d6(replies: &[SelectionReply], incomplete: Option<&str>) -> (Verdict, String) {
    let mut failures: Vec<String> = Vec::new();
    let (mut judged, mut entries, mut malformed, mut no_line, mut cut_chains, mut compared, mut evicted) =
        (0usize, 0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    for r in replies {
        judged += 1;
        compared += r.compared;
        entries += r.entries;
        malformed += r.malformed;
        no_line += r.no_line;
        cut_chains += r.cut_chains;
        evicted += r.evicted;
        failures.extend(r.failures.iter().cloned());
    }
    if !failures.is_empty() {
        // A failure that was seen stands, whatever else was missed.
        let more = failures.len().saturating_sub(NAMED);
        let tail = if more > 0 { format!("; and {more} more") } else { String::new() };
        let first: Vec<&str> = failures.iter().take(NAMED).map(String::as_str).collect();
        return (
            Verdict::Fail,
            format!(
                "{} selectionRange finding(s): {}{tail}; {} reply(s) with {} entries judged, {} malformed, {} range(s) on a line the text doesn't have, {} parent chain(s) cut short{}",
                failures.len(),
                first.join("; "),
                judged,
                entries,
                malformed,
                no_line,
                cut_chains,
                selection_evicted(evicted)
            ),
        );
    }
    let extras = selection_extras(malformed, no_line, cut_chains, evicted);
    if let Some(why) = incomplete {
        return (
            Verdict::Error,
            format!(
                "inconclusive, because {why}: a selectionRange reply may have been missed; {} reply(s) with {} entries judged{extras}",
                judged, entries
            ),
        );
    }
    if judged == 0 {
        return (Verdict::NotExercised, format!("no selectionRange reply with an array result was judged{extras}"));
    }
    // Replies whose entries were all malformed or on missing lines gave
    // nothing to compare: no evidence either way.
    if compared == 0 {
        return (
            Verdict::NotExercised,
            format!(
                "{judged} selectionRange reply(s), but no range could be compared with its position or parent{extras}"
            ),
        );
    }
    (
        Verdict::Pass,
        format!(
            "{} selectionRange reply(s) with {} entries judged: every range contains its position, and every parent contains its child{extras}",
            judged, entries
        ),
    )
}

/// Version numbers for a detail, joined by "and", with a run of three or
/// more consecutive ones written as its first and last ("1 to 80").
fn version_list(numbers: &[i64]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < numbers.len() {
        let mut j = i;
        while j + 1 < numbers.len() && numbers[j + 1] == numbers[j] + 1 {
            j += 1;
        }
        if j >= i + 2 {
            parts.push(format!("{} to {}", numbers[i], numbers[j]));
        } else {
            parts.extend(numbers[i..=j].iter().map(i64::to_string));
        }
        i = j + 1;
    }
    parts.join(" and ")
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
        // for this URI by the time the notification was read, including
        // versions whose texts it no longer keeps.
        let sent: Vec<i64> = doc.sent().filter(|(at, _)| *at <= p.read_at).map(|(_, n)| n).collect();
        if sent.contains(&n) {
            checked += 1;
            continue;
        }
        let had = if sent.is_empty() { "none".to_string() } else { version_list(&sent) };
        let place = p.place.as_ref().map(|place| format!("{place}: ")).unwrap_or_default();
        fails.push(format!(
            "{place}{} carries version {}, and lspcheck had sent {had} for it before the notification was read",
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
            dropped: Vec::new(),
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
            place: None,
        }
    }

    fn diagnostics(found: Vec<Found>, read: u64, label: Option<i64>) -> Source {
        Source {
            method: "textDocument/publishDiagnostics".into(),
            found,
            asked_at: None,
            read_at: Duration::from_millis(read),
            label: label.map(|version| (URI.into(), version)),
            place: None,
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
        let nums = |s: &Source| candidates(&d, s).unwrap().iter().map(|v| v.number).collect::<Vec<_>>();
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
    fn review_regression_diagnostics_labels_apply_only_to_their_uri() {
        let a = doc(&[(10, 2, "😀abc"), (20, 3, "abc")]);
        let mut b = doc(&[(10, 2, "😀abc"), (20, 3, "abc")]);
        b.uri = "file:///w/other.txt".into();
        let params = serde_json::json!({
            "uri": URI, "version": 2,
            "diagnostics": [{
                "range": {"start": {"line": 0, "character": 2}, "end": {"line": 0, "character": 2}},
                "message": "example",
                "relatedInformation": [{
                    "location": {"uri": "file:///w/%6fther.txt", "range": {
                        "start": {"line": 0, "character": 1}, "end": {"line": 0, "character": 1}
                    }}, "message": "related example"
                }]
            }]
        });
        let source = diagnostics(text::positions("textDocument/publishDiagnostics", &params, None), 30, Some(2));
        let j = judge_positions(Encoding::Utf16, &[a.clone(), b.clone()], std::slice::from_ref(&source));
        assert_eq!((j.valid, j.invalid.len(), j.evicted), (4, 0, 0));
        let primary = diagnostics(vec![range(URI, (0, 1), (0, 1))], 30, Some(2));
        let j = judge_positions(Encoding::Utf16, std::slice::from_ref(&a), &[primary]);
        assert_eq!((j.valid, j.invalid.len()), (0, 2));
        b.push(Version { at: Duration::from_millis(25), number: 4, text: "abc".into() }, 1);
        let j = judge_positions(Encoding::Utf16, &[a, b], &[source]);
        assert_eq!((j.valid, j.invalid.len(), j.evicted), (2, 0, 2));
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
        Published { uri: uri.into(), version: Some(version), read_at: Duration::from_millis(read), place: None }
    }

    fn unversioned(uri: &str, read: u64) -> Published {
        Published { uri: uri.into(), version: None, read_at: Duration::from_millis(read), place: None }
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
    fn what_is_about_a_closed_document_is_forgotten_position_by_position() {
        let b = "file:///w/crlf.txt";
        let at = Duration::from_millis(100);
        // A diagnostic for open A, read after B's close, with related
        // information in B: A's position stays, B's goes.
        let mut sources = vec![
            diagnostics(vec![range(URI, (1, 11), (1, 12)), range(b, (0, 0), (0, 1))], 150, None),
            diagnostics(vec![range(b, (1, 1), (1, 2))], 50, None),
        ];
        let mut published = vec![
            Published {
                uri: b.into(),
                version: Some(serde_json::json!(7)),
                read_at: Duration::from_millis(150),
                place: None,
            },
            Published {
                uri: b.into(),
                version: Some(serde_json::json!(7)),
                read_at: Duration::from_millis(50),
                place: None,
            },
            Published { uri: URI.into(), version: None, read_at: Duration::from_millis(150), place: None },
        ];
        forget_after_close(&mut sources, &mut published, b, at);
        assert_eq!(sources[0].found.len(), 1);
        assert_eq!(sources[0].found[0].uri, URI);
        assert_eq!(sources[1].found.len(), 1, "read before the close: kept");
        assert_eq!(published.len(), 2);
        assert!(published.iter().all(|p| p.uri == URI || p.read_at < at));
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

    // --- D5: the line-ending document -------------------------------------

    fn judged_d5(invalid: &[(usize, usize)], informative: usize, seen: usize) -> Judged {
        let positions: Vec<Position> = invalid.iter().map(|(l, c)| Position { line: *l, character: *c }).collect();
        Judged {
            seen,
            elsewhere: 0,
            valid: seen - invalid.len(),
            informative,
            invalid: invalid.iter().map(|(l, c)| format!("{l}:{c}")).collect(),
            invalid_positions: positions.clone(),
            all_positions: positions,
            past_end: 0,
            past_end_fits: Vec::new(),
            no_line: 0,
            before_open: 0,
            evicted: 0,
        }
    }

    #[test]
    fn doc_b_has_the_briefs_lines_and_ends() {
        let text = doc_b();
        let lines = text::split_lines(&text);
        assert_eq!(lines.len(), 6);
        assert_eq!(lines[0], "x");
        assert_eq!(lines[1], "\u{1F600}".repeat(8));
        assert_eq!(lines[2], format!("{}word", " ".repeat(9)));
        assert_eq!(lines[3], "\u{1F600}".repeat(8));
        assert_eq!(lines[4], format!("{}name", " ".repeat(11)));
        assert_eq!(lines[5], "");
        // The ends: B0 lone `\r`, B1 `\r\n`, B2 to B4 `\n`.
        assert!(text.starts_with("x\r"));
        assert!(text.contains(&format!("{}\r\n", "\u{1F600}".repeat(8))));
        assert!(text.contains(" word\n"));
        assert!(text.ends_with(" name\n"));
    }

    #[test]
    fn doc_b_word_and_name_map_onto_emoji_lines() {
        let text = doc_b();
        let spec = text::split_lines(&text);
        let emoji = |s: &str| !s.is_empty() && s.chars().all(|c| c == '\u{1F600}');
        // Under each wrong split, `word` sits on a line whose spec-split
        // counterpart is an emoji line; `name` does too, except under
        // `\r\n`-as-two, where it lands on the trailing empty line.
        for &split in &WRONG_SPLITS {
            let lines = split_lines_wrong(&text, split);
            let word_line = lines.iter().position(|l| l.contains("word")).unwrap();
            let name_line = lines.iter().position(|l| l.contains("name")).unwrap();
            assert!(emoji(spec[word_line]), "word: spec line {word_line} is not emoji under {split:?}");
            if split != WrongSplit::CrLfTwo {
                assert!(emoji(spec[name_line]), "name: spec line {name_line} is not emoji under {split:?}");
            }
            // `word`'s offsets (9 and 13), on the emoji line the spec split
            // puts them on, are inside a character in UTF-16 and UTF-8 and past
            // the end in code points: no counting of the spec's lines explains
            // them. On the wrong split's own line they are boundaries.
            for off in [9usize, 13] {
                for e in [Encoding::Utf16, Encoding::Utf8] {
                    assert_eq!(
                        text::position_status(spec[word_line], e, 0, off),
                        PositionStatus::InsideCharacter,
                        "word offset {off} in {e:?} under {split:?}"
                    );
                }
                assert_eq!(
                    text::position_status(spec[word_line], Encoding::Utf32, 0, off),
                    PositionStatus::PastEndOfLine
                );
                assert_eq!(text::position_status(lines[word_line], Encoding::Utf16, 0, off), PositionStatus::Boundary);
            }
        }
    }

    #[test]
    fn d5_clean_passes() {
        let j = judged_d5(&[], 4, 4);
        let (v, d) = judge_d5(Encoding::Utf16, &doc_b(), &j, None);
        assert_eq!(v, Verdict::Pass);
        assert!(d.contains("informative"), "{d}");
    }

    #[test]
    fn d5_counting_fault_is_an_error() {
        // (1,1) is inside a character of the emoji line in UTF-16, but a boundary
        // of it in UTF-32: a counting fault, not a line-splitting fault.
        let j = judged_d5(&[(1, 1)], 3, 4);
        let (v, d) = judge_d5(Encoding::Utf16, &doc_b(), &j, None);
        assert_eq!(v, Verdict::Error);
        assert!(d.contains("can't tell"), "{d}");
    }

    #[test]
    fn d5_utf32_is_not_exercised() {
        let j = judged_d5(&[], 4, 4);
        let (v, d) = judge_d5(Encoding::Utf32, &doc_b(), &j, None);
        assert_eq!(v, Verdict::NotExercised);
        assert!(d.contains("utf-32"), "{d}");
    }

    #[test]
    fn d5_no_informative_position_is_not_exercised() {
        let j = judged_d5(&[], 0, 2);
        let (v, d) = judge_d5(Encoding::Utf16, &doc_b(), &j, None);
        assert_eq!(v, Verdict::NotExercised);
        assert!(d.contains("no position"), "{d}");
    }

    #[test]
    fn wrong_splits_split_as_named() {
        let text = "a\rb\r\nc\nd";
        assert_eq!(split_lines_wrong(text, WrongSplit::LfOnly), vec!["a\rb\r", "c", "d"]);
        assert_eq!(split_lines_wrong(text, WrongSplit::NoLoneCr), vec!["a\rb", "c", "d"]);
        assert_eq!(split_lines_wrong(text, WrongSplit::CrLfTwo), vec!["a", "b", "", "c", "d"]);
    }

    #[test]
    fn d5_wrong_splits_fail_on_bs_own_evidence() {
        // `word` reported at 9..13 on the spec's emoji lines 1 and 3, as each
        // wrong split puts it, is explained by a wrong split and by no counting
        // of the spec's lines: D5 fails.
        let text = doc_b();
        let j = judged_d5(&[(1, 9), (1, 13), (3, 9), (3, 13)], 0, 4);
        let (v, d) = judge_d5(Encoding::Utf16, &text, &j, None);
        assert_eq!(v, Verdict::Fail, "{d}");
        assert!(d.contains("fits"), "{d}");
        // Offsets 1 and 5 on the same lines are in-line code-point offsets: a
        // counting fault may explain them, so D5 can't blame the split.
        let j = judged_d5(&[(1, 1), (1, 5), (3, 1), (3, 5)], 0, 4);
        let (v, d) = judge_d5(Encoding::Utf16, &text, &j, None);
        assert_eq!(v, Verdict::Error, "{d}");
        assert!(d.contains("can't tell"), "{d}");
    }

    // --- D6: selection-range containment ------------------------------------

    fn sel_positions(pairs: &[(usize, usize)]) -> Vec<Position> {
        pairs.iter().map(|(l, c)| Position { line: *l, character: *c }).collect()
    }

    /// A range value from line/character pairs.
    fn rng(sl: usize, sc: usize, el: usize, ec: usize) -> Value {
        serde_json::json!({"start": {"line": sl, "character": sc}, "end": {"line": el, "character": ec}})
    }

    /// A selection entry: a range, optionally with a parent entry.
    fn entry(range: Value, parent: Option<Value>) -> Value {
        match parent {
            Some(p) => serde_json::json!({"range": range, "parent": p}),
            None => serde_json::json!({"range": range}),
        }
    }

    #[test]
    fn d6_a_range_must_contain_its_position() {
        let text = "hello world\nfoo bar\n";
        let positions = sel_positions(&[(0, 6)]);
        // The range contains the position: pass.
        let r = judge_selection_reply(
            text,
            Encoding::Utf16,
            41,
            &positions,
            &serde_json::json!([entry(rng(0, 6, 0, 11), None)]),
        )
        .unwrap();
        assert!(r.failures.is_empty(), "{r:?}");
        // Before the start: fail.
        let r = judge_selection_reply(
            text,
            Encoding::Utf16,
            41,
            &positions,
            &serde_json::json!([entry(rng(0, 0, 0, 5), None)]),
        )
        .unwrap();
        assert_eq!(r.failures.len(), 1, "{r:?}");
        assert!(r.failures[0].contains("doesn't contain the position (0, 6)"), "{r:?}");
        // After the end: fail.
        let r = judge_selection_reply(
            text,
            Encoding::Utf16,
            41,
            &positions,
            &serde_json::json!([entry(rng(0, 7, 0, 11), None)]),
        )
        .unwrap();
        assert_eq!(r.failures.len(), 1, "{r:?}");
        // On another line: fail.
        let r = judge_selection_reply(
            text,
            Encoding::Utf16,
            41,
            &positions,
            &serde_json::json!([entry(rng(1, 0, 1, 3), None)]),
        )
        .unwrap();
        assert_eq!(r.failures.len(), 1, "{r:?}");
    }

    #[test]
    fn d6_names_the_line_split_that_would_explain_the_failure() {
        // zls 0.17.0's reply about B: one range for the whole document as it
        // splits lines at `\n` only, so (4, 15) is the empty last line's end.
        let positions = sel_positions(&[(2, 9), (2, 11), (2, 13), (4, 15)]);
        let whole = || entry(rng(0, 0, 4, 0), None);
        let result = serde_json::json!([whole(), whole(), whole(), whole()]);
        let r = judge_selection_reply(&doc_b(), Encoding::Utf16, 7, &positions, &result).unwrap();
        assert_eq!(r.failures.len(), 1, "{r:?}");
        let failure = &r.failures[0];
        assert!(failure.contains("doesn't contain the position (4, 15)"), "{failure}");
        // A split at `\n` only or at `\n` and `\r\n` leaves line 4 empty,
        // where both the position and the range's end clamp to (4, 0).
        for split in [WrongSplit::LfOnly, WrongSplit::NoLoneCr] {
            assert!(failure.contains(wrong_split_name(split)), "{failure}");
        }
        // Counting `\r\n` as two breaks puts the emoji line at 4, past the
        // range's end.
        assert!(!failure.contains(wrong_split_name(WrongSplit::CrLfTwo)), "{failure}");
        assert!(failure.ends_with("; see D5)"), "{failure}");
        let (v, detail) = judge_d6(std::slice::from_ref(&r), None);
        assert_eq!(v, Verdict::Fail, "{detail}");
        assert!(detail.contains("see D5"), "{detail}");
    }

    #[test]
    fn d6_a_parent_failure_gets_the_same_hint() {
        // The child contains its position under the spec's split; the parent
        // ends at (4, 0), before the child's end (4, 15). A split that leaves
        // line 4 empty clamps both ends to (4, 0), so the parent contains it.
        let positions = sel_positions(&[(4, 15)]);
        let result = serde_json::json!([entry(rng(0, 0, 4, 15), Some(entry(rng(0, 0, 4, 0), None)))]);
        let r = judge_selection_reply(&doc_b(), Encoding::Utf16, 7, &positions, &result).unwrap();
        assert_eq!(r.failures.len(), 1, "{r:?}");
        let failure = &r.failures[0];
        assert!(failure.contains("parent (0,0)-(4,0) doesn't contain (0,0)-(4,15)"), "{failure}");
        for split in [WrongSplit::LfOnly, WrongSplit::NoLoneCr] {
            assert!(failure.contains(wrong_split_name(split)), "{failure}");
        }
        assert!(!failure.contains(wrong_split_name(WrongSplit::CrLfTwo)), "{failure}");
        assert!(failure.ends_with("; see D5)"), "{failure}");
    }

    #[test]
    fn d6_hints_only_when_a_wrong_split_explains_the_failure() {
        // (0, 0)-(0, 0) can't contain (4, 15) under any split.
        let positions = sel_positions(&[(4, 15)]);
        let result = serde_json::json!([entry(rng(0, 0, 0, 0), None)]);
        let r = judge_selection_reply(&doc_b(), Encoding::Utf16, 7, &positions, &result).unwrap();
        assert_eq!(r.failures.len(), 1, "{r:?}");
        assert!(r.failures[0].contains("doesn't contain the position (4, 15)"), "{r:?}");
        assert!(!r.failures[0].contains("see D5"), "{r:?}");
        assert_eq!(judge_d6(std::slice::from_ref(&r), None).0, Verdict::Fail);
        // Document A has no lone `\r` or `\r\n`, so its wrong splits are its
        // spec split: a failure there gets no hint either.
        let a = doc_a();
        let positions = sel_positions(&[(0, 6)]);
        let result = serde_json::json!([entry(rng(0, 0, 0, 5), None)]);
        let r = judge_selection_reply(&a, Encoding::Utf16, 41, &positions, &result).unwrap();
        assert_eq!(r.failures.len(), 1, "{r:?}");
        assert!(!r.failures[0].contains("see D5"), "{r:?}");
    }

    #[test]
    fn d6_an_empty_range_at_the_position_is_lawful() {
        let text = "hello world\n";
        let positions = sel_positions(&[(0, 6)]);
        let r = judge_selection_reply(
            text,
            Encoding::Utf16,
            41,
            &positions,
            &serde_json::json!([entry(rng(0, 6, 0, 6), None)]),
        )
        .unwrap();
        assert!(r.failures.is_empty(), "{r:?}");
        assert_eq!(r.entries, 1);
    }

    #[test]
    fn d6_the_array_length_must_match_the_positions() {
        let text = "hello world\n";
        let positions = sel_positions(&[(0, 6), (0, 10)]);
        // One entry for two positions: fail.
        let r = judge_selection_reply(
            text,
            Encoding::Utf16,
            41,
            &positions,
            &serde_json::json!([entry(rng(0, 6, 0, 6), None)]),
        )
        .unwrap();
        assert_eq!(r.failures.len(), 1, "{r:?}");
        assert!(r.failures[0].contains("1 entries for 2 positions"), "{r:?}");
        // Two entries for two positions: no length failure.
        let r = judge_selection_reply(
            text,
            Encoding::Utf16,
            41,
            &positions,
            &serde_json::json!([entry(rng(0, 6, 0, 6), None), entry(rng(0, 10, 0, 10), None)]),
        )
        .unwrap();
        assert!(r.failures.is_empty(), "{r:?}");
    }

    #[test]
    fn d6_a_parent_must_contain_its_child() {
        let text = "hello world\n";
        let positions = sel_positions(&[(0, 6)]);
        // The parent contains the child: pass.
        let r = judge_selection_reply(
            text,
            Encoding::Utf16,
            41,
            &positions,
            &serde_json::json!([entry(rng(0, 6, 0, 11), Some(entry(rng(0, 0, 0, 11), None)))]),
        )
        .unwrap();
        assert!(r.failures.is_empty(), "{r:?}");
        // The parent is strictly inside the child: fail.
        let r = judge_selection_reply(
            text,
            Encoding::Utf16,
            41,
            &positions,
            &serde_json::json!([entry(rng(0, 6, 0, 11), Some(entry(rng(0, 7, 0, 10), None)))]),
        )
        .unwrap();
        assert_eq!(r.failures.len(), 1, "{r:?}");
        assert!(r.failures[0].contains("parent (0,7)-(0,10) doesn't contain (0,6)-(0,11)"), "{r:?}");
        // Two levels up: the grandparent must contain the parent too.
        let r = judge_selection_reply(
            text,
            Encoding::Utf16,
            41,
            &positions,
            &serde_json::json!([entry(
                rng(0, 6, 0, 11),
                Some(entry(rng(0, 0, 0, 11), Some(entry(rng(0, 1, 0, 10), None)))),
            )]),
        )
        .unwrap();
        assert_eq!(r.failures.len(), 1, "{r:?}");
        assert!(r.failures[0].contains("parent (0,1)-(0,10) doesn't contain (0,0)-(0,11)"), "{r:?}");
    }

    #[test]
    fn d6_clamps_a_character_past_the_line_end() {
        // A line of five characters; a range ending at character 999 clamps
        // to the line's end, and contains a position at that end.
        let text = "hello\n";
        let positions = sel_positions(&[(0, 5)]);
        let r = judge_selection_reply(
            text,
            Encoding::Utf16,
            41,
            &positions,
            &serde_json::json!([entry(rng(0, 0, 0, 999), None)]),
        )
        .unwrap();
        assert!(r.failures.is_empty(), "{r:?}");
        // A position past the end clamps too, and is contained.
        let positions = sel_positions(&[(0, 999)]);
        let r = judge_selection_reply(
            text,
            Encoding::Utf16,
            41,
            &positions,
            &serde_json::json!([entry(rng(0, 0, 0, 5), None)]),
        )
        .unwrap();
        assert!(r.failures.is_empty(), "{r:?}");
    }

    #[test]
    fn d6_a_range_on_a_missing_line_is_not_judged() {
        // The text has one line; a range on line 5 is not judged.
        let text = "hello\n";
        let positions = sel_positions(&[(0, 1)]);
        // Part 1: the entry's range is on line 5 (missing). Not judged.
        let r = judge_selection_reply(
            text,
            Encoding::Utf16,
            41,
            &positions,
            &serde_json::json!([entry(rng(5, 0, 5, 3), None)]),
        )
        .unwrap();
        assert!(r.failures.is_empty(), "{r:?}");
        assert_eq!(r.no_line, 1, "{r:?}");
        // Part 2: the parent's range is on line 5 (missing). The entry's
        // range contains the position; the parent is not judged.
        let r = judge_selection_reply(
            text,
            Encoding::Utf16,
            41,
            &positions,
            &serde_json::json!([entry(rng(0, 0, 0, 3), Some(entry(rng(5, 0, 5, 3), None)))]),
        )
        .unwrap();
        assert!(r.failures.is_empty(), "{r:?}");
        assert_eq!(r.no_line, 1, "{r:?}");
    }

    #[test]
    fn d6_needs_a_comparison_to_pass() {
        let text = "hello\n";
        let positions = sel_positions(&[(0, 1), (0, 2)]);
        // Every entry malformed: a reply, but nothing compared.
        let bad = judge_selection_reply(text, Encoding::Utf16, 41, &positions, &serde_json::json!([{}, {}])).unwrap();
        assert_eq!(bad.compared, 0, "{bad:?}");
        let (v, d) = judge_d6(std::slice::from_ref(&bad), None);
        assert_eq!(v, Verdict::NotExercised, "{d}");
        assert!(d.contains("no range could be compared"), "{d}");
        // Every range on a missing line: the same.
        let missing = judge_selection_reply(
            text,
            Encoding::Utf16,
            41,
            &positions,
            &serde_json::json!([entry(rng(5, 0, 5, 3), None), entry(rng(6, 0, 6, 3), None)]),
        )
        .unwrap();
        assert_eq!(judge_d6(&[missing], None).0, Verdict::NotExercised);
        // One real comparison is enough.
        let good = judge_selection_reply(
            text,
            Encoding::Utf16,
            41,
            &positions,
            &serde_json::json!([entry(rng(0, 0, 0, 5), None), entry(rng(0, 0, 0, 5), None)]),
        )
        .unwrap();
        assert_eq!(judge_d6(&[bad, good], None).0, Verdict::Pass);
    }

    #[test]
    fn d6_malformed_entries_are_counted_not_judged() {
        let text = "hello world\n";
        let positions = sel_positions(&[(0, 6), (0, 10)]);
        // An entry that isn't an object.
        let r = judge_selection_reply(
            text,
            Encoding::Utf16,
            41,
            &positions,
            &serde_json::json!([7, entry(rng(0, 10, 0, 10), None)]),
        )
        .unwrap();
        assert_eq!(r.malformed, 1, "{r:?}");
        assert_eq!(r.entries, 1, "{r:?}");
        // An entry with no `range`.
        let r = judge_selection_reply(
            text,
            Encoding::Utf16,
            41,
            &positions,
            &serde_json::json!([{"x": 1}, entry(rng(0, 10, 0, 10), None)]),
        )
        .unwrap();
        assert_eq!(r.malformed, 1, "{r:?}");
        // A `parent` that isn't an object.
        let r = judge_selection_reply(
            text,
            Encoding::Utf16,
            41,
            &positions,
            &serde_json::json!([
                serde_json::json!({"range": rng(0, 6, 0, 6), "parent": 7}),
                entry(rng(0, 10, 0, 10), None)
            ]),
        )
        .unwrap();
        assert_eq!(r.malformed, 1, "{r:?}");
        // A `parent` with no `range`.
        let r = judge_selection_reply(
            text,
            Encoding::Utf16,
            41,
            &positions,
            &serde_json::json!([
                serde_json::json!({"range": rng(0, 6, 0, 6), "parent": {}}),
                entry(rng(0, 10, 0, 10), None)
            ]),
        )
        .unwrap();
        assert_eq!(r.malformed, 1, "{r:?}");
    }

    #[test]
    fn d6_a_long_parent_chain_is_cut_short() {
        let text = "hello world\n";
        let positions = sel_positions(&[(0, 6)]);
        // Build a single entry with a parent chain of 1001 nodes, each
        // containing the last.
        let mut chain = entry(rng(0, 0, 0, 11), None);
        for _ in 0..SELECTION_CHAIN_BOUND {
            chain = entry(rng(0, 0, 0, 11), Some(chain));
        }
        let result = serde_json::json!([chain]);
        let r = judge_selection_reply(text, Encoding::Utf16, 41, &positions, &result).unwrap();
        assert_eq!(r.cut_chains, 1, "{r:?}");
        assert!(r.failures.is_empty(), "{r:?}");
    }

    #[test]
    fn d6_a_non_array_result_is_not_judged() {
        let text = "hello world\n";
        let positions = sel_positions(&[(0, 6)]);
        // An object result: not an array, not judged.
        assert!(judge_selection_reply(text, Encoding::Utf16, 41, &positions, &serde_json::json!({})).is_none());
        // A null result: not an array, not judged.
        assert!(judge_selection_reply(text, Encoding::Utf16, 41, &positions, &Value::Null).is_none());
    }

    #[test]
    fn d6_verdicts() {
        let text = "hello world\n";
        let positions = sel_positions(&[(0, 6)]);
        let ok = judge_selection_reply(
            text,
            Encoding::Utf16,
            41,
            &positions,
            &serde_json::json!([entry(rng(0, 6, 0, 11), None)]),
        )
        .unwrap();
        let bad = judge_selection_reply(
            text,
            Encoding::Utf16,
            41,
            &positions,
            &serde_json::json!([entry(rng(0, 0, 0, 5), None)]),
        )
        .unwrap();
        // A pass over evidence.
        let (v, d) = judge_d6(std::slice::from_ref(&ok), None);
        assert_eq!(v, Verdict::Pass, "{d}");
        assert!(d.contains("1 selectionRange reply(s) with 1 entries judged"), "{d}");
        // A failure seen stands, even over missed evidence.
        let (v, d) = judge_d6(std::slice::from_ref(&bad), Some("a frame was skipped"));
        assert_eq!(v, Verdict::Fail, "{d}");
        assert!(d.contains("doesn't contain the position"), "{d}");
        // A pass over missed evidence is an error.
        let (v, d) = judge_d6(std::slice::from_ref(&ok), Some("a frame was skipped"));
        assert_eq!(v, Verdict::Error, "{d}");
        assert!(d.contains("inconclusive"), "{d}");
        // No reply with an array result: not exercised.
        let (v, d) = judge_d6(&[], None);
        assert_eq!(v, Verdict::NotExercised, "{d}");
        assert!(d.contains("no selectionRange reply with an array result was judged"), "{d}");
        // Several failures are named a few at a time.
        let many: Vec<SelectionReply> = (0..5).map(|_| bad.clone()).collect();
        let (v, d) = judge_d6(&many, None);
        assert_eq!(v, Verdict::Fail, "{d}");
        assert!(d.starts_with("5 selectionRange finding(s):"), "{d}");
        assert!(d.contains("; and 2 more"), "{d}");
    }

    #[test]
    fn d6_request_positions_are_extracted() {
        let params = serde_json::json!({"positions": [{"line": 0, "character": 6}, {"line": 1, "character": 2}]});
        assert_eq!(request_positions(&params), sel_positions(&[(0, 6), (1, 2)]));
        // No positions: empty.
        assert!(request_positions(&serde_json::json!({})).is_empty());
        // A malformed position is skipped.
        let params = serde_json::json!({"positions": [{"line": 0}, {"line": 1, "character": 2}]});
        assert_eq!(request_positions(&params), sel_positions(&[(1, 2)]));
    }

    // --- The mutation scenario's window of texts ---------------------------

    /// A document sent at versions 1 to `n` (at 10 ms apart), whose text at
    /// version `v` is `"v"` repeated, keeping the latest `keep` texts.
    fn long_doc(n: i64, keep: usize) -> OpenDoc {
        let mut d = OpenDoc::opened(URI.into(), Version { at: Duration::from_millis(10), number: 1, text: "1".into() });
        for v in 2..=n {
            let text = v.to_string().repeat(v as usize);
            d.push(Version { at: Duration::from_millis(10 * v as u64), number: v, text }, keep);
        }
        d
    }

    #[test]
    fn dropped_texts_keep_their_versions_and_leave_what_refers_to_them_unjudged() {
        let d = long_doc(10, 4);
        assert_eq!(d.versions.iter().map(|v| v.number).collect::<Vec<_>>(), [7, 8, 9, 10]);
        assert_eq!(d.sent().map(|(_, n)| n).collect::<Vec<_>>(), (1..=10).collect::<Vec<_>>());
        let nums = |s: &Source| candidates(&d, s).map(|c| c.iter().map(|v| v.number).collect::<Vec<_>>());
        // A reply to a request sent at version 7 or later is judged; one sent
        // at version 6 may refer to its text, which is gone.
        assert_eq!(nums(&reply(vec![], 75, 85)), Some(vec![7, 8]));
        assert_eq!(nums(&reply(vec![], 65, 85)), None);
        // A publish labelled with a kept version is judged against it alone; one
        // labelled with a dropped version, or unlabelled, isn't judged.
        assert_eq!(nums(&diagnostics(vec![], 200, Some(9))), Some(vec![9]));
        assert_eq!(nums(&diagnostics(vec![], 200, Some(3))), None);
        assert_eq!(nums(&diagnostics(vec![], 200, None)), None);
        // Before the open, still nothing to refer to.
        assert_eq!(nums(&diagnostics(vec![], 5, None)), Some(vec![]));
        // Unjudged positions are counted, and shown.
        let early = reply(vec![range(URI, (0, 1), (0, 2))], 65, 85);
        let j = judge_positions(Encoding::Utf16, std::slice::from_ref(&d), &[early]);
        assert_eq!((j.seen, j.evicted, j.invalid.len()), (0, 2, 0));
        let (_, detail) = judge_d1(Encoding::Utf16, &j, None);
        assert!(
            detail.contains(", 2 that may refer to a version older than the 64 whose texts lspcheck keeps"),
            "{detail}"
        );
        // Nothing is dropped below the bound.
        assert!(long_doc(10, KEPT_TEXTS).dropped.is_empty());
    }

    #[test]
    fn d2_accepts_a_version_whose_text_was_dropped_and_names_versions_compactly() {
        let d = long_doc(10, 4);
        let (v, detail) = judge_d2(std::slice::from_ref(&d), &[publish(URI, serde_json::json!(2), 200)], None);
        assert_eq!(v, Verdict::Pass, "{detail}");
        let mut late = publish(URI, serde_json::json!(11), 200);
        late.place = Some("case 3".into());
        let (v, detail) = judge_d2(std::slice::from_ref(&d), &[late], None);
        assert_eq!(v, Verdict::Fail, "{detail}");
        assert!(
            detail.contains(": case 3: file:///w/doc.txt carries version 11, and lspcheck had sent 1 to 10 for it"),
            "{detail}"
        );
        assert_eq!(version_list(&[41, 42]), "41 and 42");
        assert_eq!(version_list(&[1, 2, 3, 5, 7, 8]), "1 to 3 and 5 and 7 and 8");
        assert_eq!(version_list(&[7]), "7");
    }

    #[test]
    fn d6_judges_a_reply_against_every_text_it_may_describe() {
        // Version 3's line 0 has ten characters; version 4 shortens it to three.
        // A request at column 9 of version 3, answered late about version 4: the
        // parent ends at column 3, which contains the position clamped there.
        let v3 = Version { at: Duration::from_millis(10), number: 3, text: "abcdefghij\n".into() };
        let v4 = Version { at: Duration::from_millis(20), number: 4, text: "abc\n".into() };
        let positions = sel_positions(&[(0, 9)]);
        let result = serde_json::json!([{"range": rng(0, 9, 0, 9), "parent": {"range": rng(0, 0, 0, 3)}}]);
        // Against version 3 alone, the parent doesn't contain its child.
        let alone = judge_selection_candidates(Some(&[&v3]), Encoding::Utf16, 3, &positions, &result).unwrap();
        assert_eq!(
            alone.failures,
            ["selectionRange at version 3, entry 0: parent (0,0)-(0,3) doesn't contain (0,9)-(0,9)"]
        );
        // Against either, it does in version 4: no failure.
        let either = judge_selection_candidates(Some(&[&v3, &v4]), Encoding::Utf16, 3, &positions, &result).unwrap();
        assert!(either.failures.is_empty(), "{:?}", either.failures);
        assert_eq!((either.entries, either.compared), (1, 2));
        // A failure in every text names them all.
        let outside = serde_json::json!([{"range": rng(0, 0, 0, 1)}]);
        let r = judge_selection_candidates(Some(&[&v3, &v4]), Encoding::Utf16, 3, &positions, &outside).unwrap();
        assert_eq!(
            r.failures,
            ["selectionRange at version 3 or 4, entry 0: range (0,0)-(0,1) doesn't contain the position (0, 9)"]
        );
        // A text lspcheck no longer keeps: the count is judged, containment isn't.
        let r = judge_selection_candidates(None, Encoding::Utf16, 3, &positions, &outside).unwrap();
        assert!(r.failures.is_empty() && r.compared == 0 && r.evicted == 1, "{r:?}");
        let r = judge_selection_candidates(None, Encoding::Utf16, 3, &positions, &serde_json::json!([])).unwrap();
        assert_eq!(r.failures, ["selectionRange at version 3 or later: 0 entries for 1 positions"]);
        let (v, d) = judge_d6(&[r], None);
        assert_eq!(v, Verdict::Fail, "{d}");
        assert!(d.ends_with("1 reply(s) that may describe a version older than the 64 whose texts lspcheck keeps, whose containment wasn't judged"), "{d}");
    }
}
