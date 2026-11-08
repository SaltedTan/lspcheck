// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! K1: the requests a server's capabilities advertise, the places in a
//! document they are asked about, and the verdict on how they were answered.
//!
//! Pure functions only; the scenario that sends the requests is in
//! [`crate::checks`].

use crate::message::codes;
use crate::text::{self, Encoding, Position};
use serde_json::{Value, json};

/// How many positions a profile's `[document]` may give.
pub const MAX_POSITIONS: usize = 5;
/// How many identifier-like tokens give positions when the profile gives none.
pub const GENERIC_POSITIONS: usize = 3;
/// The `newName` K1's rename requests carry.
const NEW_NAME: &str = "lspcheckRenamed";

/// A place in a document: a line and a column counted in Unicode code points,
/// both from 0. Profiles write it from 1, as `LINE:COLUMN`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spot {
    pub line: usize,
    pub column: usize,
}

impl Spot {
    /// Parses `LINE:COLUMN`, both whole numbers from 1.
    pub fn parse(s: &str) -> Option<Spot> {
        let (line, column) = s.split_once(':')?;
        let number = |n: &str| {
            (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                .then(|| n.parse::<usize>().ok())?
                .filter(|&n| n > 0)
        };
        Some(Spot { line: number(line)? - 1, column: number(column)? - 1 })
    }

    /// As a profile writes it.
    pub fn label(self) -> String {
        format!("{}:{}", self.line + 1, self.column + 1)
    }

    /// Why the spot isn't in `text`, if it isn't: its line must exist (the
    /// spec's line split), and its column be at most the line's length in
    /// code points, so 1 past its last character at most.
    pub fn check(self, text: &str) -> Result<(), String> {
        let lines = text::split_lines(text);
        let Some(line) = lines.get(self.line) else {
            return Err(format!("{} is past the end: the document has {} line(s)", self.label(), lines.len()));
        };
        let length = line.chars().count();
        if self.column > length {
            return Err(format!(
                "{} is past the end of line {}, which has {length} character(s), so its columns run from 1 to {}",
                self.label(),
                self.line + 1,
                length + 1
            ));
        }
        Ok(())
    }

    /// The spot counted in `encoding`, if it is in `text`.
    pub fn position(self, text: &str, encoding: Encoding) -> Option<Position> {
        let line = *text::split_lines(text).get(self.line)?;
        let bytes = match line.char_indices().nth(self.column) {
            Some((at, _)) => at,
            None if line.chars().count() == self.column => line.len(),
            None => return None,
        };
        Some(Position { line: self.line, character: text::line_length(&line[..bytes], encoding) })
    }
}

/// One character into each of the first `n` identifier-like tokens of `text`
/// (`[A-Za-z_][A-Za-z0-9_]*`, matched leftmost first): a place a server is
/// likely to have something to say about, whatever the language.
pub fn identifier_spots(text: &str, n: usize) -> Vec<Spot> {
    let mut spots = Vec::new();
    for (number, line) in text::split_lines(text).into_iter().enumerate() {
        let mut chars = line.chars().enumerate().peekable();
        while let Some((column, c)) = chars.next() {
            if spots.len() == n {
                return spots;
            }
            if c.is_ascii_alphabetic() || c == '_' {
                spots.push(Spot { line: number, column: column + 1 });
                while chars.next_if(|(_, c)| c.is_ascii_alphanumeric() || *c == '_').is_some() {}
            }
        }
    }
    spots
}

/// The document K1 asks about.
pub struct Document<'a> {
    pub uri: &'a str,
    pub language_id: &'a str,
    pub text: &'a str,
    /// Whether lspcheck opened it with `didOpen`. Requests about a document
    /// are sent only about an open one.
    pub opened: bool,
}

/// A request K1 sends.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub method: &'static str,
    /// The method and, for one asked at a position, where (`LINE:COLUMN`).
    pub label: String,
    pub params: Value,
}

