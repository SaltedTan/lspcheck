// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! K1: the requests a server's capabilities advertise, the places in a
//! document they are asked about, the follow-ups their answers lead to, and
//! the verdict on how they were answered.
//!
//! Pure functions only; the scenario that sends the requests, and checks a
//! follow-up's item against the meta-model, is in [`crate::checks`].

use crate::message::{self, codes};
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
    followup: Option<Followup>,
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

type Row = (&'static str, Option<&'static str>, &'static str, Shape, Option<Followup>);

/// The capability, its required part, the method, its params and its follow-ups.
fn table() -> Vec<Row> {
    use Shape::*;
    let none = || json!({});
    let options = || json!({"options": {"tabSize": 4, "insertSpaces": true}});
    vec![
        ("hoverProvider", None, "textDocument/hover", Position(none()), None),
        ("definitionProvider", None, "textDocument/definition", Position(none()), None),
        ("declarationProvider", None, "textDocument/declaration", Position(none()), None),
        ("typeDefinitionProvider", None, "textDocument/typeDefinition", Position(none()), None),
        ("implementationProvider", None, "textDocument/implementation", Position(none()), None),
        (
            "referencesProvider",
            None,
            "textDocument/references",
            Position(json!({"context": {"includeDeclaration": true}})),
            None,
        ),
        ("documentHighlightProvider", None, "textDocument/documentHighlight", Position(none()), None),
        ("signatureHelpProvider", None, "textDocument/signatureHelp", Position(none()), None),
        (
            "completionProvider",
            None,
            "textDocument/completion",
            Position(none()),
            Some(Followup::resolve(&["completionItem/resolve"], "CompletionItem")),
        ),
        ("renameProvider", Some("prepareProvider"), "textDocument/prepareRename", Position(none()), None),
        ("renameProvider", None, "textDocument/rename", Position(json!({"newName": NEW_NAME})), None),
        ("linkedEditingRangeProvider", None, "textDocument/linkedEditingRange", Position(none()), None),
        (
            "callHierarchyProvider",
            None,
            "textDocument/prepareCallHierarchy",
            Position(none()),
            Some(Followup::hierarchy(
                &["callHierarchy/incomingCalls", "callHierarchy/outgoingCalls"],
                "CallHierarchyItem",
            )),
        ),
        (
            "typeHierarchyProvider",
            None,
            "textDocument/prepareTypeHierarchy",
            Position(none()),
            Some(Followup::hierarchy(&["typeHierarchy/supertypes", "typeHierarchy/subtypes"], "TypeHierarchyItem")),
        ),
        ("monikerProvider", None, "textDocument/moniker", Position(none()), None),
        ("selectionRangeProvider", None, "textDocument/selectionRange", Positions, None),
        ("documentSymbolProvider", None, "textDocument/documentSymbol", Document(none()), None),
        ("foldingRangeProvider", None, "textDocument/foldingRange", Document(none()), None),
        (
            "codeLensProvider",
            None,
            "textDocument/codeLens",
            Document(none()),
            Some(Followup::resolve(&["codeLens/resolve"], "CodeLens")),
        ),
        (
            "documentLinkProvider",
            None,
            "textDocument/documentLink",
            Document(none()),
            Some(Followup::resolve(&["documentLink/resolve"], "DocumentLink")),
        ),
        ("colorProvider", None, "textDocument/documentColor", Document(none()), None),
        ("diagnosticProvider", None, "textDocument/diagnostic", Document(none()), None),
        ("documentFormattingProvider", None, "textDocument/formatting", Document(options()), None),
        ("documentRangeFormattingProvider", None, "textDocument/rangeFormatting", Range(options()), None),
        ("codeActionProvider", None, "textDocument/codeAction", Range(json!({"context": {"diagnostics": []}})), None),
        (
            "inlayHintProvider",
            None,
            "textDocument/inlayHint",
            Range(none()),
            Some(Followup::resolve(&["inlayHint/resolve"], "InlayHint")),
        ),
        ("semanticTokensProvider", Some("full"), "textDocument/semanticTokens/full", Document(none()), None),
        ("semanticTokensProvider", Some("range"), "textDocument/semanticTokens/range", Range(none()), None),
        (
            "workspaceSymbolProvider",
            None,
            "workspace/symbol",
            Workspace(json!({"query": ""})),
            Some(Followup::resolve(&["workspaceSymbol/resolve"], "WorkspaceSymbol")),
        ),
    ]
}

/// The initial probe methods available to `[mutation].requests`, in K1's table order.
pub fn methods() -> Vec<&'static str> {
    table().into_iter().map(|(_, _, method, ..)| method).collect()
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

/// Plans initial probes in table order and records why any advertised probe was excluded.
/// Each probe carries any follow-up methods that its reply can trigger. Positions use `encoding`.
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
    for (capability, part, method, shape, followup) in table() {
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
        let followup =
            followup.filter(|f| f.wrapped || caps[capability].get("resolveProvider") == Some(&Value::Bool(true)));
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
                    requests.push(Request { method, label: format!("{method} at {at}"), params, followup });
                }
            }
            Shape::Positions => {
                let labels: Vec<&str> = positions.iter().map(|(at, _)| at.as_str()).collect();
                let all: Vec<Value> = positions.iter().map(|(_, p)| p.clone()).collect();
                let params = with(base(json!({})), ("positions", Value::Array(all)));
                requests.push(Request {
                    method,
                    label: format!("{method} at {}", labels.join(", ")),
                    params,
                    followup,
                });
            }
            Shape::Document(members) => {
                requests.push(Request { method, label: method.into(), params: base(members), followup })
            }
            Shape::Range(members) => {
                let params = with(base(members), ("range", whole.clone()));
                requests.push(Request { method, label: method.into(), params, followup });
            }
            Shape::Workspace(params) => requests.push(Request { method, label: method.into(), params, followup }),
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

