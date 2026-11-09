// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! JSON-RPC 2.0 messages.
//!
//! JSON-RPC requires a response to carry "the same value" as its request's
//! id. A string and a number are never the same value, and numbers compare
//! by exact decimal value, so `1`, `1.0` and `10e-1` are the same id. serde_json
//! is built with `arbitrary_precision`, so no id passes through a lossy `f64`,
//! and each id keeps the spelling it arrived with as evidence.

use serde_json::{Map, Value};
use std::hash::{Hash, Hasher};

/// A request id.
#[derive(Debug, Clone)]
pub enum Id {
    /// A number: its canonical exact value (see [`canonical_number`]) and its spelling.
    Num {
        canon: String,
        raw: String,
    },
    Str(String),
    /// `null`, which JSON-RPC uses for errors about unreadable requests.
    Null,
    /// A boolean, object or array (not valid ids), as raw JSON.
    Other(String),
}

impl PartialEq for Id {
    fn eq(&self, other: &Id) -> bool {
        match (self, other) {
            (Id::Num { canon: a, .. }, Id::Num { canon: b, .. }) => a == b,
            (Id::Str(a), Id::Str(b)) | (Id::Other(a), Id::Other(b)) => a == b,
            (Id::Null, Id::Null) => true,
            _ => false,
        }
    }
}

impl Eq for Id {}

impl Hash for Id {
    fn hash<H: Hasher>(&self, h: &mut H) {
        match self {
            Id::Num { canon, .. } => (0u8, canon).hash(h),
            Id::Str(s) => (1u8, s).hash(h),
            Id::Null => 2u8.hash(h),
            Id::Other(s) => (3u8, s).hash(h),
        }
    }
}

impl Id {
    pub fn int(n: i64) -> Self {
        let raw = n.to_string();
        Id::Num { canon: canonical_number(&raw).expect("an integer is a JSON number"), raw }
    }

    pub fn from_value(v: &Value) -> Self {
        match v {
            Value::String(s) => Id::Str(s.clone()),
            Value::Null => Id::Null,
            Value::Number(n) => {
                let raw = n.to_string();
                match canonical_number(&raw) {
                    Some(canon) => Id::Num { canon, raw },
                    None => Id::Other(raw),
                }
            }
            other => Id::Other(other.to_string()),
        }
    }

    /// The id as JSON, spelled as it was received or created.
    pub fn to_value(&self) -> Value {
        match self {
            Id::Num { raw, .. } | Id::Other(raw) => serde_json::from_str(raw).expect("an id's spelling is valid JSON"),
            Id::Str(s) => Value::String(s.clone()),
            Id::Null => Value::Null,
        }
    }

    /// The JSON type, for messages: "number", "string", ...
    pub fn type_name(&self) -> &'static str {
        match self {
            Id::Num { .. } => "number",
            Id::Str(_) => "string",
            Id::Null => "null",
            Id::Other(_) => "another JSON type",
        }
    }

    /// Whether this is a whole number. LSP requests use integer ids.
    pub fn is_integer(&self) -> bool {
        self.integer().is_some()
    }

    /// Whether this is a number in LSP's `integer` range (signed 32-bit),
    /// which numeric ids must be.
    pub fn is_lsp_integer(&self) -> bool {
        self.integer().is_some_and(|n| i32::try_from(n).is_ok())
    }

    fn integer(&self) -> Option<i64> {
        match self {
            Id::Num { raw, .. } => serde_json::from_str::<Value>(raw).ok().and_then(|v| integer_value(v.as_number()?)),
            _ => None,
        }
    }
}

/// Whether two ids are only loosely equal: the same number spelled as a string
/// and a number, or numbers that become equal when rounded through `f64` but
/// are not exactly equal. Such a reply kept the right value but the wrong JSON
/// type or precision, which J1 reports.
pub fn loosely_equal(a: &Id, b: &Id) -> bool {
    match (a, b) {
        (Id::Str(s), Id::Num { canon, .. }) | (Id::Num { canon, .. }, Id::Str(s)) => {
            canonical_number(s).is_some_and(|c| &c == canon)
        }
        // `f64` is how a server that parses ids as doubles loses precision.
        (Id::Num { canon: a, raw: ra }, Id::Num { canon: b, raw: rb }) => {
            if a == b {
                return false;
            }
            match (ra.parse::<f64>(), rb.parse::<f64>()) {
                (Ok(x), Ok(y)) => x.is_finite() && x == y,
                _ => false,
            }
        }
        _ => false,
    }
}

impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Id::Num { raw, .. } | Id::Other(raw) => f.write_str(raw),
            Id::Str(s) => write!(f, "{s:?}"),
            Id::Null => f.write_str("null"),
        }
    }
}

/// The exact value of a JSON number as `[-]DIGITSeEXP` with no leading or
/// trailing zeros in DIGITS (zero is `0e0`). `None` if `text` isn't a JSON
/// number or its exponent doesn't fit in 64 bits.
pub fn canonical_number(text: &str) -> Option<String> {
    let (negative, rest) = match text.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, text),
    };
    let (mantissa, exp) = match rest.find(['e', 'E']) {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => (rest, "0"),
    };
    // Zero is zero however large its exponent, so recognise it before the
    // exponent is parsed (which overflows for such numbers).
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let zeros = |t: &str| t.bytes().all(|b| b == b'0');
    let unsigned = exp.strip_prefix(['+', '-']).unwrap_or(exp);
    if !int.is_empty()
        && zeros(int)
        && zeros(frac)
        && !(mantissa.contains('.') && frac.is_empty())
        && !unsigned.is_empty()
        && unsigned.bytes().all(|b| b.is_ascii_digit())
    {
        return Some("0e0".into());
    }
    let exp = exp.parse::<i64>().ok()?;
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if int.is_empty() || !int.bytes().all(|b| b.is_ascii_digit()) || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if mantissa.contains('.') && frac.is_empty() {
        return None;
    }
    let mut digits: String = format!("{int}{frac}").trim_start_matches('0').to_string();
    let mut exp = exp.checked_sub(i64::try_from(frac.len()).ok()?)?;
    if digits.is_empty() {
        return Some("0e0".into());
    }
    while digits.ends_with('0') {
        digits.pop();
        exp = exp.checked_add(1)?;
    }
    Some(format!("{}{digits}e{exp}", if negative { "-" } else { "" }))
}

/// A JSON number's value as an `i64`, if it is an integer that fits.
pub fn integer_value(n: &serde_json::Number) -> Option<i64> {
    let canon = canonical_number(&n.to_string())?;
    let (mantissa, exp) = canon.rsplit_once('e')?;
    let exp: u32 = exp.parse().ok()?;
    if exp > 18 {
        return None;
    }
    format!("{mantissa}{}", "0".repeat(exp as usize)).parse().ok()
}

/// How a response ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Result(Value),
    /// `code` is `None` when the error's code isn't an integer.
    Error {
        code: Option<i64>,
        message: Option<String>,
        error: Value,
    },
    /// Both `result` and `error`, or neither: not a valid response.
    Malformed(String),
}

/// What kind of message a body is.
#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Request {
        id: Id,
        method: String,
    },
    Notification {
        method: String,
    },
    Response {
        id: Id,
        outcome: Outcome,
    },
    /// Valid JSON but not a JSON-RPC 2.0 message.
    Invalid(String),
}

/// A parsed message with its JSON.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub value: Value,
    pub kind: Kind,
}

impl Message {
    /// Parses a frame body. Returns `Err` when it isn't JSON at all.
    pub fn parse(body: &[u8]) -> Result<Message, String> {
        let value: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
        Ok(Message::classify(value))
    }

    pub fn classify(value: Value) -> Message {
        let kind = match &value {
            Value::Object(obj) => classify_object(obj),
            Value::Array(_) => Kind::Invalid("a batch (JSON array), which LSP doesn't use".into()),
            _ => Kind::Invalid("not a JSON object".into()),
        };
        Message { value, kind }
    }