/// What a request is asked with, beyond its `textDocument`.
enum Shape {
    /// `position`, at each spot, and these members.
    Position(Value),
    /// `positions`: every spot at once.
    Positions,
    /// Nothing more than these members.
    Document(Value),
    /// The whole document's `range`, and these members.
    Range(Value),
    /// No document: these params.
    Workspace(Value),
}

/// The capability, the part of it that must be advertised (for a capability
/// that advertises several requests), the method and its params.
fn table() -> Vec<(&'static str, Option<&'static str>, &'static str, Shape)> {
    use Shape::*;
    let none = || json!({});
    let options = || json!({"options": {"tabSize": 4, "insertSpaces": true}});
    vec![
        ("hoverProvider", None, "textDocument/hover", Position(none())),
        ("definitionProvider", None, "textDocument/definition", Position(none())),
        ("declarationProvider", None, "textDocument/declaration", Position(none())),
        ("typeDefinitionProvider", None, "textDocument/typeDefinition", Position(none())),
        ("implementationProvider", None, "textDocument/implementation", Position(none())),
        (
            "referencesProvider",
            None,
            "textDocument/references",
            Position(json!({"context": {"includeDeclaration": true}})),
        ),
        ("documentHighlightProvider", None, "textDocument/documentHighlight", Position(none())),
        ("signatureHelpProvider", None, "textDocument/signatureHelp", Position(none())),
        ("completionProvider", None, "textDocument/completion", Position(none())),
        ("renameProvider", Some("prepareProvider"), "textDocument/prepareRename", Position(none())),
        ("renameProvider", None, "textDocument/rename", Position(json!({"newName": NEW_NAME}))),
        ("linkedEditingRangeProvider", None, "textDocument/linkedEditingRange", Position(none())),
        ("callHierarchyProvider", None, "textDocument/prepareCallHierarchy", Position(none())),
        ("typeHierarchyProvider", None, "textDocument/prepareTypeHierarchy", Position(none())),
        ("monikerProvider", None, "textDocument/moniker", Position(none())),
        ("selectionRangeProvider", None, "textDocument/selectionRange", Positions),
        ("documentSymbolProvider", None, "textDocument/documentSymbol", Document(none())),
        ("foldingRangeProvider", None, "textDocument/foldingRange", Document(none())),
        ("codeLensProvider", None, "textDocument/codeLens", Document(none())),
        ("documentLinkProvider", None, "textDocument/documentLink", Document(none())),
        ("colorProvider", None, "textDocument/documentColor", Document(none())),
        ("diagnosticProvider", None, "textDocument/diagnostic", Document(none())),
        ("documentFormattingProvider", None, "textDocument/formatting", Document(options())),
        ("documentRangeFormattingProvider", None, "textDocument/rangeFormatting", Range(options())),
        ("codeActionProvider", None, "textDocument/codeAction", Range(json!({"context": {"diagnostics": []}}))),
        ("inlayHintProvider", None, "textDocument/inlayHint", Range(none())),
        ("semanticTokensProvider", Some("full"), "textDocument/semanticTokens/full", Document(none())),
        ("semanticTokensProvider", Some("range"), "textDocument/semanticTokens/range", Range(none())),
        ("workspaceSymbolProvider", None, "workspace/symbol", Workspace(json!({"query": ""}))),
    ]
}

/// Whether a capability's value advertises support: `true`, or an options
/// object.
fn truthy(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::Bool(true) | Value::Object(_)))
}

/// Whether the server advertises the request: the capability is `true` or an
/// object, and `part`, when given, is too. `semanticTokensProvider` is only
/// ever an object, whose `full` and `range` say which requests it serves;
/// `renameProvider.prepareProvider` must be `true` itself.
fn advertised(caps: &Value, capability: &str, part: Option<&str>) -> bool {
    let value = caps.get(capability);
    match part {
        None => capability != "semanticTokensProvider" && truthy(value),
        Some("prepareProvider") => value.and_then(|v| v.get("prepareProvider")) == Some(&Value::Bool(true)),
        Some(part) => value.is_some_and(Value::is_object) && truthy(value.and_then(|v| v.get(part))),
    }
}

