// Copyright 2026 The lspcheck Authors.
// SPDX-License-Identifier: Apache-2.0

//! The terminal summary, the JSON report, JUnit XML and a Markdown summary.

use crate::checks::{self, Area, Basis, CheckResult, ScenarioRun, Verdict};
use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;

/// The JSON report's schema identifier.
pub const SCHEMA: &str = "lspcheck-report-v1";

/// Results in catalogue order.
pub fn ordered(runs: &[ScenarioRun]) -> Vec<&CheckResult> {
    let mut all: Vec<&CheckResult> = runs.iter().flat_map(|r| &r.results).collect();
    all.sort_by_key(|r| checks::CATALOGUE.iter().position(|c| c.id == r.id));
    all
}

/// The process exit status for a run: 1 when a MUST check (or with `strict`,
/// any check) failed, 2 when lspcheck itself hit an error, otherwise 0.
pub fn exit_code(runs: &[ScenarioRun], strict: bool) -> i32 {
    let results = ordered(runs);
    let failed = |r: &CheckResult| r.verdict == Verdict::Fail;
    if results.iter().any(|r| failed(r) && checks::info(r.id).basis == Basis::Must) {
        return 1;
    }
    if results.iter().any(|r| r.verdict == Verdict::Error) {
        return 2;
    }
    if strict && results.iter().any(|r| failed(r)) {
        return 1;
    }
    0
}

/// The counts line the terminal report ends with, shared with the Markdown
/// summary so the two can't disagree.
pub fn counts_line(results: &[&CheckResult]) -> String {
    let count = |v: Verdict| results.iter().filter(|r| r.verdict == v).count();
    let failed_by =
        |b: Basis| results.iter().filter(|r| r.verdict == Verdict::Fail && checks::info(r.id).basis == b).count();
    format!(
        "{} checks: {} pass, {} fail ({} MUST, {} SHOULD, {} POLICY), {} skipped, {} not exercised, {} errors",
        results.len(),
        count(Verdict::Pass),
        count(Verdict::Fail),
        failed_by(Basis::Must),
        failed_by(Basis::Should),
        failed_by(Basis::Policy),
        count(Verdict::Skip),
        count(Verdict::NotExercised),
        count(Verdict::Error),
    )
}

pub fn print_terminal(out: &mut impl Write, server: &str, runs: &[ScenarioRun]) -> std::io::Result<()> {
    writeln!(out, "lspcheck {} — {server}", env!("CARGO_PKG_VERSION"))?;
    let results = ordered(runs);
    for r in &results {
        let c = checks::info(r.id);
        writeln!(out, "  {:<5} {:<3} {:<6}  {}", r.verdict.label(), c.id, c.basis.label(), c.title)?;
        if !r.detail.is_empty() {
            writeln!(out, "                     {}", r.detail)?;
        }
        if matches!(r.verdict, Verdict::Fail | Verdict::Error)
            && let Some((label, path)) = transcript(r, runs)
        {
            writeln!(out, "                     {label}: {}", path.display())?;
        }
    }
    writeln!(out, "{}", counts_line(&results))
}

pub fn json_report(server: &[String], runs: &[ScenarioRun]) -> Value {
    let results: Vec<Value> = ordered(runs)
        .into_iter()
        .map(|r| {
            let c = checks::info(r.id);
            json!({
                "id": r.id,
                "area": c.area,
                "basis": c.basis,
                "title": c.title,
                "spec": checks::spec_url(c),
                "verdict": r.verdict,
                "detail": r.detail,
                "scenario": r.scenario,
            })
        })
        .collect();
    json!({
        "schema": SCHEMA,
        "lspcheck": env!("CARGO_PKG_VERSION"),
        "server": server,
        "host": {"os": std::env::consts::OS, "arch": std::env::consts::ARCH},
        "results": results,
    })
}

