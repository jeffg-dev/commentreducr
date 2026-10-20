//! Deterministic policy: question probabilities -> verdict. A line-for-line port of
//! `tools/bakeoff/policy.py`; `tests/fixtures/policy/cases.json` is the contract both pass.
use crate::judge::{Answers, Judgement};
use anyhow::Context;
use serde::Deserialize;

pub const THRESHOLDS_STRICT: &str = include_str!("schema/thresholds-strict-v1.json");

/// The meaning of each key is next to `DEFAULT_THRESHOLDS` in `policy.py`.
#[derive(Debug, Clone, Deserialize)]
pub struct Thresholds {
    pub g1: f64,
    pub api_delete: f64,
    pub api_rewrite: f64,
    pub c5_min: f64,
    pub c2_min: f64,
    pub rename: f64,
    pub hazard: f64,
    pub c3_min: f64,
    pub leak: f64,
    pub cap_lines: usize,
    pub margin: f64,
    pub use_holistic: bool,
    pub verify_preserves: f64,
    pub verify_adds: f64,
    pub verify_g1: f64,
    pub stale_g1: f64,
    /// Off: no verified rewrite; rewrite_shorter keeps the comment and fix_wrong_claim goes to
    /// review. Off by default since verified rewrites still introduced errors on the gold test
    /// split (PLAN.md risk table).
    pub rewrite: bool,
    /// `reduce --ai`: the review band (`margin`) is at least this wide, so near-threshold items
    /// go to the agent instead of being decided.
    pub ai_margin: f64,
}

impl Thresholds {
    pub fn strict() -> Thresholds {
        serde_json::from_str(THRESHOLDS_STRICT)
            .expect("embedded thresholds-strict-v1.json is valid")
    }

    /// The embedded thresholds with `cfg.judge_thresholds` merged over them; unknown keys fail.
    pub fn load(cfg: &crate::Config) -> anyhow::Result<Thresholds> {
        let mut th: serde_json::Value = serde_json::from_str(THRESHOLDS_STRICT)?;
        let Some(text) = &cfg.judge_thresholds else {
            return Ok(Thresholds::strict());
        };
        let over: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(text).context("judge_thresholds: not a JSON object")?;
        for (k, v) in over {
            anyhow::ensure!(th.get(&k).is_some(), "judge_thresholds: unknown key {k}");
            th[k] = v;
        }
        serde_json::from_value(th).context("judge_thresholds: bad value")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Keep,
    RewriteShorter,
    Delete,
    RenameInstead,
    FixWrongClaim,
    Review,
}

impl Verdict {
    const ALL: [Verdict; 6] = [
        Verdict::Keep,
        Verdict::RewriteShorter,
        Verdict::Delete,
        Verdict::RenameInstead,
        Verdict::FixWrongClaim,
        Verdict::Review,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Keep => "keep",
            Verdict::RewriteShorter => "rewrite_shorter",
            Verdict::Delete => "delete",
            Verdict::RenameInstead => "rename_instead",
            Verdict::FixWrongClaim => "fix_wrong_claim",
            Verdict::Review => "review",
        }
    }

    pub fn parse(s: &str) -> Option<Verdict> {
        Verdict::ALL.into_iter().find(|v| v.as_str() == s)
    }
}

/// A question (or a score option) the policy needs was missing or malformed: Python's KeyError.
struct Missing;

