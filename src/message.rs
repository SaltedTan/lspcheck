// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! JSON-RPC 2.0 messages, keeping each id exactly as written.
//!
//! serde_json is built with `arbitrary_precision`, so a number keeps its
//! lexical form: an id sent as `7` and returned as `7.0` or `"7"` is a
//! different id, as JSON-RPC requires.

use serde_json::{Map, Value};

/// A request id as written on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Id {
    /// An integer, in its exact lexical form (`7`, `-1`, `9007199254740993`).
    Int(String),
    /// A string.
    Str(String),
    /// `null`, which JSON-RPC uses for errors about unreadable requests.
    Null,
    /// Anything else (a fraction, an exponent, a boolean, an object), as raw JSON.
    Other(String),
}

impl Id {
    pub fn int(n: i64) -> Self {
        Id::Int(n.to_string())
    }

    pub fn from_value(v: &Value) -> Self {
        match v {
            Value::String(s) => Id::Str(s.clone()),
            Value::Null => Id::Null,
            Value::Number(n) => {
                let text = n.to_string();
                let digits = text.strip_prefix('-').unwrap_or(&text);
                if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                    Id::Int(text)
                } else {
                    Id::Other(text)
                }
            }
            other => Id::Other(other.to_string()),
        }
    }

    pub fn to_value(&self) -> Value {
        match self {
            Id::Int(text) | Id::Other(text) => {
                serde_json::from_str(text).expect("an id's lexical form is valid JSON")
            }
            Id::Str(s) => Value::String(s.clone()),
            Id::Null => Value::Null,
        }
    }
}

impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Id::Int(t) | Id::Other(t) => f.write_str(t),
            Id::Str(s) => write!(f, "{s:?}"),
            Id::Null => f.write_str("null"),
        }
    }
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
            obj.get("jsonrpc")
                .map_or("missing".to_string(), Value::to_string)
        ));
    }
    match (obj.get("method"), obj.get("id")) {
        (Some(Value::String(method)), Some(id)) => Kind::Request {
            id: Id::from_value(id),
            method: method.clone(),
        },
        (Some(Value::String(method)), None) => Kind::Notification {
            method: method.clone(),
        },
        (Some(other), _) => Kind::Invalid(format!("\"method\" is {other}, not a string")),
        (None, Some(id)) => {
            let outcome = match (obj.get("result"), obj.get("error")) {
                (Some(result), None) => Outcome::Result(result.clone()),
                (None, Some(error)) => Outcome::Error {
                    code: error.get("code").and_then(integer_code),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    error: error.clone(),
                },
                (Some(_), Some(_)) => {
                    Outcome::Malformed("has both \"result\" and \"error\"".into())
                }
                (None, None) => Outcome::Malformed("has neither \"result\" nor \"error\"".into()),
            };
            Kind::Response {
                id: Id::from_value(id),
                outcome,
            }
        }
        (None, None) => Kind::Invalid("has neither \"method\" nor \"id\"".into()),
    }
}

fn integer_code(v: &Value) -> Option<i64> {
    let n = v.as_number()?;
    let text = n.to_string();
    let digits = text.strip_prefix('-').unwrap_or(&text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
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
    fn ids_keep_their_lexical_form() {
        let m = Message::parse(br#"{"jsonrpc":"2.0","id":7.0,"result":null}"#).unwrap();
        assert_eq!(m.response_id(), Some(&Id::Other("7.0".into())));
        let m =
            Message::parse(br#"{"jsonrpc":"2.0","id":9007199254740993,"result":null}"#).unwrap();
        assert_eq!(m.response_id(), Some(&Id::Int("9007199254740993".into())));
        let m = Message::parse(br#"{"jsonrpc":"2.0","id":"7","result":null}"#).unwrap();
        assert_eq!(m.response_id(), Some(&Id::Str("7".into())));
        assert_ne!(Id::Str("7".into()), Id::int(7));
        assert_eq!(Id::Int("-3".into()).to_value().to_string(), "-3");
    }

    #[test]
    fn classifies_messages() {
        let k = |s: &str| Message::parse(s.as_bytes()).unwrap().kind;
        assert!(matches!(
            k(r#"{"jsonrpc":"2.0","id":1,"method":"m"}"#),
            Kind::Request { .. }
        ));
        assert!(matches!(
            k(r#"{"jsonrpc":"2.0","method":"m"}"#),
            Kind::Notification { .. }
        ));
        assert!(matches!(
            k(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"x"}}"#),
            Kind::Response {
                outcome: Outcome::Error {
                    code: Some(-32601),
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            k(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601.5,"message":"x"}}"#),
            Kind::Response {
                outcome: Outcome::Error { code: None, .. },
                ..
            }
        ));
        assert!(matches!(
            k(r#"{"jsonrpc":"2.0","id":1,"result":1,"error":{}}"#),
            Kind::Response {
                outcome: Outcome::Malformed(_),
                ..
            }
        ));
        assert!(matches!(k(r#"{"id":1,"result":null}"#), Kind::Invalid(_)));
        assert!(matches!(k(r#"[]"#), Kind::Invalid(_)));
        assert!(Message::parse(b"{").is_err());
    }
}
