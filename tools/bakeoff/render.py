"""Render a dataset v2 row as the judge `state` string, and pick the questions that apply."""

import json
from pathlib import Path

DEFAULT_SCHEMA = Path(__file__).parent / ".." / ".." / "src" / "schema" / "strict-v1.json"

QUESTION_KEYS = ("type", "instructions", "criteria")
EVIDENCE_KEYS = ("authored_by_agent", "agent", "comment_age_days", "code_newer_than_comment",
                 "comment_new", "comment_modified", "scope_changed")


def _estimate(text):
    return len(text) // 4


def _scope_window(scope, annotated, half):
    """Keep `half` lines either side of the centre of `annotated` in `scope`, with ... markers."""
    lines = scope.splitlines()
    first = next((ln.strip() for ln in annotated.splitlines() if ln.strip()), "")
    start = next((i for i, ln in enumerate(lines) if first and first in ln), len(lines) // 2)
    centre = start + len(annotated.splitlines()) // 2
    lo, hi = max(0, centre - half), min(len(lines), centre + half + 1)
    return "\n".join((["..."] if lo > 0 else []) + lines[lo:hi] + (["..."] if hi < len(lines) else []))


def render_state(row, max_tokens=3000, count_tokens=None):
    count = count_tokens or _estimate
    symbol = row.get("symbol") or {}
    head = [
        f"language: {row.get('language')}  comment_kind: {row.get('comment_kind')}  placement: {row.get('placement')}",
        f"comment:\n{row.get('comment', '')}",
        f"annotated_code:\n{row.get('annotated_code', '')}",
        f"symbol: {symbol.get('name')}  public: {symbol.get('public')}  is_test: {symbol.get('is_test')}",
    ]
    change = row.get("change") or {}
    evidence = [f"{k}: {change[k]}" for k in EVIDENCE_KEYS if change.get(k) is not None]
    if evidence:
        head.append("evidence:\n" + "\n".join(evidence))
    if change.get("annotated_code_before") is not None:
        head.append(f"annotated_code_before:\n{change['annotated_code_before']}")

    return _fit(head, row.get("enclosing_scope", ""), row.get("annotated_code", ""), max_tokens, count)


def render_verify(original, candidate, annotated_code, enclosing_scope, max_tokens=3000, count_tokens=None):
    """State for the rewrite verification questions (src/schema/verify-v1.json)."""
    head = [f"original:\n{original}", f"candidate:\n{candidate}", f"annotated_code:\n{annotated_code}"]
    return _fit(head, enclosing_scope, annotated_code, max_tokens, count_tokens or _estimate)


def _fit(head, scope, annotated, max_tokens, count):
    """`head` then `enclosing_scope`, the scope trimmed around `annotated` to fit `max_tokens`."""
    def build(scope):
        return "\n\n".join(head + [f"enclosing_scope:\n{scope}"])

    text = build(scope)
    if count(text) <= max_tokens:
        return text
    # Binary search for the widest centred scope window that fits; empty scope if none does.
    lo, hi, best = 0, len(scope.splitlines()), build("...")
    while lo <= hi:
        half = (lo + hi) // 2
        candidate = build(_scope_window(scope, annotated, half))
        if count(candidate) <= max_tokens:
            best, lo = candidate, half + 1
        else:
            hi = half - 1
    return best


def questions_for(schema, row):
    """System One `questions` for this row: drop conditional questions that do not apply.
    A schema's top-level `instructions` prefix each question's: the request carries only the questions."""
    if schema is None:
        schema = json.loads(DEFAULT_SCHEMA.read_text())
    applies = schema.get("applies_to", {})
    pre = schema.get("instructions")

    def keep(k, v):
        return f"{pre} {v}" if k == "instructions" and isinstance(v, str) and isinstance(pre, str) else v

    return {
        qid: {k: keep(k, v) for k, v in q.items() if k in QUESTION_KEYS}
        for qid, q in schema["questions"].items()
        if qid not in applies or applies[qid] == row.get("comment_kind")
    }