/// Records every (name, value, threshold, scale) comparison, like `decide`'s `checks` list.
/// The names are presentation only; policy.py has none.
struct Checks(Vec<(&'static str, f64, f64, f64)>);

impl Checks {
    fn lt(&mut self, name: &'static str, v: f64, t: f64, scale: f64) -> bool {
        self.0.push((name, v, t, scale));
        v < t
    }
    fn gt(&mut self, name: &'static str, v: f64, t: f64, scale: f64) -> bool {
        self.0.push((name, v, t, scale));
        v > t
    }
    fn named(&self) -> Vec<(String, f64, f64)> {
        self.0
            .iter()
            .map(|&(n, v, t, _)| (n.into(), v, t))
            .collect()
    }
}

fn p(a: &Answers, q: &str, opt: &str) -> Result<f64, Missing> {
    Ok(a.get(q).ok_or(Missing)?.get(opt).copied().unwrap_or(0.0))
}

fn e(a: &Answers, q: &str) -> Result<f64, Missing> {
    a.get(q).ok_or(Missing)?.iter().try_fold(0.0, |s, (k, p)| {
        Ok(s + k.parse::<i64>().map_err(|_| Missing)? as f64 * p)
    })
}

fn replaceable_weight(k: &str) -> Result<f64, Missing> {
    match k {
        "no" => Ok(1.0),
        "partly" => Ok(0.5),
        "fully" => Ok(0.0),
        _ => Err(Missing),
    }
}

/// policy.py::decide. `comment_kind`, `comment` and `code` (annotated_code) are the row fields it reads.
pub fn decide(
    answers: &Answers,
    comment_kind: &str,
    comment: &str,
    code: &str,
    th: &Thresholds,
) -> Verdict {
    decide_with_checks(answers, comment_kind, comment, code, th).0
}

/// `decide` plus the named (value, threshold) comparisons it made.
pub(crate) fn decide_with_checks(
    answers: &Answers,
    comment_kind: &str,
    comment: &str,
    code: &str,
    th: &Thresholds,
) -> (Verdict, Vec<(String, f64, f64)>) {
    if matches!(comment_kind, "todo" | "directive") {
        return (Verdict::Keep, Vec::new());
    }
    let mut checks = Checks(Vec::new());
    let v = decide_checked(answers, comment_kind, comment, th, &mut checks);
    (unit_guard(v, comment, code, th), checks.named())
}

/// The unit guard (evidence::unit_literal): never delete the only statement of a literal's unit.
fn unit_guard(v: Verdict, comment: &str, code: &str, th: &Thresholds) -> Verdict {
    if !matches!(v, Verdict::Delete | Verdict::RenameInstead)
        || crate::evidence::unit_literal(comment, code).is_none()
    {
        v
    } else if comment.matches('\n').count() + 1 > th.cap_lines {
        Verdict::RewriteShorter
    } else {
        Verdict::Keep
    }
}

fn decide_checked(
    answers: &Answers,
    comment_kind: &str,
    comment: &str,
    th: &Thresholds,
    checks: &mut Checks,
) -> Verdict {
    let Ok(v) = decide_inner(answers, comment_kind, comment, th, checks) else {
        return Verdict::Review;
    };
    if checks
        .0
        .iter()
        .any(|&(_, x, t, s)| (x - t).abs() < th.margin * s)
    {
        return Verdict::Review;
    }
    if th.use_holistic
        && let Some(a) = answers.get("verdict").filter(|a| !a.is_empty())
    {
        let max = a.values().copied().fold(f64::NEG_INFINITY, f64::max);
        let top: Vec<&str> = a
            .iter()
            .filter(|&(_, &p)| p == max)
            .map(|(k, _)| k.as_str())
            .collect();
        // An exact tie is review: a BTreeMap cannot reproduce Python max()'s tie-break.
        if top != [v.as_str()] {
            return Verdict::Review;
        }
    }
    v
}

fn decide_inner(
    a: &Answers,
    kind: &str,
    comment: &str,
    th: &Thresholds,
    c: &mut Checks,
) -> Result<Verdict, Missing> {
    if c.gt("g1", p(a, "g1_contradicts", "true")?, th.g1, 1.0) {
        return Ok(Verdict::FixWrongClaim);
    }
    if kind == "api_doc" {
        let c5 = e(a, "c5_contract")?;
        let rep = a.get("c9_replaceable").ok_or(Missing)?;
        let rep_sum = rep
            .iter()
            .try_fold(0.0, |s, (k, p)| Ok(s + replaceable_weight(k)? * p))?;
        let q = (2.0 * e(a, "c2_info_gain")? / 3.0
            + 3.0 * c5 / 3.0
            + 2.0 * e(a, "c8_durability")? / 3.0
            + 0.5 * rep_sum)
            / 7.5;
        if c.lt("api_quality", q, th.api_delete, 1.0) || c.lt("c5", c5, th.c5_min, 3.0) {
            return Ok(Verdict::Delete);
        }
        return Ok(if c.lt("api_quality", q, th.api_rewrite, 1.0) {
            Verdict::RewriteShorter
        } else {
            Verdict::Keep
        });
    }
    if c.lt("c2", e(a, "c2_info_gain")?, th.c2_min, 3.0) {
        return Ok(Verdict::Delete);
    }
    if c.gt("rename", p(a, "c9_replaceable", "fully")?, th.rename, 1.0) {
        return Ok(Verdict::RenameInstead);
    }
    let hazard = p(a, "hazard", "true")?;
    if c.lt("hazard", hazard, th.hazard, 1.0) && c.lt("c3", e(a, "c3_rationale")?, th.c3_min, 3.0) {
        return Ok(Verdict::Delete);
    }
    if (hazard >= th.hazard && c.gt("leak", p(a, "leak", "true")?, th.leak, 1.0))
        || comment.matches('\n').count() + 1 > th.cap_lines
    {
        return Ok(Verdict::RewriteShorter);
    }
    Ok(Verdict::Keep)
}

/// policy.py::judge_verdict: one-hot judges (or a judgement with no answers) bypass the policy.
pub fn judge_verdict(
    j: &Judgement,
    one_hot: bool,
    comment_kind: &str,
    comment: &str,
    code: &str,
    th: &Thresholds,
) -> Verdict {
    judge_verdict_with_checks(j, one_hot, comment_kind, comment, code, th).0
}

/// `judge_verdict` plus the named checks; a bypassed policy has none.
pub(crate) fn judge_verdict_with_checks(
    j: &Judgement,
    one_hot: bool,
    comment_kind: &str,
    comment: &str,
    code: &str,
    th: &Thresholds,
) -> (Verdict, Vec<(String, f64, f64)>) {
    if matches!(comment_kind, "todo" | "directive") {
        return (Verdict::Keep, Vec::new());
    }
    if one_hot || j.answers.is_empty() {
        let v = j.holistic.as_deref().and_then(Verdict::parse);
        let v = unit_guard(v.unwrap_or(Verdict::Review), comment, code, th);
        return (v, Vec::new());
    }
    decide_with_checks(&j.answers, comment_kind, comment, code, th)
}

/// Tier-2 outcome: does the changed code contradict an unchanged comment?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stale {
    Stale,
    Fresh,
}

impl Stale {
    pub fn as_str(self) -> &'static str {
        match self {
            Stale::Stale => "stale",
            Stale::Fresh => "fresh",
        }
    }
}