/// Whether a static registration's `documentSelector` selects the document,
/// as far as lspcheck can tell: a filter matches when its `language` (if
/// any) is the document's and its `scheme` (if any) is `file`. A filter with
/// a glob `pattern` or for notebooks isn't evaluated, so it doesn't match:
/// the request is then not sent rather than sent about a document the server
/// may not serve.
fn selects(selector: &Value, language_id: &str) -> bool {
    let Some(filters) = selector.as_array() else { return false };
    filters.iter().any(|f| match f {
        Value::Object(f) => {
            !f.contains_key("pattern")
                && !f.contains_key("notebook")
                && f.get("language").is_none_or(|l| l == language_id)
                && f.get("scheme").is_none_or(|s| s == "file")
        }
        _ => false,
    })
}

/// The requests K1 sends, in the order of the design's table, with notes on
/// advertised ones it doesn't send. Every position is counted in `encoding`.
pub fn requests(caps: &Value, doc: &Document, spots: &[Spot], encoding: Encoding) -> (Vec<Request>, Vec<String>) {
    let lines = text::split_lines(doc.text);
    let last = lines.len() - 1;
    let whole = json!({
        "start": {"line": 0, "character": 0},
        "end": {"line": last, "character": text::line_length(lines[last], encoding)},
    });
    let positions: Vec<(String, Value)> = spots
        .iter()
        .filter_map(|s| {
            let p = s.position(doc.text, encoding)?;
            Some((s.label(), json!({"line": p.line, "character": p.character})))
        })
        .collect();
    let mut requests = Vec::new();
    let mut unopened = Vec::new();
    let mut unselected = Vec::new();
    for (capability, part, method, shape) in table() {
        if !advertised(caps, capability, part) {
            continue;
        }
        let about_document = !matches!(shape, Shape::Workspace(_));
        if about_document && !doc.opened {
            unopened.push(method);
            continue;
        }
        if let Some(selector) = caps.get(capability).and_then(|c| c.get("documentSelector")).filter(|s| !s.is_null())
            && !selects(selector, doc.language_id)
        {
            unselected.push(method);
            continue;
        }
        let with = |mut params: Value, extra: (&str, Value)| {
            params[extra.0] = extra.1;
            params
        };
        let base = |members: Value| {
            let mut params = members;
            params["textDocument"] = json!({"uri": doc.uri});
            params
        };
        match shape {
            Shape::Position(members) => {
                for (at, position) in &positions {
                    let params = with(base(members.clone()), ("position", position.clone()));
                    requests.push(Request { method, label: format!("{method} at {at}"), params });
                }
            }
            Shape::Positions => {
                let labels: Vec<&str> = positions.iter().map(|(at, _)| at.as_str()).collect();
                let all: Vec<Value> = positions.iter().map(|(_, p)| p.clone()).collect();
                let params = with(base(json!({})), ("positions", Value::Array(all)));
                requests.push(Request { method, label: format!("{method} at {}", labels.join(", ")), params });
            }
            Shape::Document(members) => requests.push(Request { method, label: method.into(), params: base(members) }),
            Shape::Range(members) => {
                let params = with(base(members), ("range", whole.clone()));
                requests.push(Request { method, label: method.into(), params });
            }
            Shape::Workspace(params) => requests.push(Request { method, label: method.into(), params }),
        }
    }
    let mut notes = Vec::new();
    if !unopened.is_empty() {
        notes.push(format!(
            "not sent, because the server doesn't ask for didOpen (textDocumentSync), so no document is open: {}",
            unopened.join(", ")
        ));
    }
    if !unselected.is_empty() {
        notes.push(format!(
            "not sent, because their documentSelector doesn't select the document (or uses a pattern lspcheck doesn't evaluate): {}",
            unselected.join(", ")
        ));
    }
    (requests, notes)
}

