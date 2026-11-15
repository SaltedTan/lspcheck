// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! M3's comparisons: what a recovery probe's answer keeps once projected,
//! and how two projections compare, every array as a multiset
//! ([design](../docs/source-mutations.md#recovery)).
//!
//! Pure functions only; the mutation scenario that asks the probes is in
//! [`crate::checks`].

use crate::message;
use crate::metamodel;
use crate::profile::Probe;
use serde_json::{Map, Value};

/// What a probe's answer says about the text, projected and in canonical
/// form: the result of `documentSymbol` or `foldingRange`, the items of a
/// full `textDocument/diagnostic` report, or the diagnostics of a
/// `publishDiagnostics` notification (`answer` is then its params). Two
/// answers are equal after projection when these values are equal. `None`
/// for a diagnostic report that is not full or has malformed projected fields.
pub fn project(probe: Probe, answer: &Value) -> Option<Value> {
    let projected = match probe {
        Probe::DocumentSymbol => each(answer, symbol),
        Probe::FoldingRange => {
            each(answer, |v| keep(v, &["startLine", "startCharacter", "endLine", "endCharacter", "kind"]))
        }
        Probe::Diagnostic => {
            if answer.get("kind").and_then(Value::as_str) != Some("full") {
                return None;
            }
            diagnostics(answer.get("items")?)?
        }
        Probe::PublishDiagnostics => diagnostics(answer.get("diagnostics")?)?,
    };
    Some(canonical(&projected))
}

/// Where a probe's projection sits in its answer, for a difference's path.
pub fn root(probe: Probe) -> &'static str {
    match probe {
        Probe::DocumentSymbol | Probe::FoldingRange => "result",
        Probe::Diagnostic => "result.items",
        Probe::PublishDiagnostics => "params.diagnostics",
    }
}

/// The first Error-severity diagnostic in a diagnostics probe's projection,
/// if it has one.
pub fn first_error(projected: &Value) -> Option<&Value> {
    projected.as_array()?.iter().find(|d| d.get("severity") == Some(&Value::from(1)))
}

/// `f` applied to each element of an array; anything else as it is.
fn each(v: &Value, f: impl Fn(&Value) -> Value) -> Value {
    match v {
        Value::Array(items) => Value::Array(items.iter().map(f).collect()),
        other => other.clone(),
    }
}

/// An object with only the members named; anything else as it is.
fn keep(v: &Value, names: &[&str]) -> Value {
    let Value::Object(o) = v else { return v.clone() };
    Value::Object(names.iter().filter_map(|&n| o.get(n).map(|m| (n.to_string(), m.clone()))).collect())
}

/// A range's start and end, each only its line and character.
fn range(v: &Value) -> Value {
    let Value::Object(o) = v else { return v.clone() };
    let mut out = Map::new();
    for end in ["start", "end"] {
        if let Some(p) = o.get(end) {
            out.insert(end.into(), keep(p, &["line", "character"]));
        }
    }
    Value::Object(out)
}

/// A `DocumentSymbol`'s name, kind, ranges and children, or a
/// `SymbolInformation`'s name, kind, range and container name.
fn symbol(v: &Value) -> Value {
    let Value::Object(o) = v else { return v.clone() };
    let mut out = keep(v, &["name", "kind"]);
    let out_object = out.as_object_mut().expect("kept from an object");
    if let Some(location) = o.get("location") {
        if let Some(container) = o.get("containerName") {
            out_object.insert("containerName".into(), container.clone());
        }
        let projected = match location.get("range") {
            Some(r) => serde_json::json!({"range": range(r)}),
            None => keep(location, &[]),
        };
        out_object.insert("location".into(), projected);
        return out;
    }
    for name in ["range", "selectionRange"] {
        if let Some(r) = o.get(name) {
            out_object.insert(name.into(), range(r));
        }
    }
    if let Some(children) = o.get("children") {
        out_object.insert("children".into(), each(children, symbol));
    }
    out
}

