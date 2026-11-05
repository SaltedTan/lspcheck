// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! What the documents scenario judges: the documents lspcheck opened with
//! every version it sent, the texts a position may refer to, and D1's verdict
//! over every position the server sent about them.
//!
//! Pure functions only; the scenario that gathers the evidence is in
//! `checks`. A position is judged only where lspcheck knows exactly which
//! text it refers to, so the verdict can't come from a guess.

use crate::checks::Verdict;
use crate::text::{self, Encoding, Found, FoundKind, Position, PositionStatus};
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

/// Where `needle` starts and ends on line `line` of document A, counted in
/// `encoding`.
pub fn doc_a_span(line: usize, needle: &str, encoding: Encoding) -> (Position, Position) {
    let l = DOC_A_LINES[line];
    let at = l.find(needle).expect("the needle is on the line");
    let units = |bytes: usize| text::line_length(&l[..bytes], encoding);
    (Position { line, character: units(at) }, Position { line, character: units(at + needle.len()) })
}

/// A version of a document that lspcheck sent, in `didOpen` or `didChange`.
#[derive(Debug, Clone)]
pub struct Version {
    /// When lspcheck began writing it.
    pub at: Duration,
    pub number: i64,
    pub text: String,
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
    let mut j = Judged::default();
    for source in sources {
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
}