/// The methods a source answer leads to and the first item's wire type.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Followup {
    methods: &'static [&'static str],
    item: &'static str,
    wrapped: bool,
}

impl Followup {
    fn hierarchy(methods: &'static [&'static str], item: &'static str) -> Self {
        Self { methods, item, wrapped: true }
    }

    fn resolve(methods: &'static [&'static str], item: &'static str) -> Self {
        Self { methods, item, wrapped: false }
    }
}

#[derive(Debug, PartialEq)]
pub enum FollowupError {
    Unusable(String),
    Inconclusive(String),
}

impl Request {
    pub fn followup_methods(&self) -> &'static [&'static str] {
        self.followup.map_or(&[], |f| f.methods)
    }

    pub fn skipped_followups(&self, why: &str) -> String {
        format!("follow-ups not sent for {} ({}): {why}", self.label, self.followup_methods().join(" and "))
    }

    /// The source answer's first item and the meta-model type it must be
    /// valid as before it is passed back (the caller checks that).
    pub fn followup_item<'a>(&self, result: &'a Value) -> Result<(&'static str, &'a Value), FollowupError> {
        use FollowupError::Unusable;
        let Some(f) = self.followup else { return Err(Unusable("it has no follow-ups".into())) };
        let items = match result {
            Value::Null => return Err(Unusable("the answer was null".into())),
            Value::Array(items) => items,
            Value::Object(list) if self.method == "textDocument/completion" => match list.get("items") {
                Some(Value::Array(items)) => items,
                _ => return Err(Unusable("the answer had no items array".into())),
            },
            _ => return Err(Unusable("the answer wasn't an array".into())),
        };
        let Some(item) = items.first() else { return Err(Unusable("the answer was empty".into())) };
        Ok((f.item, item))
    }

    /// Why `item`, the first item [`Self::followup_item`] gave and a valid
    /// instance of its type by the meta-model, isn't passed back, if it
    /// isn't: it breaks a rule of the specification that the meta-model's
    /// types can't express, or it may be of a type the follow-up doesn't take.
    /// Members for client capabilities lspcheck didn't offer, and rules the
    /// specification only recommends, don't count.
    pub fn item_problem(&self, item: &Value) -> Option<String> {
        let f = self.followup?;
        if let Some(why) = item_flaw(f.item, item, &self.params["position"]) {
            return Some(format!("its first item {why}, so it isn't a valid {}", f.item));
        }
        // workspace/symbol may answer SymbolInformation[] even with
        // resolveProvider, and a SymbolInformation is also a valid
        // WorkspaceSymbol, but only the latter can be resolved. Only a
        // WorkspaceSymbol may have `data` or a location without a range.
        (f.item == "WorkspaceSymbol" && item.get("data").is_none() && item["location"].get("range").is_some())
            .then(|| "its first item may be a SymbolInformation, which workspaceSymbol/resolve doesn't take".into())
    }

    /// The follow-ups for `item`, which [`Self::item_problem`] passed, each
    /// with the item exactly as received.
    pub fn followups(&self, item: &Value) -> Vec<Request> {
        let Some(f) = self.followup else { return Vec::new() };
        let params = if f.wrapped { json!({"item": item}) } else { item.clone() };
        f.methods
            .iter()
            .map(|&method| Request {
                method,
                label: format!("{method} with the first item of {}", self.label),
                params: params.clone(),
                followup: None,
            })
            .collect()
    }
}

/// A position as (line, character).
type Place = (i64, i64);

/// A range's start and end.
type Span = (Place, Place);

/// A `Position`-shaped value's line and character: integers, however
/// spelled (`2` or `2.0`), as the meta-model reads them.
fn place(p: &Value) -> Option<Place> {
    let integer = |v: &Value| v.as_number().and_then(message::integer_value);
    Some((integer(&p["line"])?, integer(&p["character"])?))
}

/// The ends of a `Range`-shaped value: an object whose `start` and `end` are
/// `Position`-shaped.
fn span(v: &Value) -> Option<Span> {
    Some((place(&v["start"])?, place(&v["end"])?))
}