fn diagnostics(v: &Value) -> Option<Value> {
    let items = v.as_array()?;
    let integer = |v: &Value| v.as_number().and_then(message::integer_value);
    for item in items {
        let range = item.get("range")?;
        for end in ["start", "end"] {
            let position = range.get(end)?;
            for axis in ["line", "character"] {
                let n = integer(position.get(axis)?)?;
                if !(0..=i64::from(i32::MAX)).contains(&n) {
                    return None;
                }
            }
        }
        if item.get("severity").is_some_and(|v| !integer(v).is_some_and(|n| (1..=4).contains(&n)))
            || item.get("code").is_some_and(|v| !v.is_string() && !integer(v).is_some_and(|n| i32::try_from(n).is_ok()))
            || item.get("source").is_some_and(|v| !v.is_string())
        {
            return None;
        }
    }
    Some(Value::Array(items.iter().map(diagnostic).collect()))
}

/// A diagnostic's range, severity, code and source.
fn diagnostic(v: &Value) -> Value {
    let mut out = keep(v, &["severity", "code", "source"]);
    if let (Some(o), Some(r)) = (out.as_object_mut(), v.get("range")) {
        o.insert("range".into(), range(r));
    }
    out
}

/// `v` with every integer spelled the same way (`1.0` as `1`), every
/// object's members in key order, and every array sorted by its elements'
/// canonical JSON, so that values equal as multisets, recursively, are equal.
fn canonical(v: &Value) -> Value {
    match v {
        Value::Number(n) => message::integer_value(n).map_or_else(|| v.clone(), Value::from),
        Value::Array(items) => {
            let mut items: Vec<(String, Value)> = items
                .iter()
                .map(|item| {
                    let item = canonical(item);
                    (item.to_string(), item)
                })
                .collect();
            items.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Array(items.into_iter().map(|(_, item)| item).collect())
        }
        Value::Object(o) => {
            let mut members: Vec<(&String, &Value)> = o.iter().collect();
            members.sort_by_key(|(k, _)| *k);
            Value::Object(members.into_iter().map(|(k, m)| (k.clone(), canonical(m))).collect())
        }
        other => other.clone(),
    }
}

/// Where two canonical projections first differ, and what each has there
/// (`None` where one has nothing). An array's elements have no place, so
/// its path step is `[*]`: an element one has and the other lacks, or the
/// first such pair, compared further.
#[derive(Debug, Clone, PartialEq)]
pub struct Difference {
    pub path: String,
    pub baseline: Option<Value>,
    pub answer: Option<Value>,
}

impl Difference {
    /// The difference for a detail: the path from `root`, and both values
    /// shortened as K2 shortens them.
    pub fn describe(&self, root: &str, answer: &str) -> String {
        let shown = |v: &Option<Value>| v.as_ref().map_or_else(|| "nothing".to_string(), metamodel::shorten_value);
        format!(
            "at {root}{}: {} in the baseline, {} in {answer}",
            self.path,
            shown(&self.baseline),
            shown(&self.answer)
        )
    }
}

/// The first difference between two canonical projections, if they differ.
pub fn difference(baseline: &Value, answer: &Value) -> Option<Difference> {
    let mut path = String::new();
    differ(baseline, answer, &mut path)
}

