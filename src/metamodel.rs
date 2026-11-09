// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! K2: the LSP 3.17 meta-model, and whether a JSON value has one of its types.
//!
//! The meta-model is Microsoft's `metaModel.json`, vendored unmodified under
//! CC BY 4.0 (see `vendor/lsp-3.17/README.md`) and parsed once. Validation is
//! structural, as LSP's types are: a property a structure doesn't name is
//! lawful (servers add extensions), and definitions marked `proposed` aren't
//! judged. The syntax of `URI`, `DocumentUri` and `RegExp` strings isn't
//! judged either. Pure functions only; the judge is in [`crate::checks`].

use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::sync::OnceLock;

const SOURCE: &str = include_str!("../vendor/lsp-3.17/metaModel.json");

/// How many (value, type) pairs one message may take to validate. Enough for
/// several million array items, such as semantic tokens for R3's large
/// document; a message that needs more isn't judged, since that is
/// lspcheck's limit and not a protocol rule.
pub const NODE_BUDGET: usize = 5_000_000;

/// How many steps K2 may take over a whole run, so validating takes bounded
/// time however many messages a server sends; messages left when it is spent
/// aren't judged, which is lspcheck's limit and never a failure.
pub const RUN_BUDGET: usize = 50_000_000;

/// The stack [`on_validation_stack`] gives. Validating takes a few frames for
/// each level of a value's nesting, which serde_json limits to 128; frames are
/// large in a debug build, and Windows gives the main thread only 1 MiB.
pub const STACK: usize = 64 << 20;

/// How many violations of one message are kept for the detail; all are counted.
const KEPT: usize = 8;

/// How many of an enumeration's values a description lists.
const LISTED_VALUES: usize = 8;

/// A type of the meta-model.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Type {
    Base {
        name: Base,
    },
    /// A structure, enumeration or type alias, by name.
    Reference {
        name: String,
    },
    Array {
        element: Box<Type>,
    },
    Map {
        key: Box<Type>,
        value: Box<Type>,
    },
    And {
        items: Vec<Type>,
    },
    Or {
        items: Vec<Type>,
    },
    Tuple {
        items: Vec<Type>,
    },
    /// A structure literal.
    Literal {
        value: Literal,
    },
    StringLiteral {
        value: String,
    },
    /// The value is kept as JSON: numbers inside a tagged enum don't
    /// deserialize as integers with serde_json's `arbitrary_precision`.
    IntegerLiteral {
        value: Value,
    },
    BooleanLiteral {
        value: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum Base {
    #[serde(rename = "URI")]
    Uri,
    DocumentUri,
    #[serde(rename = "integer")]
    Integer,
    #[serde(rename = "uinteger")]
    UInteger,
    #[serde(rename = "decimal")]
    Decimal,
    RegExp,
    #[serde(rename = "string")]
    String,
    #[serde(rename = "boolean")]
    Boolean,
    #[serde(rename = "null")]
    Null,
}

impl Base {
    fn label(self) -> &'static str {
        match self {
            Base::Uri => "URI",
            Base::DocumentUri => "DocumentUri",
            Base::Integer => "integer",
            Base::UInteger => "uinteger",
            Base::Decimal => "decimal",
            Base::RegExp => "RegExp",
            Base::String => "string",
            Base::Boolean => "boolean",
            Base::Null => "null",
        }
    }

    /// The label with the range of an integer type, for a value that broke it.
    fn requirement(self) -> String {
        match self {
            Base::Integer => format!("integer (a whole number from {} to {})", i32::MIN, i32::MAX),
            Base::UInteger => format!("uinteger (a whole number from 0 to {})", i32::MAX),
            Base::Decimal => "decimal (a number)".into(),
            Base::Uri | Base::DocumentUri | Base::RegExp => format!("{} (a string)", self.label()),
            _ => self.label().into(),
        }
    }

    fn admits(self, value: &Value) -> bool {
        match self {
            Base::Integer => integer(value).is_some_and(|n| i32::try_from(n).is_ok()),
            Base::UInteger => integer(value).is_some_and(|n| (0..=i128::from(i32::MAX)).contains(&n)),
            Base::Decimal => value.is_number(),
            Base::Uri | Base::DocumentUri | Base::RegExp | Base::String => value.is_string(),
            Base::Boolean => value.is_boolean(),
            Base::Null => value.is_null(),
        }
    }

    fn string_like(self) -> bool {
        matches!(self, Base::Uri | Base::DocumentUri | Base::RegExp | Base::String)
    }
}