/// A JUnit XML report: one testsuite per check area, testcases in catalogue
/// order. The `type` of a `<failure>` carries its basis; JUnit has no
/// severity, so `--strict` still decides the process exit status.
pub fn junit_report(server: &[String], runs: &[ScenarioRun]) -> String {
    let results = ordered(runs);
    let mut suites: Vec<(Area, Vec<&CheckResult>)> = Vec::new();
    for r in &results {
        let area = checks::info(r.id).area;
        match suites.iter_mut().find(|(a, _)| *a == area) {
            Some((_, list)) => list.push(r),
            None => suites.push((area, vec![r])),
        }
    }

    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(&format!(
        "<testsuites name=\"lspcheck\" tests=\"{}\" failures=\"{}\" errors=\"{}\" skipped=\"{}\">\n",
        results.len(),
        verdict_count(&results, Verdict::Fail),
        verdict_count(&results, Verdict::Error),
        skipped_count(&results),
    ));
    for (area, list) in &suites {
        out.push_str(&format!(
            "  <testsuite name=\"{}\" tests=\"{}\" failures=\"{}\" errors=\"{}\" skipped=\"{}\">\n",
            xml_attr(area.label()),
            list.len(),
            verdict_count(list, Verdict::Fail),
            verdict_count(list, Verdict::Error),
            skipped_count(list),
        ));
        out.push_str("    <properties>\n");
        out.push_str(&format!(
            "      <property name=\"lspcheck\" value=\"{}\"/>\n",
            xml_attr(env!("CARGO_PKG_VERSION"))
        ));
        out.push_str(&format!("      <property name=\"server\" value=\"{}\"/>\n", xml_attr(&server.join(" "))));
        out.push_str("    </properties>\n");
        for r in list {
            testcase(&mut out, r, runs);
        }
        out.push_str("  </testsuite>\n");
    }
    out.push_str("</testsuites>\n");
    out
}

fn verdict_count(results: &[&CheckResult], verdict: Verdict) -> usize {
    results.iter().filter(|r| r.verdict == verdict).count()
}

fn skipped_count(results: &[&CheckResult]) -> usize {
    results.iter().filter(|r| matches!(r.verdict, Verdict::Skip | Verdict::NotExercised)).count()
}

fn testcase(out: &mut String, r: &CheckResult, runs: &[ScenarioRun]) {
    let c = checks::info(r.id);
    out.push_str(&format!(
        "    <testcase classname=\"{}\" name=\"{}\"",
        xml_attr(&format!("lspcheck.{}", c.area.label())),
        xml_attr(&format!("{} {}", c.id, c.title))
    ));
    match r.verdict {
        Verdict::Pass => out.push_str("/>\n"),
        Verdict::Fail | Verdict::Error => {
            let (tag, kind) = match r.verdict {
                Verdict::Fail => ("failure", c.basis.label()),
                _ => ("error", "error"),
            };
            let message = match r.detail.lines().next() {
                Some(line) if !line.is_empty() => line,
                _ if r.verdict == Verdict::Fail => "failed",
                _ => "error",
            };
            out.push_str(">\n");
            out.push_str(&format!("      <{tag} type=\"{}\" message=\"{}\">", xml_attr(kind), xml_attr(message)));
            out.push_str(&xml_text(&r.detail));
            out.push('\n');
            out.push_str(&format!("spec: {}", xml_text(&checks::spec_url(c))));
            if let Some((label, path)) = transcript(r, runs) {
                out.push('\n');
                out.push_str(&format!("{label}: {}", xml_text(&path.display().to_string())));
            }
            out.push_str(&format!("</{tag}>\n"));
            out.push_str("    </testcase>\n");
        }
        Verdict::Skip | Verdict::NotExercised => {
            let message = if r.detail.is_empty() {
                match r.verdict {
                    Verdict::Skip => "skipped",
                    _ => "not exercised",
                }
            } else {
                &r.detail
            };
            out.push_str(">\n");
            out.push_str(&format!("      <skipped message=\"{}\"/>\n", xml_attr(message)));
            out.push_str("    </testcase>\n");
        }
    }
}