fn differ(b: &Value, a: &Value, path: &mut String) -> Option<Difference> {
    if b == a {
        return None;
    }
    let here = |path: &str, b: Option<&Value>, a: Option<&Value>| Difference {
        path: path.to_string(),
        baseline: b.cloned(),
        answer: a.cloned(),
    };
    match (b, a) {
        (Value::Object(bo), Value::Object(ao)) => {
            let mut keys: Vec<&String> = bo.keys().chain(ao.keys()).collect();
            keys.sort();
            keys.dedup();
            for key in keys {
                let (x, y) = (bo.get(key), ao.get(key));
                if x == y {
                    continue;
                }
                path.push('.');
                path.push_str(key);
                return match (x, y) {
                    (Some(x), Some(y)) => differ(x, y, path),
                    _ => Some(here(path, x, y)),
                };
            }
            None
        }
        (Value::Array(ba), Value::Array(aa)) => {
            // Both are sorted by their elements' JSON: walk them together.
            let (bk, ak): (Vec<String>, Vec<String>) =
                (ba.iter().map(Value::to_string).collect(), aa.iter().map(Value::to_string).collect());
            let (mut i, mut j) = (0, 0);
            let (mut only_b, mut only_a) = (None, None);
            while i < ba.len() || j < aa.len() {
                match (bk.get(i), ak.get(j)) {
                    (Some(x), Some(y)) if x == y => (i, j) = (i + 1, j + 1),
                    (Some(x), Some(y)) if x < y => {
                        only_b.get_or_insert(&ba[i]);
                        i += 1;
                    }
                    (Some(_), None) => {
                        only_b.get_or_insert(&ba[i]);
                        i += 1;
                    }
                    _ => {
                        only_a.get_or_insert(&aa[j]);
                        j += 1;
                    }
                }
            }
            path.push_str("[*]");
            match (only_b, only_a) {
                (Some(x), Some(y)) => differ(x, y, path),
                (x, y) => Some(here(path, x, y)),
            }
        }
        _ => Some(here(path, Some(b), Some(a))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn range(sl: u64, sc: u64, el: u64, ec: u64) -> Value {
        json!({"start": {"line": sl, "character": sc}, "end": {"line": el, "character": ec}})
    }

    fn symbol(name: &str, line: u64, children: Vec<Value>) -> Value {
        json!({"name": name, "kind": 12, "range": range(line, 0, line, 9), "selectionRange": range(line, 0, line, 3),
               "children": children})
    }

    fn same(probe: Probe, a: &Value, b: &Value) -> bool {
        project(probe, a).unwrap() == project(probe, b).unwrap()
    }

    #[test]
    fn symbols_compare_as_multisets_with_their_children() {
        let a = symbol("a", 0, vec![symbol("x", 0, vec![]), symbol("y", 0, vec![symbol("z", 0, vec![])])]);
        let b = symbol("b", 1, vec![]);
        let shuffled_a = symbol("a", 0, vec![symbol("y", 0, vec![symbol("z", 0, vec![])]), symbol("x", 0, vec![])]);
        assert!(same(Probe::DocumentSymbol, &json!([a, b]), &json!([b, shuffled_a])));
        // A grandchild renamed is a difference, found through both levels.
        let renamed = symbol("a", 0, vec![symbol("x", 0, vec![]), symbol("y", 0, vec![symbol("w", 0, vec![])])]);
        let (before, after) = (
            project(Probe::DocumentSymbol, &json!([a, b])).unwrap(),
            project(Probe::DocumentSymbol, &json!([renamed, b])).unwrap(),
        );
        let d = difference(&before, &after).unwrap();
        assert_eq!(d.path, "[*].children[*].children[*].name");
        assert_eq!((d.baseline, d.answer), (Some(json!("z")), Some(json!("w"))));
        assert_eq!(
            d_describe(&before, &after),
            "at result[*].children[*].children[*].name: \"z\" in the baseline, \"w\" in the answer"
        );
    }

    fn d_describe(b: &Value, a: &Value) -> String {
        difference(b, a).unwrap().describe(root(Probe::DocumentSymbol), "the answer")
    }

    #[test]
    fn duplicates_count() {
        let (x, y) = (symbol("x", 0, vec![]), symbol("y", 1, vec![]));
        assert!(same(Probe::DocumentSymbol, &json!([x, y, x]), &json!([x, x, y])));
        assert!(!same(Probe::DocumentSymbol, &json!([x, x, y]), &json!([x, y, y])));
        assert!(!same(Probe::DocumentSymbol, &json!([x, y]), &json!([x, y, y])));
        let (two, three) = (
            project(Probe::DocumentSymbol, &json!([x, y])).unwrap(),
            project(Probe::DocumentSymbol, &json!([x, y, y])).unwrap(),
        );
        let d = difference(&two, &three).unwrap();
        assert_eq!((d.path.as_str(), d.baseline, d.answer), ("[*]", None, Some(project_one(&y))));
        assert!(difference(&three, &two).unwrap().answer.is_none());
        // Children duplicated too.
        let twice = symbol("p", 2, vec![x.clone(), x.clone()]);
        let once = symbol("p", 2, vec![x.clone()]);
        assert!(!same(Probe::DocumentSymbol, &json!([twice]), &json!([once])));
    }

    fn project_one(symbol: &Value) -> Value {
        project(Probe::DocumentSymbol, &json!([symbol])).unwrap()[0].clone()
    }

    #[test]
    fn what_may_vary_is_dropped_and_what_describes_the_text_is_kept() {
        let base = symbol("a", 0, vec![symbol("x", 0, vec![])]);
        let mut varied = base.clone();
        varied["detail"] = json!("answer 7");
        varied["tags"] = json!([1]);
        varied["deprecated"] = json!(true);
        varied["data"] = json!({"any": 1});
        varied["lspcheckExtra"] = json!({"standin": true});
        varied["range"]["start"]["extra"] = json!(1);
        varied["children"][0]["detail"] = json!("child detail");
        assert!(same(Probe::DocumentSymbol, &json!([base]), &json!([varied])));
        for (pointer, value) in [
            ("/name", json!("b")),
            ("/kind", json!(13)),
            ("/range/end/character", json!(8)),
            ("/selectionRange/start/line", json!(1)),
            ("/children", json!([])),
        ] {
            let mut changed = base.clone();
            *changed.pointer_mut(pointer).unwrap() = value;
            assert!(!same(Probe::DocumentSymbol, &json!([base]), &json!([changed])), "{pointer}");
        }
        // Integers compare by value, however they are spelled.
        let spelled: Value =
            serde_json::from_str(&json!([base]).to_string().replace("\"kind\":12", "\"kind\":12.0")).unwrap();
        assert_ne!(spelled, json!([base]));
        assert!(same(Probe::DocumentSymbol, &json!([base]), &spelled));
    }

    #[test]
    fn symbol_information_keeps_its_range_and_container() {
        let info = |uri: &str, container: &str, line: u64| {
            json!({"name": "a", "kind": 12, "containerName": container, "deprecated": false,
                   "location": {"uri": uri, "range": range(line, 0, line, 1)}})
        };
        assert!(same(Probe::DocumentSymbol, &json!([info("file:///a", "m", 0)]), &json!([info("file:///A", "m", 0)])));
        assert!(!same(Probe::DocumentSymbol, &json!([info("file:///a", "m", 0)]), &json!([info("file:///a", "n", 0)])));
        assert!(!same(Probe::DocumentSymbol, &json!([info("file:///a", "m", 0)]), &json!([info("file:///a", "m", 1)])));
        assert!(same(Probe::DocumentSymbol, &Value::Null, &Value::Null));
        assert!(!same(Probe::DocumentSymbol, &Value::Null, &json!([])));
    }

    #[test]
    fn folding_ranges_keep_their_lines_characters_and_kind() {
        let fold = |end: u64, text: &str| {
            json!({"startLine": 0, "startCharacter": 3, "endLine": end, "endCharacter": 1, "kind": "region",
                   "collapsedText": text})
        };
        let (a, b) = (fold(4, "…"), fold(9, "{…}"));
        assert!(same(Probe::FoldingRange, &json!([a, b]), &json!([fold(9, "other"), fold(4, "more")])));
        assert!(!same(Probe::FoldingRange, &json!([a]), &json!([fold(5, "…")])));
        let mut kindless = a.clone();
        kindless.as_object_mut().unwrap().remove("kind");
        assert!(!same(Probe::FoldingRange, &json!([a]), &json!([kindless])));
    }

    #[test]
    fn diagnostics_keep_range_severity_code_and_source() {
        let d = |message: &str, severity: u64| {
            json!({"range": range(1, 2, 1, 3), "severity": severity, "code": "E1", "source": "s", "message": message,
                   "data": {"x": 1}, "relatedInformation": [], "codeDescription": {"href": "https://x"}, "tags": [1]})
        };
        let report = |items: Vec<Value>, id: &str| json!({"kind": "full", "resultId": id, "items": items});
        let publish =
            |items: Vec<Value>, version: u64| json!({"uri": "file:///a", "version": version, "diagnostics": items});
        assert!(same(Probe::Diagnostic, &report(vec![d("a", 2)], "1"), &report(vec![d("b", 2)], "2")));
        assert!(same(
            Probe::PublishDiagnostics,
            &publish(vec![d("a", 2), d("b", 1)], 1),
            &publish(vec![d("c", 1), d("d", 2)], 9)
        ));
        assert!(!same(Probe::PublishDiagnostics, &publish(vec![d("a", 2)], 1), &publish(vec![d("a", 1)], 1)));
        let mut coded = d("a", 2);
        coded["code"] = json!(7);
        assert!(!same(Probe::PublishDiagnostics, &publish(vec![d("a", 2)], 1), &publish(vec![coded], 1)));
        // An unchanged report says nothing about the diagnostics.
        assert_eq!(project(Probe::Diagnostic, &json!({"kind": "unchanged", "resultId": "1"})), None);
        // Error severity is found in either kind of projection.
        let errors = project(Probe::Diagnostic, &report(vec![d("a", 2), d("b", 1)], "1")).unwrap();
        assert_eq!(first_error(&errors).unwrap()["severity"], 1);
        let warnings = project(Probe::PublishDiagnostics, &publish(vec![d("a", 2)], 1)).unwrap();
        assert_eq!(first_error(&warnings), None);
        let spelled: Value = serde_json::from_str(
            &publish(vec![d("a", 1)], 1).to_string().replace("\"severity\":1", "\"severity\":1.0"),
        )
        .unwrap();
        assert!(first_error(&project(Probe::PublishDiagnostics, &spelled).unwrap()).is_some());
    }

    #[test]
    fn recovery_rejects_malformed_diagnostic_evidence() {
        let valid = json!({"range": range(0, 0, 0, 1), "message": "word"});
        let mut invalid = vec![Value::Null, json!({}), json!([null]), json!([{}])];
        for end in ["start", "end"] {
            let mut missing = valid.clone();
            missing["range"].as_object_mut().unwrap().remove(end);
            invalid.push(json!([missing]));
            for axis in ["line", "character"] {
                let mut missing = valid.clone();
                missing["range"][end].as_object_mut().unwrap().remove(axis);
                invalid.push(json!([missing]));
                for bad in [Value::Null, json!("0"), json!(-1), json!(0.5), json!(2147483648u64)] {
                    let mut item = valid.clone();
                    item["range"][end][axis] = bad;
                    invalid.push(json!([item]));
                }
            }
        }
        for (field, bad) in [
            ("severity", Value::Null),
            ("severity", json!(0)),
            ("severity", json!(5)),
            ("severity", json!(1.5)),
            ("severity", json!("1")),
            ("code", Value::Null),
            ("code", json!(false)),
            ("code", json!(1.5)),
            ("code", json!(2147483648u64)),
            ("source", Value::Null),
            ("source", json!(1)),
        ] {
            let mut item = valid.clone();
            item[field] = bad;
            invalid.push(json!([item]));
        }
        for items in invalid {
            assert!(project(Probe::Diagnostic, &json!({"kind": "full", "items": items})).is_none(), "{items}");
            assert!(project(Probe::PublishDiagnostics, &json!({"diagnostics": items})).is_none(), "{items}");
        }
        for items in [
            json!([]),
            json!([valid]),
            json!([{
                "range": range(0, 0, 0, 1), "severity": 4, "code": -2147483648i64, "source": "s"
            }]),
        ] {
            assert!(project(Probe::Diagnostic, &json!({"kind": "full", "items": items})).is_some());
            assert!(project(Probe::PublishDiagnostics, &json!({"diagnostics": items})).is_some());
        }
    }

    #[test]
    fn differences_name_missing_members_and_shorten_values() {
        let a = json!([{"name": "a", "kind": 12}]);
        let b = json!([{"name": "a"}]);
        let (pa, pb) = (project(Probe::DocumentSymbol, &a).unwrap(), project(Probe::DocumentSymbol, &b).unwrap());
        let d = difference(&pa, &pb).unwrap();
        assert_eq!((d.path.as_str(), &d.baseline, &d.answer), ("[*].kind", &Some(json!(12)), &None));
        assert_eq!(
            d.describe("result", "the terminal probe's answer"),
            "at result[*].kind: 12 in the baseline, nothing in the terminal probe's answer"
        );
        let long = project(Probe::DocumentSymbol, &json!([{"name": "x".repeat(100)}])).unwrap();
        let described = difference(&pb, &long).unwrap().describe("result", "the answer");
        assert!(described.contains(&format!("\"{}…", "x".repeat(59))), "{described}");
        assert_eq!(difference(&pa, &pa), None);
        // A shape that isn't the expected one is compared as it is.
        let odd = project(Probe::DocumentSymbol, &json!({"symbols": 1})).unwrap();
        assert_eq!(difference(&pa, &odd).unwrap().path, "");
    }
}