/// The longest path to a range a note names, in characters.
const PATH_SHOWN: usize = 80;

/// How `item`, a valid `ty` by the meta-model, breaks a rule of the
/// specification the meta-model can't express, if it does. Completion text
/// edits are judged against `position`, the completion's. A value lspcheck
/// can't read where the rule needs one counts as breaking it.
fn item_flaw(ty: &str, item: &Value, position: &Value) -> Option<String> {
    if let Some(mut path) = disordered_range(item, &mut String::from("item")) {
        if let Some((end, _)) = path.char_indices().nth(PATH_SHOWN) {
            path.truncate(end);
            path.push('…');
        }
        return Some(format!("has a range that ends before it starts ({path})"));
    }
    match ty {
        "CallHierarchyItem" | "TypeHierarchyItem" => match (span(&item["range"]), span(&item["selectionRange"])) {
            (Some(range), Some(selection)) if range.0 <= selection.0 && selection.1 <= range.1 => None,
            _ => Some("has a selectionRange outside its range".into()),
        },
        "CompletionItem" => completion_flaw(item, position),
        "InlayHint" => {
            let empty = match &item["label"] {
                Value::String(label) => label.is_empty(),
                Value::Array(parts) => {
                    parts.is_empty() || parts.iter().any(|p| p["value"].as_str().is_none_or(str::is_empty))
                }
                _ => true,
            };
            if empty {
                return Some("has an empty label".into());
            }
            let clashing =
                item.get("textEdits").is_some_and(|edits| edit_spans(edits).is_none_or(|edits| clash(&edits, true)));
            clashing.then(|| "has textEdits that overlap".into())
        }
        // The union `Location | {uri}` takes `{uri, range: null}` as the
        // latter, which has no range.
        "WorkspaceSymbol" => item["location"]
            .get("range")
            .is_some_and(|range| span(range).is_none())
            .then(|| "has a location.range that isn't a Range".into()),
        _ => None,
    }
}

/// How a completion item's edits break their rules, if they do: its
/// `textEdit` must be a `TextEdit` (lspcheck doesn't offer
/// `insertReplaceSupport`) on one line, containing the completion's
/// position, and its `additionalTextEdits` may neither overlap it or each
/// other nor share a start with any of them.
fn completion_flaw(item: &Value, position: &Value) -> Option<String> {
    let edit = item.get("textEdit");
    if edit.is_some_and(|e| e.get("insert").is_some() || e.get("replace").is_some()) {
        return Some("has an InsertReplaceEdit textEdit, which lspcheck's client capabilities don't allow".into());
    }
    let main = edit.map(|e| span(&e["range"]));
    if let Some(main) = main {
        let Some((start, end)) = main.filter(|(start, end)| start.0 == end.0) else {
            return Some("has a textEdit that isn't on one line".into());
        };
        if !place(position).is_some_and(|at| start <= at && at <= end) {
            return Some("has a textEdit that doesn't contain the completion's position".into());
        }
    }
    let additional = item.get("additionalTextEdits")?;
    let edits = edit_spans(additional).map(|mut edits| {
        edits.extend(main.flatten());
        edits
    });
    edits
        .is_none_or(|edits| clash(&edits, false))
        .then(|| "has additionalTextEdits that overlap or share a start with each other or its textEdit".into())
}

/// The ranges of an array of text edits, in order, if every one has one.
fn edit_spans(edits: &Value) -> Option<Vec<Span>> {
    edits.as_array()?.iter().map(|e| span(&e["range"])).collect()
}

/// Whether ordered ranges `edits`, in array order, clash: one starts inside
/// another, or several start at one place. With `inserts`, as for any
/// `TextEdit[]`, several may start at one place if all but the last there
/// are inserts (empty ranges).
fn clash(edits: &[Span], inserts: bool) -> bool {
    let mut order: Vec<usize> = (0..edits.len()).collect();
    order.sort_by_key(|&i| (edits[i].0, i));
    // The furthest end of the ranges that start before this place.
    let mut furthest: Option<Place> = None;
    for here in order.chunk_by(|&a, &b| edits[a].0 == edits[b].0) {
        let start = edits[here[0]].0;
        let earlier = &here[..here.len() - 1];
        let shared = !earlier.is_empty() && !(inserts && earlier.iter().all(|&i| edits[i].1 == start));
        if shared || furthest.is_some_and(|furthest| start < furthest) {
            return true;
        }
        furthest = furthest.max(here.iter().map(|&i| edits[i].1).max());
    }
    false
}