/// A Markdown summary for a CI step summary: failures and errors first, then
/// the rest, with every cell escaped so server text can't change the table.
pub fn markdown_report(server: &[String], runs: &[ScenarioRun]) -> String {
    let results = ordered(runs);
    let mut out = String::new();
    out.push_str(&format!(
        "## lspcheck {}: {} pass, {} fail\n\n",
        env!("CARGO_PKG_VERSION"),
        verdict_count(&results, Verdict::Pass),
        verdict_count(&results, Verdict::Fail),
    ));
    out.push_str(&format!(
        "Server: {} · {} {}\n\n",
        code_span(&server.join(" ")),
        std::env::consts::OS,
        std::env::consts::ARCH,
    ));
    out.push_str(&counts_line(&results));
    out.push_str("\n\n| Verdict | Check | Basis | Title | Detail |\n| --- | --- | --- | --- | --- |\n");

    let interesting = |r: &CheckResult| matches!(r.verdict, Verdict::Fail | Verdict::Error);
    let rows: Vec<&CheckResult> = results
        .iter()
        .copied()
        .filter(|r| interesting(r))
        .chain(results.iter().copied().filter(|r| !interesting(r)))
        .collect();
    for r in rows {
        let c = checks::info(r.id);
        out.push_str(&format!(
            "| {} | [{}]({}) | {} | {} | {} |\n",
            verdict_cell(r.verdict),
            c.id,
            checks::spec_url(c),
            c.basis.label(),
            markdown_cell(c.title),
            markdown_cell(&r.detail),
        ));
    }

    let failures: Vec<_> =
        results.iter().copied().filter(|r| interesting(r)).filter_map(|r| Some((r, transcript(r, runs)?))).collect();
    if !failures.is_empty() {
        out.push_str("\nTranscripts:\n\n");
        for (r, (label, path)) in failures {
            out.push_str(&format!("- {}: {label}: {}\n", r.id, code_span(&path.display().to_string())));
        }
    }
    out
}

fn transcript<'a>(r: &CheckResult, runs: &'a [ScenarioRun]) -> Option<(&'static str, &'a Path)> {
    if r.scenario == "all" {
        let path = runs.iter().find_map(|run| run.transcript.as_deref())?;
        Some(("transcripts", path.parent()?))
    } else {
        Some(("transcript", runs.iter().find(|run| run.name == r.scenario)?.transcript.as_deref()?))
    }
}

fn verdict_cell(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Pass => "✅ PASS",
        Verdict::Fail => "❌ FAIL",
        Verdict::Error => "⚠️ ERROR",
        Verdict::Skip => "⏭️ SKIP",
        Verdict::NotExercised => "➖ N/E",
    }
}

/// A table cell: escape what would change the table or the Markdown around
/// it, and turn line breaks into `<br>`.
fn markdown_cell(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '\\' => out.push_str("\\\\"),
            '`' => out.push_str("\\`"),
            '*' => out.push_str("\\*"),
            '_' => out.push_str("\\_"),
            '~' => out.push_str("\\~"),
            '[' => out.push_str("\\["),
            ']' => out.push_str("\\]"),
            '<' => out.push_str("\\<"),
            '>' => out.push_str("\\>"),
            '|' => out.push_str("\\|"),
            c => out.push(c),
        }
    }
    out.replace("\r\n", "<br>").replace(['\r', '\n'], "<br>")
}

/// A code span that survives any run of backticks in `text`.
fn code_span(text: &str) -> String {
    let text = text.replace('\r', "\\r").replace('\n', "\\n");
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest + 1);
    let pad = if text.starts_with('`') || text.starts_with(' ') || text.ends_with('`') || text.ends_with(' ') {
        " "
    } else {
        ""
    };
    format!("{fence}{pad}{text}{pad}{fence}")
}