/// Tier 2, for an item the deterministic gate passed. `Stale` only when P(g1_contradicts) >
/// th.stale_g1; a missing answer is `Fresh`. There is no "possibly stale" band: on 83 real
/// findings it had precision 0.03 (docs/decisions/changeset-validation.md).
pub fn stale_verdict(answers: &Answers, th: &Thresholds) -> (Stale, Vec<(String, f64, f64)>) {
    let Ok(g1) = p(answers, "g1_contradicts", "true") else {
        return (Stale::Fresh, Vec::new());
    };
    let v = if g1 > th.stale_g1 {
        Stale::Stale
    } else {
        Stale::Fresh
    };
    (v, vec![("g1".into(), g1, th.stale_g1)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// The same file runs through policy.py in tools/test_bakeoff_policy.py::test_shared_cases.
    #[test]
    fn shared_cases() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/policy/cases.json"
        );
        let cases: Vec<Value> =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert!(cases.len() >= 20);
        for case in &cases {
            let name = case["name"].as_str().unwrap();
            let mut th: Value = serde_json::from_str(THRESHOLDS_STRICT).unwrap();
            for (k, v) in case["thresholds_override"].as_object().unwrap() {
                th[k] = v.clone();
            }
            let th: Thresholds = serde_json::from_value(th).unwrap();
            let answers: Answers = serde_json::from_value(case["answers"].clone()).unwrap();
            let got = decide(
                &answers,
                case["comment_kind"].as_str().unwrap(),
                case["comment"].as_str().unwrap(),
                case["annotated_code"].as_str().unwrap_or(""),
                &th,
            );
            assert_eq!(got.as_str(), case["expected"].as_str().unwrap(), "{name}");
        }
    }

    #[test]
    fn judge_verdict_bypass() {
        let th = Thresholds::strict();
        let j = |answers: Answers, holistic: Option<&str>| Judgement {
            answers,
            holistic: holistic.map(str::to_string),
            input_tokens: None,
            latency_ms: 0,
        };
        let c2_zero = Answers::from([
            ("g1_contradicts".into(), [("true".into(), 0.0)].into()),
            ("c2_info_gain".into(), [("0".into(), 1.0)].into()),
        ]);
        assert_eq!(
            judge_verdict(
                &j(Answers::new(), Some("delete")),
                false,
                "inline",
                "# x",
                "",
                &th
            ),
            Verdict::Delete
        );
        assert_eq!(
            judge_verdict(&j(Answers::new(), None), false, "inline", "# x", "", &th),
            Verdict::Review
        );
        assert_eq!(
            judge_verdict(
                &j(c2_zero.clone(), Some("keep")),
                true,
                "inline",
                "# x",
                "",
                &th
            ),
            Verdict::Keep
        );
        assert_eq!(
            judge_verdict(
                &j(c2_zero.clone(), Some("keep")),
                false,
                "inline",
                "# x",
                "",
                &th
            ),
            Verdict::Delete
        );
        assert_eq!(
            judge_verdict(&j(c2_zero, Some("delete")), false, "todo", "# x", "", &th),
            Verdict::Keep
        );
        // The unit guard holds on the bypass path too.
        assert_eq!(
            judge_verdict(
                &j(Answers::new(), Some("delete")),
                true,
                "inline",
                "// 5 seconds",
                "gcTime: 5000,",
                &th
            ),
            Verdict::Keep
        );
        for v in Verdict::ALL {
            assert_eq!(Verdict::parse(v.as_str()), Some(v));
        }
    }

    #[test]
    fn stale_verdict_rules() {
        let th = Thresholds {
            stale_g1: 0.5,
            ..Thresholds::strict()
        };
        let a = |g1: f64| Answers::from([("g1_contradicts".into(), [("true".into(), g1)].into())]);
        assert_eq!(stale_verdict(&a(0.9), &th).0, Stale::Stale);
        assert_eq!(stale_verdict(&a(0.5), &th).0, Stale::Fresh);
        assert_eq!(stale_verdict(&Answers::new(), &th).0, Stale::Fresh);
        assert_eq!(stale_verdict(&a(0.9), &th).1, [("g1".into(), 0.9, 0.5)]);
    }

    #[test]
    fn decide_names_its_checks() {
        let th = Thresholds::strict();
        let answers = Answers::from([
            ("g1_contradicts".into(), [("true".into(), 0.0)].into()),
            ("c2_info_gain".into(), [("0".into(), 1.0)].into()),
        ]);
        let (v, checks) = decide_with_checks(&answers, "inline", "# x", "", &th);
        assert_eq!(v, Verdict::Delete);
        let names: Vec<_> = checks.iter().map(|c| c.0.as_str()).collect();
        assert_eq!(names, ["g1", "c2"]);
    }
}