/// How one of K1's requests went.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Answered with a result, or with a malformed response (J5's to judge).
    Answered,
    /// Answered with an error with this code.
    Error(i64),
    /// Delivered, and no response was read by the end of the session.
    Unanswered,
    /// lspcheck couldn't deliver it.
    Undelivered(String),
}

/// What K1 reports.
#[derive(Debug, Clone, PartialEq)]
pub enum Judgement {
    /// A failure seen: MethodNotFound for an advertised request.
    Fail(String),
    /// K1 couldn't judge: a request wasn't delivered, or no response to a
    /// delivered one was read by the end of the session.
    Error(String),
    NotExercised(String),
    Pass(String),
}

/// A known error code's name.
fn code_name(code: i64) -> Option<&'static str> {
    Some(match code {
        codes::PARSE_ERROR => "ParseError",
        codes::INVALID_REQUEST => "InvalidRequest",
        codes::METHOD_NOT_FOUND => "MethodNotFound",
        codes::INVALID_PARAMS => "InvalidParams",
        -32603 => "InternalError",
        codes::SERVER_NOT_INITIALIZED => "ServerNotInitialized",
        codes::REQUEST_CANCELLED => "RequestCancelled",
        -32801 => "ContentModified",
        -32802 => "ServerCancelled",
        -32803 => "RequestFailed",
        _ => return None,
    })
}

/// The other errors, by code, each with the requests that got it.
fn other_errors(asked: &[(String, Outcome)]) -> Option<String> {
    let mut by_code: Vec<(i64, Vec<&str>)> = Vec::new();
    for (label, outcome) in asked {
        if let Outcome::Error(code) = outcome
            && *code != codes::METHOD_NOT_FOUND
        {
            match by_code.iter_mut().find(|(c, _)| c == code) {
                Some((_, labels)) => labels.push(label),
                None => by_code.push((*code, vec![label])),
            }
        }
    }
    if by_code.is_empty() {
        return None;
    }
    let each: Vec<String> = by_code
        .iter()
        .map(|(code, labels)| {
            let name = code_name(*code).map(|n| format!(" ({n})")).unwrap_or_default();
            format!("{code}{name} for {}", labels.join(", "))
        })
        .collect();
    Some(format!("other errors, which are lawful: {}", each.join("; ")))
}

