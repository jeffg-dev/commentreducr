"""Deterministic policy: question probabilities -> verdict (docs/reduce-redesign.md, Proposal 1, steps 2-6).

Step 1 (deterministic deletes) is out of scope. answers = {qid: {option: prob}}.
"""
import json
import re
from pathlib import Path

# g1: P(g1_contradicts) above -> fix_wrong_claim. api_delete / api_rewrite: api_doc quality below ->
# delete / rewrite_shorter. c5_min: api_doc E[c5] below -> delete. c2_min: inline E[c2] below -> delete.
# rename: P(c9 == fully) above -> rename_instead. hazard + c3_min: P(hazard) and E[c3] both below -> delete.
# leak: P(leak) above with a hazard -> rewrite_shorter. cap_lines: kept inline comments longer -> rewrite_shorter.
# margin: abstain when a deciding value is this close to its threshold. use_holistic: abstain when the
# verdict question disagrees. verify_*: the crate's rewrite verification (not used here).
# Shared with the crate (src/policy.rs), so Python and Rust cannot drift.
DEFAULT_THRESHOLDS = json.loads(
    (Path(__file__).parent / ".." / ".." / "src" / "schema" / "thresholds-strict-v1.json").read_text())
VERDICTS = ["keep", "rewrite_shorter", "delete", "rename_instead", "fix_wrong_claim"]
_REPLACEABLE = {"no": 1.0, "partly": 0.5, "fully": 0.0}

# Unit guard: a comment that names a unit ("5 seconds", "in ms", "1小时"; a bare "second" is an ordinal)
# over code with a number literal is often the only statement of that literal's unit, so the policy never
# deletes or renames it away. Mirrors src/evidence.rs::unit_literal regex for regex (both Unicode-aware).
_UNIT_WORD = re.compile(
    r"(?i:\b(?:milli|micro|nano)seconds?\b|\bseconds\b|\bmsecs?\b|\bms\b|\bminutes?\b|\bhours?\b|\bdays?\b"
    r"|\b(?:kilo|mega|giga)?bytes?\b|\bpx\b|\bpercent(?:age)?\b)|\b[KMGT]i?B\b|毫秒|秒|分钟|小时|天|字节")
_UNIT_AFTER_NUMBER = re.compile(r"(?i:\d+\s*(?:(?:s|secs?|second|mins?|h|hrs?)\b|%))")  # 30s, 1 second, 24h, 50%
_NUMBER = re.compile(r"\b\d+(?:\.\d+)?")


def unit_literal(comment, code):
    """Why the unit guard fires (the unit and the code's first number literal), or None."""
    u = _UNIT_WORD.search(comment) or _UNIT_AFTER_NUMBER.search(comment)
    n = _NUMBER.search(code)
    return f"unit {u.group(0).strip()} next to literal {n.group(0)}" if u and n else None


def _p(answers, q, opt="true"):
    return answers[q].get(opt, 0.0)


def _e(answers, q):
    return sum(int(k) * p for k, p in answers[q].items())


def decide(answers, row, th):
    """Return one of VERDICTS or "review"."""
    kind = row.get("comment_kind", "inline")
    if kind in ("todo", "directive"):
        return "keep"
    checks = []  # (value, threshold, scale): scale 3 for 0-3 expectations

    def lt(v, t, scale=1):
        checks.append((v, t, scale))
        return v < t

    def gt(v, t, scale=1):
        checks.append((v, t, scale))
        return v > t

    try:
        v = _decide(answers, row, th, kind, lt, gt)
    except KeyError:
        return "review"  # a needed question was not answered
    if any(abs(x - t) < th["margin"] * s for x, t, s in checks):
        return "review"
    if th["use_holistic"] and answers.get("verdict"):
        a = answers["verdict"]
        top = [k for k, p in a.items() if p == max(a.values())]
        if top != [v]:  # an exact tie is review: the crate's sorted maps cannot reproduce max()'s tie-break
            return "review"
    return _unit_guard(v, row, th)


def _unit_guard(v, row, th):
    comment = row.get("comment", "")
    if v in ("delete", "rename_instead") and unit_literal(comment, row.get("annotated_code", "")):
        return "rewrite_shorter" if comment.count("\n") + 1 > th["cap_lines"] else "keep"
    return v


def _decide(answers, row, th, kind, lt, gt):
    if gt(_p(answers, "g1_contradicts"), th["g1"]):
        return "fix_wrong_claim"
    if kind == "api_doc":
        c5 = _e(answers, "c5_contract")
        rep = answers["c9_replaceable"]
        q = (2 * _e(answers, "c2_info_gain") / 3 + 3 * c5 / 3 + 2 * _e(answers, "c8_durability") / 3
             + 0.5 * sum(_REPLACEABLE[k] * p for k, p in rep.items())) / 7.5
        if lt(q, th["api_delete"]) or lt(c5, th["c5_min"], 3):
            return "delete"
        return "rewrite_shorter" if lt(q, th["api_rewrite"]) else "keep"
    if lt(_e(answers, "c2_info_gain"), th["c2_min"], 3):
        return "delete"
    if gt(_p(answers, "c9_replaceable", "fully"), th["rename"]):
        return "rename_instead"
    hazard = _p(answers, "hazard")
    if lt(hazard, th["hazard"]) and lt(_e(answers, "c3_rationale"), th["c3_min"], 3):
        return "delete"
    if (hazard >= th["hazard"] and gt(_p(answers, "leak"), th["leak"])) or \
            row.get("comment", "").count("\n") + 1 > th["cap_lines"]:
        return "rewrite_shorter"
    return "keep"


GENERATIVE_JUDGES = {"gemma-asis", "gemma-state", "generative"}  # one-hot answers: NEXT 2.2 uses their verdict


def judge_verdict(res, row, th):
    """Verdict for one run file: generative judges (or runs with no answers) bypass the policy."""
    if row.get("comment_kind") in ("todo", "directive"):
        return "keep"  # structural in the crate for every judge: never reaches one
    if res.get("judge") in GENERATIVE_JUDGES or not res.get("answers"):
        v = res.get("verdict")
        return _unit_guard(v, row, th) if v in VERDICTS else "review"  # the crate's Verdict::parse: unknown is review
    return decide(res["answers"], row, th)