/// A JSON number written as an integer (`1`, not `1.0` or `1e0`), as an
/// `i128`; one too long for that saturates, which is out of LSP's ranges anyway.
fn integer(value: &Value) -> Option<i128> {
    let Value::Number(n) = value else { return None };
    // With `arbitrary_precision`, a number keeps its spelling.
    let text = n.to_string();
    let digits = text.strip_prefix('-').unwrap_or(&text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(text.parse().unwrap_or(if text.starts_with('-') { i128::MIN } else { i128::MAX }))
}

#[derive(Debug, Clone, Deserialize)]
pub struct Literal {
    pub properties: Vec<Property>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Property {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: Type,
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub proposed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Direction {
    ClientToServer,
    ServerToClient,
    Both,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub method: String,
    pub result: Type,
    pub params: Option<Type>,
    pub message_direction: Direction,
    #[serde(default)]
    pub proposed: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notification {
    pub method: String,
    pub params: Option<Type>,
    pub message_direction: Direction,
    #[serde(default)]
    pub proposed: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Structure {
    pub name: String,
    pub properties: Vec<Property>,
    #[serde(default)]
    pub extends: Vec<Type>,
    #[serde(default)]
    pub mixins: Vec<Type>,
    #[serde(default)]
    pub proposed: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Enumeration {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: Type,
    pub values: Vec<EnumerationValue>,
    #[serde(default)]
    pub supports_custom_values: bool,
    #[serde(default)]
    pub proposed: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EnumerationValue {
    pub name: String,
    pub value: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TypeAlias {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: Type,
    #[serde(default)]
    pub proposed: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Raw {
    requests: Vec<Request>,
    notifications: Vec<Notification>,
    structures: Vec<Structure>,
    enumerations: Vec<Enumeration>,
    type_aliases: Vec<TypeAlias>,
}

/// The meta-model, indexed by method and type name.
#[derive(Debug)]
pub struct Model {
    pub requests: HashMap<String, Request>,
    pub notifications: HashMap<String, Notification>,
    pub structures: HashMap<String, Structure>,
    /// Each structure's properties with those it gets through `extends` and
    /// `mixins`, recursively; its own replace inherited ones of the same name.
    properties: HashMap<String, Vec<Property>>,
    pub enumerations: HashMap<String, Enumeration>,
    pub aliases: HashMap<String, TypeAlias>,
}

/// The vendored LSP 3.17 meta-model, parsed on first use.
pub fn model() -> &'static Model {
    static MODEL: OnceLock<Model> = OnceLock::new();
    MODEL.get_or_init(|| Model::parse(SOURCE).expect("the vendored meta-model parses"))
}

impl Model {
    fn parse(text: &str) -> Result<Model, serde_json::Error> {
        let raw: Raw = serde_json::from_str(text)?;
        let structures: HashMap<String, Structure> = raw.structures.into_iter().map(|s| (s.name.clone(), s)).collect();
        let properties = structures.keys().map(|name| (name.clone(), inherited(name, &structures))).collect();
        Ok(Model {
            requests: raw.requests.into_iter().map(|r| (r.method.clone(), r)).collect(),
            notifications: raw.notifications.into_iter().map(|n| (n.method.clone(), n)).collect(),
            structures,
            properties,
            enumerations: raw.enumerations.into_iter().map(|e| (e.name.clone(), e)).collect(),
            aliases: raw.type_aliases.into_iter().map(|a| (a.name.clone(), a)).collect(),
        })
    }

    /// The type of the result of request `method`, when the meta-model
    /// defines that request and doesn't mark it proposed.
    pub fn result(&self, method: &str) -> Option<&Type> {
        self.requests.get(method).filter(|r| !r.proposed).map(|r| &r.result)
    }

    /// The type of the params of a request (`request`) or notification the
    /// server sent, when the meta-model defines one it may send (direction
    /// `serverToClient` or `both`), doesn't mark it proposed, and gives its
    /// params a type.
    pub fn server_params(&self, method: &str, request: bool) -> Option<&Type> {
        let (direction, proposed, params) = if request {
            let r = self.requests.get(method)?;
            (r.message_direction, r.proposed, r.params.as_ref())
        } else {
            let n = self.notifications.get(method)?;
            (n.message_direction, n.proposed, n.params.as_ref())
        };
        (direction != Direction::ClientToServer && !proposed).then_some(params)?
    }
}

/// A structure's properties, with inherited ones (see [`Model::properties`]).
fn inherited(name: &str, structures: &HashMap<String, Structure>) -> Vec<Property> {
    let Some(s) = structures.get(name) else { return Vec::new() };
    let mut all: Vec<Property> = Vec::new();
    let parents = s.extends.iter().chain(&s.mixins).filter_map(|t| match t {
        Type::Reference { name } => Some(name),
        _ => None,
    });
    for p in parents.flat_map(|parent| inherited(parent, structures)).chain(s.properties.iter().cloned()) {
        match all.iter_mut().find(|q| q.name == p.name) {
            Some(q) => *q = p,
            None => all.push(p),
        }
    }
    all
}

/// One place where a value isn't of its type.
#[derive(Debug, Clone, PartialEq)]
pub struct Violation {
    /// Where, as `result.contents.kind` or `params.items[0]`.
    pub path: String,
    /// How many steps below the root that is.
    depth: usize,
    /// What the meta-model requires there.
    pub expected: String,
    /// The value found there, shortened; `None` when it is missing.
    pub found: Option<String>,
    /// For a union nothing matched: the alternative that got deepest into
    /// the value, when one got deeper than the others.
    pub closest: Option<Box<Violation>>,
}

impl Violation {
    /// The innermost violation this one's `closest` chain leads to.
    fn leaf(&self) -> &Violation {
        self.closest.as_deref().map_or(self, Violation::leaf)
    }
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.found {
            None => write!(f, "{} is missing; required: {}", self.path, self.expected)?,
            Some(found) => write!(f, "{} is {found}; required: {}", self.path, self.expected)?,
        }
        if let Some(closest) = &self.closest {
            write!(f, " (closest: {closest})")?;
        }
        Ok(())
    }
}

/// What validating one value found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Violations {
    /// How many violations there were.
    pub count: usize,
    /// The first few, in the order found.
    pub first: Vec<Violation>,
    /// The one whose [`Violation::leaf`] is deepest, the first found at that depth.
    deepest: Option<Violation>,
}

impl Violations {
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn add(&mut self, v: Violation) {
        self.count += 1;
        if self.deepest.as_ref().is_none_or(|d| v.leaf().depth > d.leaf().depth) {
            self.deepest = Some(v.clone());
        }
        if self.first.len() < KEPT {
            self.first.push(v);
        }
    }

    fn merge(&mut self, other: Violations) {
        self.count += other.count - other.first.len();
        if let Some(d) = &other.deepest
            && self.deepest.as_ref().is_none_or(|mine| d.leaf().depth > mine.leaf().depth)
        {
            self.deepest = Some(d.clone());
        }
        for v in other.first {
            self.count += 1;
            if self.first.len() < KEPT {
                self.first.push(v);
            }
        }
    }
}

/// The value was too large to validate within [`NODE_BUDGET`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooLarge;

/// Runs `f`, which validates, on a thread with a [`STACK`]-sized stack. The
/// caller bounds `f`'s work (K2 by [`RUN_BUDGET`]), so the join is bounded too:
/// a wall-clock deadline instead would make verdicts depend on machine speed.
pub fn on_validation_stack<R: Send>(f: impl FnOnce() -> R + Send) -> std::io::Result<R> {
    std::thread::scope(|s| {
        let thread = std::thread::Builder::new().name("lspcheck-validate".into()).stack_size(STACK);
        let handle = thread.spawn_scoped(s, f)?;
        Ok(handle.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
    })
}

/// Validates `value` against `ty`; paths start at `root` (`result` or `params`).
pub fn validate(ty: &Type, value: &Value, root: &str) -> Result<Violations, TooLarge> {
    validate_within(ty, value, root, NODE_BUDGET)
}

/// [`validate`] with a node budget of `budget`.
pub fn validate_within(ty: &Type, value: &Value, root: &str, budget: usize) -> Result<Violations, TooLarge> {
    validate_spending(ty, value, root, budget).0
}

/// [`validate_within`], also returning how many steps of `budget` it took
/// (all of them when the value was too large).
pub fn validate_spending(ty: &Type, value: &Value, root: &str, budget: usize) -> (Result<Violations, TooLarge>, usize) {
    let mut v = Validator { model: model(), budget, root, path: Vec::new() };
    let found = v.check(ty, value, None);
    let spent = if found.is_ok() { budget - v.budget } else { budget };
    (found, spent)
}

/// The violation of a value of type `ty` that is missing at `root`, such as
/// `params` absent where the meta-model gives them a type.
pub fn missing(ty: &Type, root: &str) -> Violations {
    let mut out = Violations::default();
    out.add(Violation { path: root.into(), depth: 0, expected: describe(ty), found: None, closest: None });
    out
}

/// One step of a path.
enum Step<'a> {
    Key(&'a str),
    Index(usize),
}

struct Validator<'a> {
    model: &'a Model,
    budget: usize,
    root: &'a str,
    path: Vec<Step<'a>>,
}

impl<'a> Validator<'a> {
    fn path(&self) -> String {
        let mut out = self.root.to_string();
        for step in &self.path {
            match step {
                Step::Index(i) => out.push_str(&format!("[{i}]")),
                Step::Key(k) if is_identifier(k) => {
                    out.push('.');
                    out.push_str(k);
                }
                Step::Key(k) => out.push_str(&format!("[{}]", Value::String((*k).into()))),
            }
        }
        out
    }

    fn violation(&self, expected: String, found: Option<&Value>) -> Violation {
        Violation { path: self.path(), depth: self.path.len(), expected, found: found.map(shorten), closest: None }
    }

    fn one(&self, expected: String, found: &Value) -> Violations {
        let mut out = Violations::default();
        out.add(self.violation(expected, Some(found)));
        out
    }

    /// Validates `value` against `ty`, which the type alias `alias` names if it is one.
    fn check(&mut self, ty: &'a Type, value: &'a Value, alias: Option<&str>) -> Result<Violations, TooLarge> {
        self.budget = self.budget.checked_sub(1).ok_or(TooLarge)?;
        let expected = || match alias {
            Some(name) => format!("{name} ({})", describe(ty)),
            None => describe(ty),
        };
        let mut out = Violations::default();
        match ty {
            Type::Base { name } => {
                if !name.admits(value) {
                    let required = match alias {
                        Some(alias) => format!("{alias} ({})", name.requirement()),
                        None => name.requirement(),
                    };
                    return Ok(self.one(required, value));
                }
            }
            Type::Reference { name } => return self.reference(name, value),
            Type::Array { element } => {
                let Value::Array(items) = value else { return Ok(self.one(expected(), value)) };
                for (i, item) in items.iter().enumerate() {
                    self.path.push(Step::Index(i));
                    let found = self.check(element, item, None);
                    self.path.pop();
                    out.merge(found?);
                }
            }
            Type::Tuple { items } => {
                let Value::Array(values) = value else { return Ok(self.one(expected(), value)) };
                if values.len() != items.len() {
                    return Ok(self.one(expected(), value));
                }
                for (i, (item, ty)) in values.iter().zip(items).enumerate() {
                    self.path.push(Step::Index(i));
                    let found = self.check(ty, item, None);
                    self.path.pop();
                    out.merge(found?);
                }
            }
            Type::Map { key, value: of } => {
                let Value::Object(entries) = value else { return Ok(self.one(expected(), value)) };
                for (k, v) in entries {
                    self.path.push(Step::Key(k));
                    if let Some(required) = self.key_problem(key, k) {
                        out.add(self.violation(required, Some(&Value::String(k.clone()))));
                    }
                    let found = self.check(of, v, None);
                    self.path.pop();
                    out.merge(found?);
                }
            }
            Type::And { items } => {
                for item in items {
                    out.merge(self.check(item, value, None)?);
                }
            }
            Type::Or { items } => return self.union(items, value, expected()),
            Type::Literal { value: literal } => {
                if !value.is_object() {
                    return Ok(self.one(expected(), value));
                }
                return self.properties(&literal.properties, value);
            }
            Type::StringLiteral { value: want } => {
                if value.as_str() != Some(want.as_str()) {
                    return Ok(self.one(expected(), value));
                }
            }
            Type::IntegerLiteral { value: want } => {
                if integer(value).is_none() || integer(value) != integer(want) {
                    return Ok(self.one(expected(), value));
                }
            }
            Type::BooleanLiteral { value: want } => {
                if value.as_bool() != Some(*want) {
                    return Ok(self.one(expected(), value));
                }
            }
        }
        Ok(out)
    }

    fn reference(&mut self, name: &'a str, value: &'a Value) -> Result<Violations, TooLarge> {
        let model = self.model;
        if let Some(s) = model.structures.get(name) {
            if s.proposed {
                return Ok(Violations::default());
            }
            if !value.is_object() {
                return Ok(self.one(name.into(), value));
            }
            return self.properties(&model.properties[name], value);
        }
        if let Some(e) = model.enumerations.get(name) {
            if e.proposed || enumeration_admits(e, value) {
                return Ok(Violations::default());
            }
            return Ok(self.one(describe_enumeration(e), value));
        }
        match model.aliases.get(name) {
            Some(a) if !a.proposed => self.check(&a.ty, value, Some(name)),
            // Proposed, or a name the meta-model doesn't define (a test shows
            // every reference resolves): nothing to judge.
            _ => Ok(Violations::default()),
        }
    }

    /// The properties of a structure or structure literal, in an object.
    fn properties(&mut self, properties: &'a [Property], value: &'a Value) -> Result<Violations, TooLarge> {
        let mut out = Violations::default();
        for p in properties.iter().filter(|p| !p.proposed) {
            self.path.push(Step::Key(&p.name));
            match value.get(&p.name) {
                None if !p.optional => out.add(self.violation(describe(&p.ty), None)),
                None => {}
                Some(v) => {
                    let found = self.check(&p.ty, v, None);
                    self.path.pop();
                    out.merge(found?);
                    continue;
                }
            }
            self.path.pop();
        }
        Ok(out)
    }

    /// An `or`: one alternative must match. When none does, the violation
    /// names them all, and where the alternative that got deepest into the
    /// value failed, if one got deeper than the others (through nested unions).
    fn union(&mut self, items: &'a [Type], value: &'a Value, expected: String) -> Result<Violations, TooLarge> {
        let mut deepest: Vec<Violation> = Vec::new();
        for item in items {
            let found = self.check(item, value, None)?;
            if found.is_empty() {
                return Ok(Violations::default());
            }
            deepest.extend(found.deepest.map(|d| d.leaf().clone()));
        }
        let mut v = self.violation(expected, Some(value));
        let max = deepest.iter().map(|d| d.depth).max().unwrap_or(0);
        if max > v.depth && deepest.iter().filter(|d| d.depth == max).count() == 1 {
            v.closest = deepest.into_iter().find(|d| d.depth == max).map(Box::new);
        }
        let mut out = Violations::default();
        out.add(v);
        Ok(out)
    }

    /// Why a map key isn't of the key type, if it isn't. JSON keys are
    /// strings, so only a string enumeration's keys are judged.
    fn key_problem(&self, key: &Type, k: &str) -> Option<String> {
        let Type::Reference { name } = key else { return None };
        if let Some(e) = self.model.enumerations.get(name) {
            let string = matches!(e.ty, Type::Base { name } if name.string_like());
            let admitted = e.proposed || enumeration_admits(e, &Value::String(k.into()));
            return (string && !admitted).then(|| describe_enumeration(e));
        }
        match self.model.aliases.get(name) {
            Some(a) if !a.proposed => self.key_problem(&a.ty, k),
            _ => None,
        }
    }
}

fn enumeration_admits(e: &Enumeration, value: &Value) -> bool {
    let Type::Base { name: base } = e.ty else { return true };
    if !base.admits(value) {
        return false;
    }
    e.supports_custom_values
        || e.values.iter().any(|v| match (&v.value, value) {
            (Value::String(a), Value::String(b)) => a == b,
            (a, b) => integer(a).is_some() && integer(a) == integer(b),
        })
}

fn describe_enumeration(e: &Enumeration) -> String {
    let base = match &e.ty {
        Type::Base { name } => name.label(),
        _ => "?",
    };
    if e.supports_custom_values {
        return format!("{} (any {base})", e.name);
    }
    let mut values: Vec<String> = e.values.iter().take(LISTED_VALUES).map(|v| v.value.to_string()).collect();
    if e.values.len() > LISTED_VALUES {
        values.push(format!("… ({} values)", e.values.len()));
    }
    format!("{}: {}", e.name, values.join(" | "))
}

/// A short description of a type, as TypeScript would write it.
pub fn describe(ty: &Type) -> String {
    let grouped = |t: &Type| match t {
        Type::Or { .. } | Type::And { .. } => format!("({})", describe(t)),
        _ => describe(t),
    };
    let join = |items: &[Type], sep: &str| items.iter().map(grouped).collect::<Vec<_>>().join(sep);
    match ty {
        Type::Base { name } => name.label().into(),
        Type::Reference { name } => name.clone(),
        Type::Array { element } => format!("{}[]", grouped(element)),
        Type::Map { key, value } => format!("{{ [key: {}]: {} }}", describe(key), describe(value)),
        Type::And { items } => join(items, " & "),
        Type::Or { items } => join(items, " | "),
        Type::Tuple { items } => format!("[{}]", join(items, ", ")),
        Type::Literal { value } => {
            let names: Vec<String> =
                value.properties.iter().map(|p| format!("{}{}", p.name, if p.optional { "?" } else { "" })).collect();
            format!("{{{}}}", names.join(", "))
        }
        Type::StringLiteral { value } => Value::String(value.clone()).to_string(),
        Type::IntegerLiteral { value } => value.to_string(),
        Type::BooleanLiteral { value } => value.to_string(),
    }
}

fn is_identifier(k: &str) -> bool {
    let mut chars = k.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// A value as JSON, cut short.
fn shorten(value: &Value) -> String {
    const MAX: usize = 60;
    let text = value.to_string();
    match text.char_indices().nth(MAX) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reference(name: &str) -> Type {
        Type::Reference { name: name.into() }
    }

    fn check(ty: &Type, value: Value) -> Vec<String> {
        validate(ty, &value, "result").unwrap().first.iter().map(Violation::to_string).collect()
    }

    fn valid(ty: &Type, value: Value) {
        let found = check(ty, value.clone());
        assert!(found.is_empty(), "{value} against {}: {found:?}", describe(ty));
    }

    /// Exactly one violation, returned.
    fn invalid(ty: &Type, value: Value) -> String {
        let found = check(ty, value.clone());
        assert_eq!(found.len(), 1, "{value} against {}: {found:?}", describe(ty));
        found.into_iter().next().unwrap()
    }

    /// SHA-256 (FIPS 180-4), so the test needs no dependency.
    fn sha256(data: &[u8]) -> String {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98,
            0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
            0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8,
            0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
            0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819,
            0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
            0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];
        let mut h: [u32; 8] =
            [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
        let mut message = data.to_vec();
        message.push(0x80);
        while message.len() % 64 != 56 {
            message.push(0);
        }
        message.extend_from_slice(&(data.len() as u64 * 8).to_be_bytes());
        for block in message.chunks(64) {
            let mut w = [0u32; 64];
            for (i, word) in block.chunks(4).enumerate() {
                w[i] = u32::from_be_bytes(word.try_into().unwrap());
            }
            for i in 16..64 {
                let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
                let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
                w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
            }
            let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
            for i in 0..64 {
                let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                let ch = (e & f) ^ (!e & g);
                let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
                let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let maj = (a & b) ^ (a & c) ^ (b & c);
                let t2 = s0.wrapping_add(maj);
                (hh, g, f, e, d, c, b, a) = (g, f, e, d.wrapping_add(t1), c, b, a, t1.wrapping_add(t2));
            }
            for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
                *x = x.wrapping_add(y);
            }
        }
        h.iter().map(|x| format!("{x:08x}")).collect()
    }

    #[test]
    fn the_vendored_meta_model_is_the_original() {
        assert_eq!(sha256(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(SOURCE.len(), 395_147);
        assert_eq!(sha256(SOURCE.as_bytes()), "c207890d9e0f54d8f9e462a924670f563423ec9af90a0a369645ff55384c8d6d");
    }

    /// Every type in `ty`, recursively, without following references.
    fn walk<'t>(ty: &'t Type, out: &mut Vec<&'t Type>) {
        out.push(ty);
        match ty {
            Type::Array { element } => walk(element, out),
            Type::Map { key, value } => {
                walk(key, out);
                walk(value, out);
            }
            Type::And { items } | Type::Or { items } | Type::Tuple { items } => items.iter().for_each(|t| walk(t, out)),
            Type::Literal { value } => value.properties.iter().for_each(|p| walk(&p.ty, out)),
            _ => {}
        }
    }

    #[test]
    fn the_meta_model_is_consistent() {
        let m = model();
        assert!(m.requests.contains_key("textDocument/hover"));
        assert!(m.notifications.contains_key("window/logMessage"));
        // Names are unique across the three namespaces, so a reference is unambiguous.
        let mut names: Vec<&String> =
            m.structures.keys().chain(m.enumerations.keys()).chain(m.aliases.keys()).collect();
        let count = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), count);
        let mut types = Vec::new();
        for r in m.requests.values() {
            walk(&r.result, &mut types);
            r.params.iter().for_each(|t| walk(t, &mut types));
        }
        for n in m.notifications.values() {
            n.params.iter().for_each(|t| walk(t, &mut types));
        }
        for s in m.structures.values() {
            s.properties.iter().for_each(|p| walk(&p.ty, &mut types));
            for parent in s.extends.iter().chain(&s.mixins) {
                let Type::Reference { name } = parent else { panic!("{} extends a {parent:?}", s.name) };
                assert!(m.structures.contains_key(name), "{} extends {name}", s.name);
            }
        }
        m.aliases.values().for_each(|a| walk(&a.ty, &mut types));
        for t in types {
            if let Type::Reference { name } = t {
                assert!(names.contains(&name), "{name} is referenced but not defined");
            }
        }
        for e in m.enumerations.values() {
            assert!(matches!(e.ty, Type::Base { .. }), "{}", e.name);
        }
    }

    /// The types reachable from `ty` without entering an array, map, tuple
    /// or structure, which each take one level of the value's nesting.
    fn same_level<'m>(m: &'m Model, ty: &'m Type, out: &mut Vec<&'m str>) -> Result<(), String> {
        match ty {
            Type::Reference { name } => {
                if let Some(a) = m.aliases.get(name) {
                    if out.contains(&name.as_str()) {
                        return Err(format!("{out:?} -> {name}"));
                    }
                    out.push(name);
                    same_level(m, &a.ty, out)?;
                    out.pop();
                }
                Ok(())
            }
            Type::And { items } | Type::Or { items } => items.iter().try_for_each(|t| same_level(m, t, out)),
            _ => Ok(()),
        }
    }

    #[test]
    fn validation_depth_is_bounded_by_the_parsers_nesting_limit() {
        // Every cycle of types passes through an array, map, tuple or
        // structure, so validation goes no deeper than the value, plus a
        // bounded number of steps at each level...
        let m = model();
        for a in m.aliases.values() {
            same_level(m, &a.ty, &mut vec![a.name.as_str()]).unwrap();
        }
        // ...and messages are parsed by serde_json, which refuses values
        // nested 128 deep (its `unbounded_depth` feature is off).
        let nested = |n: usize, open: &str, close: &str| format!("{}{}", open.repeat(n), close.repeat(n));
        assert!(serde_json::from_str::<Value>(&nested(128, "[", "]")).is_err());
        // The deepest values it accepts validate on the validation stack.
        let arrays: Value = serde_json::from_str(&nested(127, "[", "]")).unwrap();
        let objects: Value = serde_json::from_str(&format!("{}1{}", "{\"a\":".repeat(127), "}".repeat(127))).unwrap();
        let range = json!({"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}});
        let mut selection = json!({"range": range});
        // 125 levels of parents, each with a range and its start inside: 127 deep.
        for _ in 0..124 {
            selection = json!({"range": range, "parent": selection});
        }
        let selection: Value = serde_json::from_str(&selection.to_string()).unwrap();
        on_validation_stack(|| {
            assert!(validate(&reference("LSPAny"), &arrays, "result").unwrap().is_empty());
            assert!(validate(&reference("LSPAny"), &objects, "result").unwrap().is_empty());
            assert!(validate(&reference("SelectionRange"), &selection, "result").unwrap().is_empty());
            // A violation at the bottom, inside unions at every level.
            let bad = Type::Array { element: Box::new(reference("LSPObject")) };
            assert_eq!(validate(&bad, &arrays, "result").unwrap().count, 1);
        })
        .unwrap();
    }

    #[test]
    fn position() {
        let p = reference("Position");
        valid(&p, json!({"line": 0, "character": 5}));
        valid(&p, json!({"line": 0, "character": 5, "extra": "lawful"}));
        let negative = invalid(&p, json!({"line": -1, "character": 0}));
        assert_eq!(negative, "result.line is -1; required: uinteger (a whole number from 0 to 2147483647)");
        let float: Value = serde_json::from_str(r#"{"line": 1.0, "character": 0}"#).unwrap();
        assert!(invalid(&p, float).starts_with("result.line is 1.0;"));
        assert_eq!(invalid(&p, json!({"line": 0})), "result.character is missing; required: uinteger");
        assert_eq!(invalid(&p, json!([0, 0])), "result is [0,0]; required: Position");
        assert_eq!(check(&p, json!({})).len(), 2);
    }

    #[test]
    fn hover() {
        let hover = reference("Hover");
        valid(&hover, json!({"contents": {"kind": "markdown", "value": "*x*"}}));
        valid(&hover, json!({"contents": "plain"}));
        valid(&hover, json!({"contents": {"language": "rust", "value": "fn"}}));
        valid(&hover, json!({"contents": ["a", {"language": "c", "value": "x"}]}));
        let number = invalid(&hover, json!({"contents": 3}));
        assert_eq!(number, "result.contents is 3; required: MarkupContent | MarkedString | MarkedString[]");
        // The result type of the request, with null.
        let result = model().result("textDocument/hover").unwrap();
        assert_eq!(describe(result), "Hover | null");
        valid(result, Value::Null);
        let wrapped = invalid(result, json!({"contents": 3}));
        assert_eq!(
            wrapped,
            "result is {\"contents\":3}; required: Hover | null (closest: result.contents is 3; required: \
             MarkupContent | MarkedString | MarkedString[])"
        );
    }

    #[test]
    fn definition_result() {
        let result = model().result("textDocument/definition").unwrap();
        let range = json!({"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}});
        let location = json!({"uri": "file:///a", "range": range});
        let link = json!({"targetUri": "file:///a", "targetRange": range, "targetSelectionRange": range});
        valid(result, Value::Null);
        valid(result, location.clone());
        valid(result, json!([location]));
        valid(result, json!([link]));
        valid(result, json!([]));
        // A Location missing its range: the single Location got deepest.
        let found = invalid(result, json!({"uri": "file:///a"}));
        assert_eq!(
            found,
            "result is {\"uri\":\"file:///a\"}; required: Definition | DefinitionLink[] | null (closest: result.range \
             is missing; required: Range)"
        );
        // A mix of locations and links matches no alternative.
        assert!(!check(result, json!([location, link])).is_empty());
    }

    #[test]
    fn markup_content_kinds() {
        let m = reference("MarkupContent");
        valid(&m, json!({"kind": "plaintext", "value": ""}));
        valid(&m, json!({"kind": "markdown", "value": ""}));
        let found = invalid(&m, json!({"kind": "html", "value": ""}));
        assert_eq!(found, "result.kind is \"html\"; required: MarkupKind: \"plaintext\" | \"markdown\"");
    }

    #[test]
    fn enumerations_with_and_without_custom_values() {
        let message_type = reference("MessageType");
        valid(&message_type, json!(1));
        valid(&message_type, json!(4));
        assert!(invalid(&message_type, json!(9)).starts_with("result is 9; required: MessageType: 1 | 2 | 3 | 4"));
        invalid(&message_type, json!("1"));
        let kind = reference("FoldingRangeKind");
        valid(&kind, json!("region"));
        valid(&kind, json!("lspcheck.custom"));
        assert_eq!(invalid(&kind, json!(1)), "result is 1; required: FoldingRangeKind (any string)");
        let action = reference("CodeActionKind");
        valid(&action, json!("lspcheck.custom"));
        // A long enumeration is listed in part.
        let symbol = invalid(&reference("SymbolKind"), json!(0));
        assert!(symbol.ends_with("| 8 | … (26 values)"), "{symbol}");
    }

    #[test]
    fn integer_bounds() {
        let int = Type::Base { name: Base::Integer };
        let uint = Type::Base { name: Base::UInteger };
        valid(&int, json!(-2147483648_i64));
        valid(&int, json!(2147483647));
        invalid(&int, json!(2147483648_i64));
        invalid(&int, json!(-2147483649_i64));
        valid(&uint, json!(0));
        valid(&uint, json!(2147483647));
        invalid(&uint, json!(-1));
        invalid(&uint, json!(2147483648_i64));
        let spelled = |s: &str| serde_json::from_str::<Value>(s).unwrap();
        invalid(&int, spelled("1.0"));
        invalid(&int, spelled("1e2"));
        invalid(&int, spelled("123456789012345678901234567890123456789012345"));
        valid(&Type::Base { name: Base::Decimal }, spelled("1.5e3"));
        invalid(&Type::Base { name: Base::Decimal }, json!("1"));
    }

    #[test]
    fn optional_is_not_null() {
        let hover = reference("Hover");
        let found = invalid(&hover, json!({"contents": "x", "range": null}));
        assert_eq!(found, "result.range is null; required: Range");
        // Where the type admits null, null is lawful.
        let params = reference("PublishDiagnosticsParams");
        let found = invalid(&params, json!({"uri": "file:///a", "diagnostics": [], "version": null}));
        assert_eq!(found, "result.version is null; required: integer (a whole number from -2147483648 to 2147483647)");
        valid(&reference("InitializeParams"), json!({"processId": null, "rootUri": null, "capabilities": {}}));
    }

    #[test]
    fn tuple() {
        let info = reference("ParameterInformation");
        valid(&info, json!({"label": "x"}));
        valid(&info, json!({"label": [0, 1]}));
        assert!(!check(&info, json!({"label": [0]})).is_empty());
        assert!(!check(&info, json!({"label": [0, 1, 2]})).is_empty());
        let label = Type::Tuple { items: vec![Type::Base { name: Base::UInteger }; 2] };
        assert_eq!(
            invalid(&label, json!([0, -1])),
            "result[1] is -1; required: uinteger (a whole number from 0 to 2147483647)"
        );
        assert_eq!(invalid(&label, json!([0])), "result is [0]; required: [uinteger, uinteger]");
    }

    #[test]
    fn map() {
        let edit = reference("WorkspaceEdit");
        let text_edit =
            json!({"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "newText": ""});
        valid(&edit, json!({"changes": {"file:///a": [text_edit]}}));
        let found = invalid(&edit, json!({"changes": {"file:///a b": [{"newText": ""}]}}));
        assert_eq!(found, "result.changes[\"file:///a b\"][0].range is missing; required: Range");
        invalid(&edit, json!({"changes": []}));
        // Keys are judged only for a string enumeration.
        let by_kind =
            Type::Map { key: Box::new(reference("MarkupKind")), value: Box::new(Type::Base { name: Base::Boolean }) };
        valid(&by_kind, json!({"markdown": true}));
        assert_eq!(
            invalid(&by_kind, json!({"html": true})),
            "result.html is \"html\"; required: MarkupKind: \"plaintext\" | \"markdown\""
        );
        let by_number = Type::Map {
            key: Box::new(Type::Base { name: Base::Integer }),
            value: Box::new(Type::Base { name: Base::Boolean }),
        };
        valid(&by_number, json!({"x": true}));
    }

    #[test]
    fn and() {
        let both = Type::And { items: vec![reference("TextDocumentIdentifier"), reference("Position")] };
        valid(&both, json!({"uri": "file:///a", "line": 0, "character": 0}));
        assert_eq!(
            check(&both, json!({"uri": "file:///a", "line": 0})),
            ["result.character is missing; required: uinteger"]
        );
        assert_eq!(check(&both, json!({"line": 0})).len(), 2);
    }

    #[test]
    fn structures_inherit_through_extends_and_mixins() {
        // HoverParams extends TextDocumentPositionParams and mixes in WorkDoneProgressParams.
        let params = reference("HoverParams");
        let position = json!({"line": 0, "character": 0});
        valid(&params, json!({"textDocument": {"uri": "file:///a"}, "position": position}));
        valid(&params, json!({"textDocument": {"uri": "file:///a"}, "position": position, "workDoneToken": "t"}));
        assert_eq!(
            check(&params, json!({"position": position})),
            ["result.textDocument is missing; required: TextDocumentIdentifier"]
        );
        let found = invalid(
            &params,
            json!({"textDocument": {"uri": "file:///a"}, "position": position, "workDoneToken": true}),
        );
        assert!(
            found.starts_with("result.workDoneToken is true; required: ProgressToken (integer | string)"),
            "{found}"
        );
        // Recursively: VersionedTextDocumentIdentifier extends TextDocumentIdentifier.
        assert_eq!(
            check(&reference("VersionedTextDocumentIdentifier"), json!({"version": 1})),
            ["result.uri is missing; required: DocumentUri"]
        );
    }

    #[test]
    fn literals() {
        let integer: Type = serde_json::from_value(json!({"kind": "integerLiteral", "value": 3})).unwrap();
        valid(&integer, json!(3));
        invalid(&integer, serde_json::from_str("3.0").unwrap());
        let boolean: Type = serde_json::from_value(json!({"kind": "booleanLiteral", "value": true})).unwrap();
        valid(&boolean, json!(true));
        invalid(&boolean, json!(false));
        let string: Type = serde_json::from_value(json!({"kind": "stringLiteral", "value": "create"})).unwrap();
        valid(&string, json!("create"));
        assert_eq!(invalid(&string, json!("delete")), "result is \"delete\"; required: \"create\"");
    }

    #[test]
    fn messages_and_their_types() {
        let m = model();
        assert!(m.result("textDocument/hover").is_some());
        assert!(m.result("lspcheck/probe").is_none());
        // Proposed in 3.17.
        assert!(m.result("textDocument/inlineCompletion").is_none());
        assert_eq!(describe(m.server_params("window/logMessage", false).unwrap()), "LogMessageParams");
        assert_eq!(describe(m.server_params("$/progress", false).unwrap()), "ProgressParams");
        assert!(m.server_params("window/workDoneProgress/create", true).is_some());
        // Sent only by the client.
        assert!(m.server_params("textDocument/didOpen", false).is_none());
        assert!(m.server_params("textDocument/hover", true).is_none());
        // No params type.
        assert!(m.server_params("workspace/codeLens/refresh", true).is_none());
        let missing = missing(m.server_params("window/logMessage", false).unwrap(), "params");
        assert_eq!(missing.count, 1);
        assert_eq!(missing.first[0].to_string(), "params is missing; required: LogMessageParams");
    }

    #[test]
    fn work_is_bounded() {
        let tokens = json!({"data": vec![1; 1000]});
        let ty = reference("SemanticTokens");
        assert!(validate_within(&ty, &tokens, "result", 2000).unwrap().is_empty());
        assert_eq!(validate_within(&ty, &tokens, "result", 500), Err(TooLarge));
    }

    #[test]
    fn many_violations_are_counted_and_a_few_kept() {
        let found = validate(&reference("SemanticTokens"), &json!({"data": vec![-1; 100]}), "result").unwrap();
        assert_eq!(found.count, 100);
        assert_eq!(found.first.len(), KEPT);
        assert_eq!(found.first[1].path, "result.data[1]");
    }

    #[test]
    fn long_values_are_shortened() {
        let found = invalid(&Type::Base { name: Base::Integer }, json!("x".repeat(500)));
        assert!(found.len() < 200 && found.contains('…'), "{found}");
    }
}