    pub fn method(&self) -> Option<&str> {
        match &self.kind {
            Kind::Request { method, .. } | Kind::Notification { method } => Some(method),
            _ => None,
        }
    }

    pub fn response_id(&self) -> Option<&Id> {
        match &self.kind {
            Kind::Response { id, .. } => Some(id),
            _ => None,
        }
    }
}

fn classify_object(obj: &Map<String, Value>) -> Kind {
    if obj.get("jsonrpc") != Some(&Value::String("2.0".into())) {
        return Kind::Invalid(format!(
            "\"jsonrpc\" is {}, not \"2.0\"",
            obj.get("jsonrpc").map_or("missing".to_string(), Value::to_string)
        ));
    }
    match (obj.get("method"), obj.get("id")) {
        (Some(Value::String(method)), Some(id)) => Kind::Request { id: Id::from_value(id), method: method.clone() },
        (Some(Value::String(method)), None) => Kind::Notification { method: method.clone() },
        (Some(other), _) => Kind::Invalid(format!("\"method\" is {other}, not a string")),
        (None, Some(id)) => {
            let outcome = match (obj.get("result"), obj.get("error")) {
                (Some(result), None) => Outcome::Result(result.clone()),
                (None, Some(error)) => Outcome::Error {
                    code: error.get("code").and_then(integer_code),
                    message: error.get("message").and_then(Value::as_str).map(str::to_string),
                    error: error.clone(),
                },
                (Some(_), Some(_)) => Outcome::Malformed("has both \"result\" and \"error\"".into()),
                (None, None) => Outcome::Malformed("has neither \"result\" nor \"error\"".into()),
            };
            Kind::Response { id: Id::from_value(id), outcome }
        }
        (None, None) => Kind::Invalid("has neither \"method\" nor \"id\"".into()),
    }
}

fn integer_code(v: &Value) -> Option<i64> {
    integer_value(v.as_number()?)
}

/// Builds a request body.
pub fn request(id: &Id, method: &str, params: Option<Value>) -> Value {
    let mut obj = Map::new();
    obj.insert("jsonrpc".into(), "2.0".into());
    obj.insert("id".into(), id.to_value());
    obj.insert("method".into(), method.into());
    if let Some(p) = params {
        obj.insert("params".into(), p);
    }
    Value::Object(obj)
}

/// Builds a notification body.
pub fn notification(method: &str, params: Option<Value>) -> Value {
    let mut obj = Map::new();
    obj.insert("jsonrpc".into(), "2.0".into());
    obj.insert("method".into(), method.into());
    if let Some(p) = params {
        obj.insert("params".into(), p);
    }
    Value::Object(obj)
}

/// Builds a successful response body.
pub fn response(id: &Id, result: Value) -> Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id.to_value(), "result": result})
}

/// Builds an error response body.
pub fn error_response(id: &Id, code: i64, message: &str) -> Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id.to_value(), "error": {"code": code, "message": message}})
}