/// The path of the first `Range`-shaped value in `value`, below `path`, that
/// ends before it starts. The items' opaque `LSPAny` values, their `data`
/// and a command's `arguments`, aren't looked into: they may hold anything.
fn disordered_range(value: &Value, path: &mut String) -> Option<String> {
    if span(value).is_some_and(|(start, end)| start > end) {
        return Some(path.clone());
    }
    let len = path.len();
    let below = |path: &mut String, v: &Value| {
        let found = disordered_range(v, path);
        path.truncate(len);
        found
    };
    let opaque = |key: &str| key == "data" || key == "arguments";
    match value {
        Value::Object(members) => members.iter().filter(|(key, _)| !opaque(key)).find_map(|(key, v)| {
            if !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                path.push('.');
                path.push_str(key);
            } else {
                path.push_str(&format!("[{}]", Value::from(key.as_str())));
            }
            below(path, v)
        }),
        Value::Array(items) => items.iter().enumerate().find_map(|(i, v)| {
            path.push_str(&format!("[{i}]"));
            below(path, v)
        }),
        _ => None,
    }
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
pub fn judge(asked: &[(String, Outcome)], notes: &[String], inspection_errors: &[String]) -> Judgement {
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
    // Everything K1 couldn't judge.
    let unjudged: Vec<String> =
        unanswered_detail.into_iter().chain(undelivered).chain(inspection_errors.iter().cloned()).collect();
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
    // Follow-ups come between their sources, so a method may recur apart.
    let mut methods: Vec<&str> = Vec::new();
    for (label, _) in asked {
        let method = label.split(' ').next().unwrap_or(label);
        if !methods.contains(&method) {
            methods.push(method);
        }
    }
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

    fn source_requests(caps: &Value) -> Vec<Request> {
        let doc = Document { uri: "file:///w/a.txt", language_id: "plaintext", text: "fn", opened: true };
        requests(caps, &doc, &[Spot { line: 0, column: 1 }], Encoding::Utf16).0
    }

    #[test]
    fn follow_ups_need_their_capability_and_resolve_provider() {
        let caps = json!({
            "callHierarchyProvider": {},
            "typeHierarchyProvider": false,
            "completionProvider": {"resolveProvider": true},
            "codeLensProvider": {"resolveProvider": false},
            "documentLinkProvider": {},
            "inlayHintProvider": {"resolveProvider": true, "documentSelector": null, "id": "x"},
            "workspaceSymbolProvider": true,
        });
        let advertised: Vec<&str> =
            source_requests(&caps).iter().flat_map(|r| r.followup_methods().iter().copied()).collect();
        assert_eq!(
            advertised,
            [
                "completionItem/resolve",
                "callHierarchy/incomingCalls",
                "callHierarchy/outgoingCalls",
                "inlayHint/resolve"
            ]
        );
    }

    #[test]
    fn follow_ups_pass_the_first_item_back_exactly_as_received() {
        let caps = json!({"callHierarchyProvider": true, "completionProvider": {"resolveProvider": true}});
        let at = json!({"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}});
        let item = json!({"name": "f", "kind": 12, "uri": "file:///w/a.txt", "range": at, "selectionRange": at,
                          "data": {"deep": [1, {"x": null}]}, "lspcheckExtra": true});
        let other = json!({"name": "g", "kind": 12, "uri": "file:///w/a.txt", "range": at, "selectionRange": at});
        let requests = source_requests(&caps);
        let prepare = requests.iter().find(|r| r.method == "textDocument/prepareCallHierarchy").unwrap();
        let sent = plan(prepare, &json!([item, other])).unwrap();
        let labels: Vec<&str> = sent.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "callHierarchy/incomingCalls with the first item of textDocument/prepareCallHierarchy at 1:2",
                "callHierarchy/outgoingCalls with the first item of textDocument/prepareCallHierarchy at 1:2",
            ]
        );
        assert!(sent.iter().all(|r| r.params == json!({"item": item})));

        let completion = requests.iter().find(|r| r.method == "textDocument/completion").unwrap();
        let first = json!({"label": "a", "data": 7});
        for answer in [json!([first, {"label": "b"}]), json!({"isIncomplete": true, "items": [first]})] {
            let sent = plan(completion, &answer).unwrap();
            assert_eq!(sent.len(), 1);
            assert_eq!(sent[0].method, "completionItem/resolve");
            assert_eq!(sent[0].params, first, "the item itself is the params");
        }
    }

    /// The follow-ups for a source answer whose first item is valid by the
    /// meta-model.
    fn plan(r: &Request, answer: &Value) -> Result<Vec<Request>, FollowupError> {
        let item = r.followup_item(answer)?.1;
        match r.item_problem(item) {
            Some(why) => Err(FollowupError::Unusable(why)),
            None => Ok(r.followups(item)),
        }
    }

    #[test]
    fn an_answer_without_a_first_item_gets_no_follow_up() {
        let caps = json!({"callHierarchyProvider": true, "completionProvider": {"resolveProvider": true}});
        let requests = source_requests(&caps);
        let prepare = requests.iter().find(|r| r.method == "textDocument/prepareCallHierarchy").unwrap();
        let completion = requests.iter().find(|r| r.method == "textDocument/completion").unwrap();
        for (r, answer, why) in [
            (prepare, json!(null), "the answer was null"),
            (prepare, json!([]), "the answer was empty"),
            (prepare, json!({"name": "f"}), "the answer wasn't an array"),
            (completion, json!({"isIncomplete": false, "items": []}), "the answer was empty"),
            (completion, json!({"isIncomplete": false}), "the answer had no items array"),
        ] {
            assert_eq!(r.followup_item(&answer), Err(FollowupError::Unusable(why.into())), "{answer}");
        }
        let item = json!({"label": "a"});
        assert_eq!(completion.followup_item(&json!([item])), Ok(("CompletionItem", &item)));
    }

    /// A range from `(start line, start character)` to `(end line, end
    /// character)`, each number as given, so its spelling can vary.
    fn range(start: (Value, Value), end: (Value, Value)) -> Value {
        json!({"start": {"line": start.0, "character": start.1}, "end": {"line": end.0, "character": end.1}})
    }

    /// A range of plain integers.
    fn at(sl: u32, sc: u32, el: u32, ec: u32) -> Value {
        range((sl.into(), sc.into()), (el.into(), ec.into()))
    }

    /// Whether `r` follows up `item`, or else why not.
    fn judged(r: &Request, item: &Value) -> Result<(), String> {
        r.item_problem(item).map_or(Ok(()), Err)
    }

    #[test]
    fn a_hierarchy_item_with_disordered_or_uncontained_ranges_gets_no_follow_up() {
        let caps = json!({"callHierarchyProvider": true, "typeHierarchyProvider": true});
        let requests = source_requests(&caps);
        let item = |range: Value, selection: Value| json!({"name": "f", "kind": 12, "uri": "file:///w/a.txt", "range": range, "selectionRange": selection});
        for r in &requests {
            for (range, selection, why) in [
                (at(1, 0, 0, 5), at(1, 0, 1, 0), Some("has a range that ends before it starts (item.range)")),
                (at(0, 0, 2, 0), at(1, 4, 1, 2), Some("has a range that ends before it starts (item.selectionRange)")),
                (at(1, 2, 3, 0), at(1, 1, 1, 4), Some("has a selectionRange outside its range")),
                (at(1, 2, 3, 0), at(2, 0, 3, 1), Some("has a selectionRange outside its range")),
                (at(1, 2, 3, 0), at(1, 2, 3, 0), None),
                (at(1, 2, 3, 0), at(1, 9, 2, 0), None),
            ] {
                let got = plan(r, &json!([item(range.clone(), selection.clone())]));
                match why {
                    Some(why) => {
                        let ty = if r.method.contains("Call") { "CallHierarchyItem" } else { "TypeHierarchyItem" };
                        let why = format!("its first item {why}, so it isn't a valid {ty}");
                        assert_eq!(got, Err(FollowupError::Unusable(why)), "{range} {selection}");
                    }
                    None => assert_eq!(got.map(|sent| sent.len()), Ok(2), "{range} {selection}"),
                }
            }
        }
    }

    #[test]
    fn positions_are_compared_by_value_however_they_are_spelled() {
        let caps = json!({"callHierarchyProvider": true});
        let prepare = &source_requests(&caps)[0];
        let item = |range: Value, selection: Value| json!({"name": "f", "kind": 12, "uri": "file:///w/a.txt", "range": range, "selectionRange": selection});
        let n = |text: &str| -> Value { serde_json::from_str(text).unwrap() };
        let zero = || (n("0"), n("0.0"));
        // 0 to 2.0 is in order, and contains 1e0 to 2.0.
        let valid = item(range(zero(), (n("2.0"), n("0"))), range((n("1e0"), n("0")), (n("2.0"), n("0.0"))));
        assert_eq!(judged(prepare, &valid), Ok(()));
        let inverted = item(range((n("3.0"), n("0")), (n("1.0"), n("0"))), range(zero(), zero()));
        let why = "its first item has a range that ends before it starts (item.range), so it isn't a valid \
                   CallHierarchyItem";
        assert_eq!(judged(prepare, &inverted), Err(why.into()));
        let outside = item(range(zero(), (n("2.0"), n("0"))), range((n("2"), n("0")), (n("2.0"), n("1"))));
        let why = "its first item has a selectionRange outside its range, so it isn't a valid CallHierarchyItem";
        assert_eq!(judged(prepare, &outside), Err(why.into()));
    }

    #[test]
    fn a_range_anywhere_in_an_item_must_be_in_order_but_opaque_values_may_hold_anything() {
        let caps = json!({"codeLensProvider": {"resolveProvider": true}});
        let lenses = &source_requests(&caps)[0];
        let ok = at(0, 0, 0, 1);
        let backwards = at(2, 0, 1, 0);
        let lens = |extra: Value| {
            let mut lens = json!({"range": ok});
            lens.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            lens
        };
        let invalid = |path: &str| {
            Err(format!("its first item has a range that ends before it starts ({path}), so it isn't a valid CodeLens"))
        };
        assert_eq!(judged(lenses, &lens(json!({}))), Ok(()));
        assert_eq!(judged(lenses, &json!({"range": backwards})), invalid("item.range"));
        // Within the same line too.
        assert_eq!(judged(lenses, &json!({"range": at(1, 4, 1, 3)})), invalid("item.range"));
        assert_eq!(
            judged(lenses, &lens(json!({"x-ext": {"nested": [1, backwards]}}))),
            invalid("item[\"x-ext\"].nested[1]")
        );
        // `data` and a command's `arguments` are LSPAny, which may hold anything.
        for opaque in [
            json!({"data": {"range": backwards}}),
            json!({"command": {"title": "t", "command": "c", "arguments": [backwards]}}),
            // Not Range-shaped: the line isn't an integer.
            json!({"x": {"start": {"line": 2.5, "character": 0}, "end": {"line": 1, "character": 0}}}),
        ] {
            assert_eq!(judged(lenses, &lens(opaque.clone())), Ok(()), "{opaque}");
        }
        // A long path is cut short.
        let key = "k".repeat(200);
        let Err(why) = judged(lenses, &lens(json!({key: backwards}))) else { panic!() };
        assert!(why.contains(&format!("(item.{}…)", "k".repeat(75))), "{why}");
    }

    #[test]
    fn a_completion_items_edits_must_be_valid_for_lspchecks_client_and_its_position() {
        let caps = json!({"completionProvider": {"resolveProvider": true}});
        // At 1:2, that is line 0, character 1.
        let completion = &source_requests(&caps)[0];
        let edit = |range: Value| json!({"range": range, "newText": "x"});
        let item = |extra: Value| {
            let mut item = json!({"label": "a"});
            item.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            item
        };
        let invalid = |why: &str| Err(format!("its first item {why}, so it isn't a valid CompletionItem"));
        for (extra, want) in [
            (json!({"textEdit": edit(at(0, 0, 0, 1))}), Ok(())),
            (json!({"textEdit": edit(at(0, 1, 0, 1))}), Ok(())),
            (json!({"textEdit": edit(at(0, 1, 0, 2))}), Ok(())),
            (json!({"textEdit": edit(at(0, 0, 1, 0))}), invalid("has a textEdit that isn't on one line")),
            (
                json!({"textEdit": edit(at(0, 2, 0, 3))}),
                invalid("has a textEdit that doesn't contain the completion's position"),
            ),
            (
                json!({"textEdit": {"newText": "x", "insert": at(0, 0, 0, 1), "replace": at(0, 0, 0, 2)}}),
                invalid("has an InsertReplaceEdit textEdit, which lspcheck's client capabilities don't allow"),
            ),
            // Additional edits may touch, but neither overlap nor share a start.
            (json!({"textEdit": edit(at(0, 1, 0, 2)), "additionalTextEdits": [edit(at(0, 0, 0, 1))]}), Ok(())),
            (json!({"additionalTextEdits": [edit(at(1, 0, 1, 2)), edit(at(0, 0, 0, 0))]}), Ok(())),
            (
                json!({"textEdit": edit(at(0, 1, 0, 2)), "additionalTextEdits": [edit(at(0, 1, 0, 1))]}),
                invalid("has additionalTextEdits that overlap or share a start with each other or its textEdit"),
            ),
            (
                json!({"additionalTextEdits": [edit(at(3, 0, 3, 0)), edit(at(3, 0, 3, 0))]}),
                invalid("has additionalTextEdits that overlap or share a start with each other or its textEdit"),
            ),
            (
                json!({"additionalTextEdits": [edit(at(2, 0, 4, 0)), edit(at(1, 0, 2, 1))]}),
                invalid("has additionalTextEdits that overlap or share a start with each other or its textEdit"),
            ),
            (
                json!({"additionalTextEdits": [edit(at(3, 1, 3, 0))]}),
                invalid("has a range that ends before it starts (item.additionalTextEdits[0].range)"),
            ),
        ] {
            assert_eq!(judged(completion, &item(extra.clone())), want, "{extra}");
        }
    }

    #[test]
    fn an_inlay_hint_needs_a_label_and_edits_that_dont_overlap() {
        let caps = json!({"inlayHintProvider": {"resolveProvider": true}});
        let hints = &source_requests(&caps)[0];
        let hint = |label: Value| json!({"position": {"line": 0, "character": 1}, "label": label});
        let invalid = |why: &str| Err(format!("its first item {why}, so it isn't a valid InlayHint"));
        for (label, want) in [
            (json!("a"), Ok(())),
            (json!([{"value": "a"}, {"value": "b"}]), Ok(())),
            (json!(""), invalid("has an empty label")),
            (json!([]), invalid("has an empty label")),
            (json!([{"value": "a"}, {"value": ""}]), invalid("has an empty label")),
            (
                json!([{"value": "a", "location": {"uri": "file:///w/a.txt", "range": at(0, 3, 0, 2)}}]),
                invalid("has a range that ends before it starts (item.label[0].location.range)"),
            ),
        ] {
            assert_eq!(judged(hints, &hint(label.clone())), want, "{label}");
        }
        let edit = |range: Value| json!({"range": range, "newText": "x"});
        for (edits, want) in [
            (json!([edit(at(0, 0, 0, 1)), edit(at(0, 1, 0, 2))]), Ok(())),
            // A TextEdit[] may insert several times at one place, before at
            // most one other edit there.
            (json!([edit(at(0, 1, 0, 1)), edit(at(0, 1, 0, 1)), edit(at(0, 1, 0, 3))]), Ok(())),
            (json!([edit(at(0, 1, 0, 3)), edit(at(0, 1, 0, 1))]), invalid("has textEdits that overlap")),
            (json!([edit(at(0, 0, 0, 3)), edit(at(0, 2, 0, 2))]), invalid("has textEdits that overlap")),
        ] {
            let mut item = hint(json!("a"));
            item["textEdits"] = edits.clone();
            assert_eq!(judged(hints, &item), want, "{edits}");
        }
    }

    #[test]
    fn only_a_symbol_that_cant_be_a_symbol_information_is_resolved() {
        let caps = json!({"workspaceSymbolProvider": {"resolveProvider": true}});
        let symbols = &source_requests(&caps)[0];
        let uri = "file:///w/a.txt";
        let full = json!({"uri": uri, "range": at(0, 0, 0, 1)});
        let information = "its first item may be a SymbolInformation, which workspaceSymbol/resolve doesn't take";
        let invalid = |why: &str| Err(format!("its first item {why}, so it isn't a valid WorkspaceSymbol"));
        for (symbol, want) in [
            (json!({"name": "s", "kind": 13, "location": full}), Err(information.into())),
            (json!({"name": "s", "kind": 13, "location": full, "containerName": "c"}), Err(information.into())),
            (json!({"name": "s", "kind": 13, "location": full, "data": null}), Ok(())),
            (json!({"name": "s", "kind": 13, "location": {"uri": uri}}), Ok(())),
            // The meta-model reads `{uri, range: null}` as the range-less
            // location, which ignores the extra member.
            (
                json!({"name": "s", "kind": 13, "location": {"uri": uri, "range": null}, "data": 1}),
                invalid("has a location.range that isn't a Range"),
            ),
            (
                json!({"name": "s", "kind": 13, "location": {"uri": uri, "range": at(1, 0, 0, 0)}, "data": 1}),
                invalid("has a range that ends before it starts (item.location.range)"),
            ),
        ] {
            assert_eq!(judged(symbols, &symbol), want, "{symbol}");
            if want.is_ok() {
                assert_eq!(symbols.followups(&symbol)[0].params, symbol);
            }
        }
    }

    #[test]
    fn the_items_hold_opaque_values_only_in_data_and_command_arguments() {
        use crate::metamodel::{Type, model};
        use std::collections::BTreeSet;
        /// Adds the names of the members of `ty`, reached through `key`, whose
        /// type is `LSPAny`.
        fn opaque(ty: &Type, key: &str, within: &mut Vec<String>, found: &mut BTreeSet<String>) {
            match ty {
                Type::Reference { name } if name == "LSPAny" => {
                    found.insert(key.into());
                }
                Type::Reference { name } if !within.contains(name) => {
                    if let Some(s) = model().structures.get(name) {
                        within.push(name.clone());
                        for t in s.extends.iter().chain(&s.mixins) {
                            opaque(t, key, within, found);
                        }
                        for p in &s.properties {
                            opaque(&p.ty, &p.name, within, found);
                        }
                        within.pop();
                    } else if let Some(alias) = model().aliases.get(name) {
                        opaque(&alias.ty, key, within, found);
                    }
                }
                Type::Array { element } => opaque(element, key, within, found),
                Type::Map { value, .. } => opaque(value, key, within, found),
                Type::And { items } | Type::Or { items } | Type::Tuple { items } => {
                    for t in items {
                        opaque(t, key, within, found);
                    }
                }
                Type::Literal { value } => {
                    for p in &value.properties {
                        opaque(&p.ty, &p.name, within, found);
                    }
                }
                _ => {}
            }
        }
        let mut found = BTreeSet::new();
        for (.., followup) in table() {
            if let Some(f) = followup {
                opaque(&Type::Reference { name: f.item.into() }, "", &mut Vec::new(), &mut found);
            }
        }
        assert_eq!(found, BTreeSet::from(["arguments".to_string(), "data".to_string()]));
    }

    #[test]
    fn edits_clash_when_they_overlap_or_share_a_start() {
        let span = |sl, sc, el, ec| ((sl, sc), (el, ec));
        let apart = [span(0, 0, 0, 2), span(0, 2, 0, 3), span(1, 0, 1, 0)];
        assert!(!clash(&apart, false) && !clash(&apart, true));
        // Out of order, the later one runs into the earlier.
        let into = [span(1, 0, 2, 0), span(0, 0, 1, 5)];
        assert!(clash(&into, false) && clash(&into, true));
        // An insert inside another edit.
        let inside = [span(0, 0, 0, 3), span(0, 1, 0, 1)];
        assert!(clash(&inside, false) && clash(&inside, true));
        // Inserts at one place, then one other edit there: lawful only as a TextEdit[].
        let inserts = [span(0, 1, 0, 1), span(0, 1, 0, 1), span(0, 1, 0, 4)];
        assert!(clash(&inserts, false) && !clash(&inserts, true));
        let replace_first = [span(0, 1, 0, 4), span(0, 1, 0, 1)];
        assert!(clash(&replace_first, true));
        let two_replaces = [span(0, 1, 0, 4), span(0, 1, 0, 2)];
        assert!(clash(&two_replaces, true));
        assert!(!clash(&[], false));
    }

    #[test]
    fn a_pass_counts_each_method_once_though_follow_ups_come_between() {
        let asked = [
            answered("textDocument/completion at 1:2", Outcome::Answered),
            answered("completionItem/resolve with the first item of textDocument/completion at 1:2", Outcome::Answered),
            answered("textDocument/completion at 2:6", Outcome::Answered),
            answered("completionItem/resolve with the first item of textDocument/completion at 2:6", Outcome::Answered),
        ];
        let Judgement::Pass(detail) = judge(&asked, &[], &[]) else { panic!() };
        assert!(detail.starts_with("4 request(s) for 2 advertised method(s) were answered"), "{detail}");
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
        let Judgement::Fail(detail) = judge(&asked, &[], &[]) else { panic!() };
        assert!(detail.starts_with("MethodNotFound (-32601) for 1 advertised request(s): textDocument/foldingRange"));
        assert!(detail.contains("no response to textDocument/documentColor by the end of the session"), "{detail}");
        assert!(detail.contains("-32803 (RequestFailed) for textDocument/codeLens"), "{detail}");
    }

    #[test]
    fn a_local_follow_up_validation_limit_leaves_k1_inconclusive() {
        let error = "follow-ups not sent for textDocument/inlayHint (inlayHint/resolve): its first item is too large";
        let errors = [error.to_string()];
        let mut asked = [answered("textDocument/inlayHint", Outcome::Answered)];
        assert_eq!(judge(&asked, &[], &errors), Judgement::Error(error.into()));
        asked[0].1 = Outcome::Error(codes::METHOD_NOT_FOUND);
        let Judgement::Fail(detail) = judge(&asked, &[], &errors) else { panic!() };
        assert!(detail.contains(error), "{detail}");
    }

    #[test]
    fn an_unanswered_request_is_inconclusive_and_an_undelivered_one_is_an_error() {
        let asked = [
            answered("workspace/symbol", Outcome::Error(-32801)),
            answered("textDocument/hover at 1:1", Outcome::Unanswered),
        ];
        assert_eq!(
            judge(&asked, &[], &[]),
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
        let Judgement::Error(detail) = judge(&asked, &[], &[]) else { panic!() };
        assert_eq!(detail, "lspcheck couldn't send workspace/symbol (the server's stdin is closed)");
    }

    #[test]
    fn an_unanswered_request_doesnt_hide_one_that_couldnt_be_sent() {
        let asked = [
            answered("textDocument/hover at 1:1", Outcome::Unanswered),
            answered("textDocument/hover at 2:1", Outcome::Undelivered("the server exited with status 17".into())),
        ];
        let Judgement::Error(detail) = judge(&asked, &[], &[]) else { panic!() };
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
        let Judgement::Fail(detail) = judge(&asked, &[], &[]) else { panic!() };
        assert!(detail.contains("no response to textDocument/hover at 1:1"), "{detail}");
        assert!(
            detail.contains("couldn't send textDocument/hover at 2:1 (the server exited with status 17)"),
            "{detail}"
        );
    }

    #[test]
    fn nothing_advertised_is_not_exercised_and_answers_pass() {
        assert!(matches!(judge(&[], &[], &[]), Judgement::NotExercised(_)));
        let Judgement::NotExercised(d) = judge(&[], &["not sent: x".into()], &[]) else { panic!() };
        assert!(d.contains("not sent: x"), "{d}");
        let asked = [
            answered("textDocument/hover at 1:1", Outcome::Answered),
            answered("textDocument/hover at 2:1", Outcome::Error(-32602)),
            answered("workspace/symbol", Outcome::Answered),
        ];
        let Judgement::Pass(detail) = judge(&asked, &[], &[]) else { panic!() };
        assert_eq!(
            detail,
            "3 request(s) for 2 advertised method(s) were answered, none with MethodNotFound; other errors, which \
             are lawful: -32602 (InvalidParams) for textDocument/hover at 2:1"
        );
    }
}