/// K1's verdict over the requests it sent, in order, with `notes` on what it
/// didn't send. MethodNotFound for any of them is a failure seen, whatever
/// happened to the rest. LSP sets no deadline for a response, so a delivered
/// request with no response by the end of the session is inconclusive rather
/// than a failure; one lspcheck couldn't deliver is an error too.
pub fn judge(asked: &[(String, Outcome)], notes: &[String]) -> Judgement {
    let with_notes = |detail: String| {
        let mut parts = vec![detail];
        parts.extend(other_errors(asked));
        parts.extend(notes.iter().cloned());
        parts.join("; ")
    };
    let not_found: Vec<&str> = asked
        .iter()
        .filter(|(_, o)| *o == Outcome::Error(codes::METHOD_NOT_FOUND))
        .map(|(label, _)| label.as_str())
        .collect();
    let unanswered: Vec<&str> =
        asked.iter().filter(|(_, o)| *o == Outcome::Unanswered).map(|(label, _)| label.as_str()).collect();
    let undelivered: Vec<String> = asked
        .iter()
        .filter_map(|(label, o)| match o {
            Outcome::Undelivered(why) => Some(format!("lspcheck couldn't send {label} ({why})")),
            _ => None,
        })
        .collect();
    let unanswered_detail = (!unanswered.is_empty()).then(|| {
        let each = if unanswered.len() == 1 { "it" } else { "them" };
        format!("no response to {} by the end of the session, so K1 couldn't judge {each}", unanswered.join(", "))
    });
    // Everything K1 couldn't judge: unanswered requests, then undelivered ones.
    let unjudged: Vec<String> = unanswered_detail.into_iter().chain(undelivered).collect();
    if !not_found.is_empty() {
        let mut parts = vec![format!(
            "MethodNotFound (-32601) for {} advertised request(s): {}",
            not_found.len(),
            not_found.join(", ")
        )];
        parts.extend(unjudged);
        return Judgement::Fail(with_notes(parts.join("; ")));
    }
    if !unjudged.is_empty() {
        return Judgement::Error(with_notes(unjudged.join("; ")));
    }
    if asked.is_empty() {
        let mut detail = String::from("the server advertises none of the requests K1 sends");
        if !notes.is_empty() {
            detail = format!("no advertised request could be sent; {}", notes.join("; "));
        }
        return Judgement::NotExercised(detail);
    }
    let mut methods: Vec<&str> = asked.iter().map(|(label, _)| label.split(' ').next().unwrap_or(label)).collect();
    methods.dedup();
    Judgement::Pass(with_notes(format!(
        "{} request(s) for {} advertised method(s) were answered, none with MethodNotFound",
        asked.len(),
        methods.len()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spots_parse_from_one_and_check_against_the_text() {
        assert_eq!(Spot::parse("3:6"), Some(Spot { line: 2, column: 5 }));
        for bad in ["", "3", "3:", ":6", "0:1", "1:0", "-1:2", "1:2:3", "a:b", " 1:2", "1:+2"] {
            assert_eq!(Spot::parse(bad), None, "{bad:?}");
        }
        let text = "ab\nc😀d\n";
        assert!(Spot::parse("1:3").unwrap().check(text).is_ok());
        assert!(Spot::parse("2:4").unwrap().check(text).is_ok());
        // The empty line after the final newline exists.
        assert!(Spot::parse("3:1").unwrap().check(text).is_ok());
        let e = Spot::parse("1:4").unwrap().check(text).unwrap_err();
        assert!(e.contains("columns run from 1 to 3"), "{e}");
        let e = Spot::parse("4:1").unwrap().check(text).unwrap_err();
        assert!(e.contains("3 line(s)"), "{e}");
    }

    #[test]
    fn spots_are_counted_in_the_negotiated_encoding() {
        let text = "x\r😀é|\n";
        let spot = Spot { line: 1, column: 2 };
        assert_eq!(spot.position(text, Encoding::Utf8), Some(Position { line: 1, character: 6 }));
        assert_eq!(spot.position(text, Encoding::Utf16), Some(Position { line: 1, character: 3 }));
        assert_eq!(spot.position(text, Encoding::Utf32), Some(Position { line: 1, character: 2 }));
        // The end of a line is a place too; past it isn't.
        assert_eq!(
            Spot { line: 1, column: 3 }.position(text, Encoding::Utf16),
            Some(Position { line: 1, character: 4 })
        );
        assert_eq!(Spot { line: 1, column: 4 }.position(text, Encoding::Utf16), None);
    }

    #[test]
    fn generic_spots_are_one_character_into_identifier_tokens() {
        let text = "  é9 _x1 = 2\n\n😀 fn(a)\n";
        let spots = identifier_spots(text, 3);
        assert_eq!(spots, [Spot { line: 0, column: 6 }, Spot { line: 2, column: 3 }, Spot { line: 2, column: 6 }]);
        // Leftmost matching: digits before letters don't start a token.
        assert_eq!(identifier_spots("3abc", 3), [Spot { line: 0, column: 2 }]);
        assert!(identifier_spots("123 + 456", 3).is_empty());
    }

    fn doc(opened: bool) -> Document<'static> {
        Document { uri: "file:///w/a.txt", language_id: "go", text: "ab\ncd😀\n", opened }
    }

    #[test]
    fn requests_follow_the_advertised_capabilities() {
        let caps = json!({
            "hoverProvider": true,
            "definitionProvider": false,
            "referencesProvider": {},
            "renameProvider": {"prepareProvider": true},
            "selectionRangeProvider": true,
            "semanticTokensProvider": {"legend": {}, "full": {"delta": true}, "range": false},
            "codeActionProvider": true,
            "workspaceSymbolProvider": true,
        });
        let spots = [Spot { line: 0, column: 1 }, Spot { line: 1, column: 3 }];
        let (requests, notes) = requests(&caps, &doc(true), &spots, Encoding::Utf16);
        assert!(notes.is_empty(), "{notes:?}");
        let labels: Vec<&str> = requests.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "textDocument/hover at 1:2",
                "textDocument/hover at 2:4",
                "textDocument/references at 1:2",
                "textDocument/references at 2:4",
                "textDocument/prepareRename at 1:2",
                "textDocument/prepareRename at 2:4",
                "textDocument/rename at 1:2",
                "textDocument/rename at 2:4",
                "textDocument/selectionRange at 1:2, 2:4",
                "textDocument/codeAction",
                "textDocument/semanticTokens/full",
                "workspace/symbol",
            ]
        );
        let hover = &requests[1].params;
        assert_eq!(
            hover,
            &json!({"textDocument": {"uri": "file:///w/a.txt"}, "position": {"line": 1, "character": 4}})
        );
        assert_eq!(requests[2].params["context"], json!({"includeDeclaration": true}));
        assert_eq!(requests[6].params["newName"], "lspcheckRenamed");
        assert_eq!(requests[8].params["positions"], json!([{"line": 0, "character": 1}, {"line": 1, "character": 4}]));
        // The whole document ends on the empty line after the final newline.
        assert_eq!(
            requests[9].params,
            json!({"textDocument": {"uri": "file:///w/a.txt"}, "context": {"diagnostics": []},
                   "range": {"start": {"line": 0, "character": 0}, "end": {"line": 2, "character": 0}}})
        );
        assert_eq!(requests[11].params, json!({"query": ""}));
    }

    #[test]
    fn rename_without_prepare_and_semantic_tokens_as_true_send_no_extra_request() {
        let caps = json!({"renameProvider": true, "semanticTokensProvider": true});
        let (requests, _) = requests(&caps, &doc(true), &[Spot { line: 0, column: 0 }], Encoding::Utf16);
        let methods: Vec<&str> = requests.iter().map(|r| r.method).collect();
        assert_eq!(methods, ["textDocument/rename"]);
    }

    #[test]
    fn k1_document_requests_need_an_open_document_and_a_selector_that_selects_it() {
        let caps = json!({"hoverProvider": true, "workspaceSymbolProvider": true});
        let (sent, notes) = requests(&caps, &doc(false), &[Spot { line: 0, column: 0 }], Encoding::Utf16);
        assert_eq!(sent.iter().map(|r| r.method).collect::<Vec<_>>(), ["workspace/symbol"]);
        assert!(notes[0].contains("textDocument/hover"), "{notes:?}");

        for (selector, sent) in [
            (json!([{"language": "go"}]), true),
            (json!([{"language": "go", "scheme": "file"}]), true),
            (json!(["go"]), false),
            (json!([{"language": "python"}, {"scheme": "file"}]), true),
            (json!(null), true),
            (json!([{"language": "python"}]), false),
            (json!([{"scheme": "untitled"}]), false),
            (json!([{"language": "go", "pattern": "**/*.go"}]), false),
            (json!([{"notebook": "jupyter"}]), false),
            (json!("go"), false),
        ] {
            let caps = json!({"colorProvider": {"documentSelector": selector, "id": "x"}});
            let (requests, notes) = super::requests(&caps, &doc(true), &[], Encoding::Utf16);
            assert_eq!(requests.len(), usize::from(sent), "{selector}");
            assert_eq!(notes.len(), usize::from(!sent), "{selector}");
        }
    }

    fn answered(label: &str, outcome: Outcome) -> (String, Outcome) {
        (label.into(), outcome)
    }

    #[test]
    fn method_not_found_fails_whatever_else_happened() {
        let asked = [
            answered("textDocument/hover at 1:2", Outcome::Answered),
            answered("textDocument/foldingRange", Outcome::Error(-32601)),
            answered("textDocument/codeLens", Outcome::Error(-32803)),
            answered("textDocument/documentColor", Outcome::Unanswered),
        ];
        let Judgement::Fail(detail) = judge(&asked, &[]) else { panic!() };
        assert!(detail.starts_with("MethodNotFound (-32601) for 1 advertised request(s): textDocument/foldingRange"));
        assert!(detail.contains("no response to textDocument/documentColor by the end of the session"), "{detail}");
        assert!(detail.contains("-32803 (RequestFailed) for textDocument/codeLens"), "{detail}");
    }

    #[test]
    fn an_unanswered_request_is_inconclusive_and_an_undelivered_one_is_an_error() {
        let asked = [
            answered("workspace/symbol", Outcome::Error(-32801)),
            answered("textDocument/hover at 1:1", Outcome::Unanswered),
        ];
        assert_eq!(
            judge(&asked, &[]),
            Judgement::Error(
                "no response to textDocument/hover at 1:1 by the end of the session, so K1 couldn't judge it; \
                 other errors, which are lawful: -32801 (ContentModified) for workspace/symbol"
                    .into()
            )
        );
        let asked = [
            answered("textDocument/hover at 1:1", Outcome::Answered),
            answered("workspace/symbol", Outcome::Undelivered("the server's stdin is closed".into())),
        ];
        let Judgement::Error(detail) = judge(&asked, &[]) else { panic!() };
        assert_eq!(detail, "lspcheck couldn't send workspace/symbol (the server's stdin is closed)");
    }

    #[test]
    fn an_unanswered_request_doesnt_hide_one_that_couldnt_be_sent() {
        let asked = [
            answered("textDocument/hover at 1:1", Outcome::Unanswered),
            answered("textDocument/hover at 2:1", Outcome::Undelivered("the server exited with status 17".into())),
        ];
        let Judgement::Error(detail) = judge(&asked, &[]) else { panic!() };
        assert_eq!(
            detail,
            "no response to textDocument/hover at 1:1 by the end of the session, so K1 couldn't judge it; \
             lspcheck couldn't send textDocument/hover at 2:1 (the server exited with status 17)"
        );
        let asked = [
            answered("textDocument/documentColor", Outcome::Error(-32601)),
            answered("textDocument/hover at 1:1", Outcome::Unanswered),
            answered("textDocument/hover at 2:1", Outcome::Undelivered("the server exited with status 17".into())),
        ];
        let Judgement::Fail(detail) = judge(&asked, &[]) else { panic!() };
        assert!(detail.contains("no response to textDocument/hover at 1:1"), "{detail}");
        assert!(
            detail.contains("couldn't send textDocument/hover at 2:1 (the server exited with status 17)"),
            "{detail}"
        );
    }

    #[test]
    fn nothing_advertised_is_not_exercised_and_answers_pass() {
        assert!(matches!(judge(&[], &[]), Judgement::NotExercised(_)));
        let Judgement::NotExercised(d) = judge(&[], &["not sent: x".into()]) else { panic!() };
        assert!(d.contains("not sent: x"), "{d}");
        let asked = [
            answered("textDocument/hover at 1:1", Outcome::Answered),
            answered("textDocument/hover at 2:1", Outcome::Error(-32602)),
            answered("workspace/symbol", Outcome::Answered),
        ];
        let Judgement::Pass(detail) = judge(&asked, &[]) else { panic!() };
        assert_eq!(
            detail,
            "3 request(s) for 2 advertised method(s) were answered, none with MethodNotFound; other errors, which \
             are lawful: -32602 (InvalidParams) for textDocument/hover at 2:1"
        );
    }
}