/// Error codes from JSON-RPC 2.0 and LSP 3.17.
pub mod codes {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const SERVER_NOT_INITIALIZED: i64 = -32002;
    pub const REQUEST_CANCELLED: i64 = -32800;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_ids_compare_by_exact_value() {
        let id = |s: &str| {
            Message::parse(format!(r#"{{"jsonrpc":"2.0","id":{s},"result":null}}"#).as_bytes())
                .unwrap()
                .response_id()
                .cloned()
                .unwrap()
        };
        assert_eq!(id("7.0"), Id::int(7));
        assert_eq!(id("70e-1"), Id::int(7));
        assert_eq!(id("0.7e1"), Id::int(7));
        assert_ne!(id("7.5"), Id::int(7));
        assert_ne!(id("\"7\""), Id::int(7));
        assert_eq!(id("-0"), id("0"));
        // No rounding through f64: these differ only in the 17th digit.
        assert_ne!(id("9007199254740993"), id("9007199254740992"));
        assert_eq!(id("7.0").to_string(), "7.0", "the received spelling is kept");
        assert_eq!(Id::int(-3).to_value().to_string(), "-3");
    }

    #[test]
    fn loosely_equal_ids_cover_type_and_precision_slips() {
        let num = |s: &str| {
            Message::parse(format!(r#"{{"jsonrpc":"2.0","id":{s},"result":null}}"#).as_bytes())
                .unwrap()
                .response_id()
                .cloned()
                .unwrap()
        };
        // A server that turned the numeric string into a number, or the other way round.
        assert!(loosely_equal(&num("\"123\""), &Id::int(123)));
        assert!(loosely_equal(&Id::int(123), &num("\"123\"")));
        assert!(!loosely_equal(&num("\"lspcheck-1\""), &Id::int(1)));
        assert!(!loosely_equal(&num("\"7\""), &Id::int(8)));
        // Two different numbers that a double can't tell apart.
        assert!(loosely_equal(&num("9007199254740993"), &num("9007199254740992")));
        assert!(!loosely_equal(&num("7.0"), &num("7")));
        assert!(!loosely_equal(&num("\"7\""), &num("\"7\"")));
    }

    #[test]
    fn whole_numbers_are_the_only_valid_numeric_request_ids() {
        assert!(Id::int(7).is_integer());
        assert!(Id::int(-0).is_integer());
        assert!(!Id::from_value(&serde_json::json!({"not": "a number"})).is_integer());
        let fraction = Message::parse(br#"{"jsonrpc":"2.0","id":1.5,"method":"m"}"#).unwrap();
        let Kind::Request { id, .. } = fraction.kind else { panic!("not a request") };
        assert!(!id.is_integer());
        let huge = Message::parse(br#"{"jsonrpc":"2.0","id":1e400,"method":"m"}"#).unwrap();
        let Kind::Request { id, .. } = huge.kind else { panic!("not a request") };
        assert!(!id.is_integer());
    }

    #[test]
    fn canonical_numbers() {
        for (text, want) in [
            ("0", Some("0e0")),
            ("-1", Some("-1e0")),
            ("100", Some("1e2")),
            ("1.50", Some("15e-1")),
            ("1e400", Some("1e400")),
            ("01", Some("1e0")),
            ("1.", None),
            ("abc", None),
        ] {
            assert_eq!(canonical_number(text).as_deref(), want, "{text}");
        }
        let n = |s: &str| serde_json::from_str::<Value>(s).unwrap().as_number().cloned().unwrap();
        assert_eq!(integer_value(&n("-32601.0")), Some(-32601));
        assert_eq!(integer_value(&n("-32601.5")), None);
        assert_eq!(integer_value(&n("1e30")), None);
        // Zero, whatever its exponent; an exponent that big on anything else isn't an integer.
        for zero in ["0e9223372036854775808", "-0.0e-99999999999999999999", "0E+99999999999999999999"] {
            assert_eq!(integer_value(&n(zero)), Some(0), "{zero}");
        }
        assert_eq!(integer_value(&n("1e9223372036854775808")), None);
    }

    #[test]
    fn classifies_messages() {
        let k = |s: &str| Message::parse(s.as_bytes()).unwrap().kind;
        assert!(matches!(k(r#"{"jsonrpc":"2.0","id":1,"method":"m"}"#), Kind::Request { .. }));
        assert!(matches!(k(r#"{"jsonrpc":"2.0","method":"m"}"#), Kind::Notification { .. }));
        assert!(matches!(
            k(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"x"}}"#),
            Kind::Response { outcome: Outcome::Error { code: Some(-32601), .. }, .. }
        ));
        assert!(matches!(
            k(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601.5,"message":"x"}}"#),
            Kind::Response { outcome: Outcome::Error { code: None, .. }, .. }
        ));
        assert!(matches!(
            k(r#"{"jsonrpc":"2.0","id":1,"result":1,"error":{}}"#),
            Kind::Response { outcome: Outcome::Malformed(_), .. }
        ));
        assert!(matches!(k(r#"{"id":1,"result":null}"#), Kind::Invalid(_)));
        assert!(matches!(k(r#"[]"#), Kind::Invalid(_)));
        assert!(Message::parse(b"{").is_err());
    }
}