/// A character XML 1.0 allows in content.
fn xml_allowed(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r')
        || matches!(c, '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
}

fn xml_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match if xml_allowed(c) { c } else { '\u{fffd}' } {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

fn xml_attr(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match if xml_allowed(c) { c } else { '\u{fffd}' } {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\n' => out.push_str("&#10;"),
            '\t' => out.push_str("&#9;"),
            '\r' => out.push_str("&#13;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn result(id: &'static str, verdict: Verdict, detail: &str) -> CheckResult {
        CheckResult { id, verdict, detail: detail.into(), scenario: "test", absent_at: Vec::new(), unless_missed: None }
    }

    fn scenario(results: Vec<CheckResult>) -> ScenarioRun {
        ScenarioRun { name: "test", results, observed: None, transcript: None }
    }

    fn server() -> Vec<String> {
        vec!["gopls".into(), "serve".into()]
    }

    /// Tags balance and every `&` starts a valid entity.
    fn assert_well_formed(xml: &str) {
        let bytes = xml.as_bytes();
        let mut stack: Vec<&str> = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'&' => {
                    let semi = xml[i..].find(';').expect("an `&` starts an entity");
                    let entity = &xml[i..i + semi + 1];
                    let named = ["&amp;", "&lt;", "&gt;", "&quot;", "&apos;"].contains(&entity);
                    let numeric = entity.strip_prefix("&#").and_then(|e| e.strip_suffix(';')).is_some_and(|e| {
                        if let Some(hex) = e.strip_prefix('x') {
                            !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit())
                        } else {
                            !e.is_empty() && e.chars().all(|c| c.is_ascii_digit())
                        }
                    });
                    assert!(named || numeric, "bad entity {entity:?}");
                    i += semi + 1;
                }
                b'<' if xml[i..].starts_with("<?") => {
                    i += xml[i..].find("?>").expect("a declaration ends") + 2;
                }
                b'<' => {
                    let end = xml[i..].find('>').expect("a tag ends");
                    let tag = &xml[i + 1..i + end];
                    let closing = tag.starts_with('/');
                    let self_closing = tag.ends_with('/');
                    let name = if closing { &tag[1..] } else { tag.split_whitespace().next().unwrap_or("") };
                    if closing {
                        assert_eq!(stack.pop(), Some(name), "mismatched close {name:?}");
                    } else if !self_closing {
                        stack.push(name);
                    }
                    i += end + 1;
                }
                _ => i += 1,
            }
        }
        assert!(stack.is_empty(), "unclosed tags: {stack:?}");
    }

    #[test]
    fn junit_groups_by_area_and_counts_every_verdict() {
        let xml = junit_report(
            &server(),
            &[scenario(vec![
                result("F1", Verdict::Pass, ""),
                result("F2", Verdict::Fail, "first line\nsecond line"),
                result("J1", Verdict::Error, "couldn't tell"),
                result("L1", Verdict::Skip, ""),
                result("L2", Verdict::NotExercised, ""),
                result("D1", Verdict::Pass, ""),
            ])],
        );
        assert!(xml.contains("<testsuites name=\"lspcheck\" tests=\"6\" failures=\"1\" errors=\"1\" skipped=\"2\">"));
        assert!(xml.contains("<testsuite name=\"Framing\" tests=\"2\" failures=\"1\" errors=\"0\" skipped=\"0\">"));
        assert!(xml.contains("<testsuite name=\"JSON-RPC\" tests=\"1\" failures=\"0\" errors=\"1\" skipped=\"0\">"));
        assert!(xml.contains("<testsuite name=\"Lifecycle\" tests=\"2\" failures=\"0\" errors=\"0\" skipped=\"2\">"));
        assert!(xml.contains("<testsuite name=\"Documents\" tests=\"1\" failures=\"0\" errors=\"0\" skipped=\"0\">"));
        // Areas the run has no checks from have no suite.
        assert!(!xml.contains("Cancellation"));
        assert!(xml.contains("<property name=\"lspcheck\" value=\""));
        assert!(xml.contains("<property name=\"server\" value=\"gopls serve\"/>"));
        assert!(xml.contains(&format!(
            "<testcase classname=\"lspcheck.Framing\" name=\"F2 {}\">",
            xml_attr(checks::info("F2").title)
        )));
        assert!(xml.contains("<failure type=\"MUST\" message=\"first line\">first line\nsecond line\nspec: "));
        assert!(xml.contains("<error type=\"error\" message=\"couldn&apos;t tell\">"));
        assert!(xml.contains("<skipped message=\"skipped\"/>"));
        assert!(xml.contains("<skipped message=\"not exercised\"/>"));
        assert!(xml.contains(
            "<testcase classname=\"lspcheck.Framing\" name=\"F1 Every byte on stdout belongs to a well-formed frame\"/>"
        ));
        assert_well_formed(&xml);
    }

    #[test]
    fn junit_failure_carries_the_basis_and_the_transcript() {
        let mut run = scenario(vec![result("L7", Verdict::Fail, "the process exited with status 0")]);
        run.transcript = Some(PathBuf::from("/tmp/out/lifecycle.jsonl"));
        let xml = junit_report(&server(), &[run]);
        let c = checks::info("L7");
        assert!(
            xml.contains(&format!("<testcase classname=\"lspcheck.Lifecycle\" name=\"L7 {}\">", xml_attr(c.title)))
        );
        assert!(xml.contains("<failure type=\"SHOULD\" message=\"the process exited with status 0\">"));
        assert!(xml.contains(&format!("spec: {}", checks::spec_url(c))));
        assert!(xml.contains("transcript: /tmp/out/lifecycle.jsonl</failure>"));
        assert_well_formed(&xml);
    }

    #[test]
    fn junit_escapes_text_attributes_and_control_characters() {
        let mut run = scenario(vec![result("L7", Verdict::Fail, "a & b < c > d \" e ' f \u{1} g \u{fffe}")]);
        run.transcript = None;
        let xml = junit_report(&["fast\n\tserver\r".into()], &[run]);
        assert!(xml.contains(
            "message=\"a &amp; b &lt; c &gt; d &quot; e &apos; f \u{fffd} g \u{fffd}\">\
             a &amp; b &lt; c &gt; d &quot; e &apos; f \u{fffd} g \u{fffd}\n"
        ));
        assert!(!xml.contains('\u{1}'));
        assert!(xml.contains("value=\"fast&#10;&#9;server&#13;\""));
        assert_well_formed(&xml);
    }

    #[test]
    fn markdown_puts_failures_and_errors_first_and_escapes_cells() {
        let text = markdown_report(
            &server(),
            &[scenario(vec![
                result("F1", Verdict::Pass, ""),
                result("L7", Verdict::Fail, "a | b `c` *d* _e_ [f] <g> h\\i\nnext &copy; &#169; &amp; ~~workspace~~"),
                result("J1", Verdict::Error, "no answer"),
                result("L1", Verdict::Skip, "not for this server"),
                result("D1", Verdict::NotExercised, ""),
            ])],
        );
        assert!(text.starts_with(&format!("## lspcheck {}: 1 pass, 1 fail\n", env!("CARGO_PKG_VERSION"))));
        assert!(text.contains("Server: `gopls serve` · "));
        assert!(
            text.contains(
                "5 checks: 1 pass, 1 fail (0 MUST, 1 SHOULD, 0 POLICY), 1 skipped, 1 not exercised, 1 errors"
            )
        );
        let header = text.find("| Verdict | Check | Basis | Title | Detail |").unwrap();
        let j1 = text[header..].find("[J1]").unwrap();
        let l7 = text[header..].find("[L7]").unwrap();
        let f1 = text[header..].find("[F1]").unwrap();
        let l1 = text[header..].find("[L1]").unwrap();
        let d1 = text[header..].find("[D1]").unwrap();
        assert!(j1 < l7 && l7 < f1 && f1 < l1 && l1 < d1, "{text}");
        assert!(text.contains(
            "| ❌ FAIL | [L7](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#exit) \
             | SHOULD | exit without shutdown ends the process with status 1 \
             | a \\| b \\`c\\` \\*d\\* \\_e\\_ \\[f\\] \\<g\\> h\\\\i<br>next &amp;copy; &amp;#169; &amp;amp; \\~\\~workspace\\~\\~ |"
        ), "{text}");
        assert!(text.contains("| ⚠️ ERROR | [J1]"));
        assert!(text.contains("| ✅ PASS | [F1]"));
        assert!(text.contains("| ⏭️ SKIP | [L1]"));
        assert!(text.contains("| ➖ N/E | [D1]"));
    }

    #[test]
    fn markdown_lists_transcripts_for_failures_and_errors() {
        let mut run = scenario(vec![result("L7", Verdict::Fail, "bad"), result("F1", Verdict::Pass, "")]);
        run.transcript = Some(PathBuf::from("/tmp/out/lifecycle.jsonl"));
        let text = markdown_report(&server(), &[run]);
        assert!(text.contains("\nTranscripts:\n\n- L7: transcript: `/tmp/out/lifecycle.jsonl`\n"), "{text}");
    }

    #[test]
    fn reports_reference_aggregate_evidence_only_for_failures_and_errors() {
        for verdict in [Verdict::Fail, Verdict::Error, Verdict::Pass, Verdict::Skip, Verdict::NotExercised] {
            for recorded in [false, true] {
                let mut aggregate = result("F1", verdict, "in framing: evidence");
                aggregate.scenario = "all";
                let runs = [
                    ScenarioRun { name: "unstarted", results: vec![], observed: None, transcript: None },
                    ScenarioRun {
                        name: "framing",
                        results: vec![],
                        observed: None,
                        transcript: recorded.then(|| PathBuf::from("evidence/framing.jsonl")),
                    },
                    ScenarioRun {
                        name: "lifecycle",
                        results: vec![],
                        observed: None,
                        transcript: recorded.then(|| PathBuf::from("evidence/lifecycle.jsonl")),
                    },
                    ScenarioRun { name: "all", results: vec![aggregate], observed: None, transcript: None },
                ];
                let expected = recorded && matches!(verdict, Verdict::Fail | Verdict::Error);
                let mut terminal = Vec::new();
                print_terminal(&mut terminal, "gopls serve", &runs).unwrap();
                let terminal = String::from_utf8(terminal).unwrap();
                let xml = junit_report(&server(), &runs);
                let markdown = markdown_report(&server(), &runs);
                assert_eq!(terminal.contains("transcripts: evidence\n"), expected, "{terminal}");
                assert_eq!(xml.contains("\ntranscripts: evidence</"), expected, "{xml}");
                assert_eq!(markdown.contains("- F1: transcripts: `evidence`\n"), expected, "{markdown}");
                assert!(!terminal.contains("transcript:"), "{terminal}");
                assert!(!xml.contains("transcript:"), "{xml}");
                assert!(!markdown.contains("transcript:"), "{markdown}");
            }
        }
    }

    #[test]
    fn code_spans_display_line_breaks_in_commands_and_paths() {
        let mut run = scenario(vec![result("L7", Verdict::Fail, "bad")]);
        run.transcript = Some(PathBuf::from("evidence\r\n\n# path`name.jsonl"));
        let text =
            markdown_report(&["python3".into(), "-c".into(), "print('ready')\r\n\n# server`script".into()], &[run]);
        assert!(text.contains("Server: ``python3 -c print('ready')\\r\\n\\n# server`script`` · "), "{text}");
        assert!(text.contains("- L7: transcript: ``evidence\\r\\n\\n# path`name.jsonl``\n"), "{text}");
    }

    #[test]
    fn code_spans_survive_backticks_and_padding() {
        let text = markdown_report(&["a`b``c".into()], &[scenario(vec![result("F1", Verdict::Pass, "")])]);
        assert!(text.contains("Server: ```a`b``c```"), "{text}");
        let text = markdown_report(&["`x`".into()], &[scenario(vec![result("F1", Verdict::Pass, "")])]);
        assert!(text.contains("Server: `` `x` ``"), "{text}");
    }
}
