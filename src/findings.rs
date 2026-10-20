//! Findings as text, JSON and SARIF 2.1.0.

use crate::Finding;
use serde_json::{Value, json};

fn tier(f: &Finding) -> Value {
    serde_json::to_value(f.tier).unwrap_or(Value::Null)
}

fn uri(f: &Finding) -> String {
    f.path.to_string_lossy().replace('\\', "/")
}

fn checks_json(f: &Finding) -> Vec<Value> {
    f.checks
        .iter()
        .map(|(name, value, threshold)| json!({"name": name, "value": value, "threshold": threshold}))
        .collect()
}

/// The line without the path prefix.
fn body(f: &Finding) -> String {
    let t = tier(f);
    let mut s = format!(
        "{} ({}, {}) {}",
        f.verdict,
        f.kind,
        t.as_str().unwrap_or_default(),
        f.message
    );
    if !f.checks.is_empty() {
        let cs: Vec<String> = f
            .checks
            .iter()
            .map(|(n, v, t)| format!("{n}={v:.2}/{t:.2}"))
            .collect();
        s.push_str("; ");
        s.push_str(&cs.join(", "));
    }
    if !f.evidence.is_empty() {
        s.push_str("; ");
        s.push_str(&f.evidence);
    }
    s
}

pub fn text(f: &Finding) -> String {
    format!("{}:{}-{}: {}", uri(f), f.start_line, f.end_line, body(f))
}

pub fn json(findings: &[Finding]) -> Value {
    Value::Array(
        findings
            .iter()
            .map(|f| {
                json!({
                    "path": uri(f),
                    "start_line": f.start_line,
                    "end_line": f.end_line,
                    "tier": tier(f),
                    "kind": f.kind,
                    "verdict": f.verdict,
                    "checks": checks_json(f),
                    "evidence": f.evidence,
                    "message": f.message,
                })
            })
            .collect(),
    )
}

/// What `reduce --ai` tells the agent to do with an item it left alone.
fn recommendation(verdict: &str) -> &'static str {
    match verdict {
        "review" => "Delete or reduce via rewrite",
        "rewrite" => "Reduce via rewrite",
        "fix" => "Fix via rewrite: it may contradict the code",
        "rename" => "Delete after renaming the code so the name says it",
        "stale" => "Fix or delete: the code it describes changed",
        _ => "Review",
    }
}

/// A policy check in words: what the judge's score measures, and which side of the threshold
/// means what.
fn signal(name: &str) -> &'static str {
    match name {
        "g1" => "P(contradicts the code); fix above",
        "api_quality" => "doc comment quality 0-1; delete below",
        "c5" => "documents the contract a caller needs, 0-3; delete below",
        "c2" => "information the code does not show, 0-3; delete below",
        "rename" => "P(a rename could replace it); rename above",
        "hazard" => "P(warns about a trap or hazard); keep at or above",
        "c3" => "explains why, 0-3; keep at or above",
        "leak" => "P(history or process the reader does not need); shorten above",
        _ => "score",
    }
}

/// One `reduce --ai` JSONL record.
pub fn ai_line(f: &Finding) -> Value {
    json!({
        "path": uri(f),
        "start_line": f.start_line,
        "end_line": f.end_line,
        "recommendation": recommendation(&f.verdict),
        "kind": f.kind,
        "comment": f.comment,
        "signals": f.checks.iter().map(|(name, value, threshold)| json!({
            "signal": signal(name),
            "value": (value * 100.0).round() / 100.0,
            "threshold": threshold,
        })).collect::<Vec<_>>(),
        "evidence": f.evidence,
    })
}

fn rule_id(f: &Finding) -> String {
    format!("comment/{}", f.verdict)
}

pub fn sarif(findings: &[Finding], version: &str) -> Value {
    let mut ids: Vec<String> = findings.iter().map(rule_id).collect();
    ids.sort();
    ids.dedup();
    let rules: Vec<Value> = ids
        .iter()
        .map(|id| json!({"id": id, "shortDescription": {"text": format!("Comment verdict: {}", &id[8..])}}))
        .collect();
    let results: Vec<Value> = findings
        .iter()
        .map(|f| {
            let level = if matches!(f.verdict.as_str(), "delete" | "reduce" | "stale") {
                "warning"
            } else {
                "note"
            };
            json!({
                "ruleId": rule_id(f),
                "level": level,
                "message": {"text": body(f)},
                "locations": [{"physicalLocation": {
                    "artifactLocation": {"uri": uri(f)},
                    "region": {"startLine": f.start_line, "endLine": f.end_line}}}],
                "properties": {
                    "tier": tier(f),
                    "kind": f.kind,
                    "checks": checks_json(f),
                    "evidence": f.evidence,
                },
            })
        })
        .collect();
    json!({
        "version": "2.1.0",
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "runs": [{"tool": {"driver": {"name": "commentreducr", "version": version, "rules": rules}},
                  "results": results}],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changeset::Tier;

    fn finding(verdict: &str, tier: Tier) -> Finding {
        Finding {
            path: "src/a.py".into(),
            start_line: 8,
            end_line: 10,
            tier,
            kind: "inline".into(),
            verdict: verdict.into(),
            checks: vec![("g1".into(), 0.5, 0.25)],
            evidence: "x".into(),
            message: "m".into(),
            comment: "# c".into(),
        }
    }

    #[test]
    fn ai_line_shape() {
        let v = ai_line(&finding("rewrite", Tier::Added));
        assert_eq!(v["recommendation"], "Reduce via rewrite");
        assert_eq!(v["comment"], "# c");
        assert_eq!(
            v["signals"][0]["signal"],
            "P(contradicts the code); fix above"
        );
        assert_eq!(v["path"], "src/a.py");
    }

    #[test]
    fn sarif_shape() {
        let fs = [
            finding("delete", Tier::Added),
            finding("review", Tier::Modified),
            finding("stale", Tier::ScopeChanged),
        ];
        let s = sarif(&fs, "1.2.3");
        let run = &s["runs"][0];
        assert_eq!(run["results"].as_array().unwrap().len(), 3);
        for r in run["results"].as_array().unwrap() {
            assert!(r["ruleId"].is_string() && r["level"].is_string());
            assert!(r["message"]["text"].is_string());
            assert!(r["locations"][0]["physicalLocation"]["region"]["startLine"].is_u64());
        }
        assert_eq!(run["results"][0]["level"], "warning");
        assert_eq!(run["results"][1]["level"], "note");
        assert_eq!(run["results"][2]["level"], "warning");
        let ids: Vec<_> = run["tool"]["driver"]["rules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["comment/delete", "comment/review", "comment/stale"]);
        assert_eq!(
            text(&fs[0]),
            "src/a.py:8-10: delete (inline, added) m; g1=0.50/0.25; x"
        );
        assert_eq!(json(&fs)[2]["tier"], "scope_changed");
    }
}
