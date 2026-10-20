#!/usr/bin/env python3
"""Validate a dataset-v2 gold file and report counts. Usage: check.py GOLD. Exit 1 on errors."""
import json, os, sys
from collections import Counter

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from split import split_of  # noqa: E402

ENUMS = {
    "language": {"python", "typescript", "tsx", "javascript", "rust", "yaml"},
    "comment_kind": {"inline", "api_doc", "todo", "directive"},
    "placement": {"before_statement", "trailing", "function_head", "class_head", "module_head"},
}
LABEL_ENUMS = {
    "kind": {"rationale", "summary", "contract", "invariant", "warning", "alternatives_rejected", "todo",
             "directive", "restating", "commented_out_code", "journal_or_history", "banner",
             "cross_reference_leak", "change_narration"},
    "c4_level": {"same_level_narration", "higher_summary", "lower_precision", "mental_model"},
    "c9_replaceable": {"fully", "partly", "no"},
    "verdict": {"keep", "rewrite_shorter", "delete", "rename_instead", "fix_wrong_claim"},
}
BOOLS = ["g1_contradicts", "g1_unverifiable", "hazard", "leak"]
SCORES = ["c2_info_gain", "c3_rationale", "c8_durability"]
norm = lambda s: " ".join(s.split())


def validate(row):
    e, lab = [], row.get("labels")
    for k in ("id", "language", "comment_kind", "placement", "comment", "annotated_code",
              "enclosing_scope", "labels", "claims"):
        if k not in row:
            e.append(f"missing {k}")
    if e or not isinstance(lab, dict):
        return e or ["labels not an object"]
    for k, vals in ENUMS.items():
        if row[k] not in vals:
            e.append(f"{k}={row[k]!r} not in enum")
    for k, vals in LABEL_ENUMS.items():
        if lab.get(k) not in vals:
            e.append(f"labels.{k}={lab.get(k)!r} not in enum")
    e += [f"labels.{k} must be bool" for k in BOOLS if not isinstance(lab.get(k), bool)]
    e += [f"labels.{k} must be int 0-3" for k in SCORES if lab.get(k) not in (0, 1, 2, 3) or isinstance(lab.get(k), bool)]
    for k, kind in (("c5_contract", "api_doc"), ("c10_actionable", "todo")):
        v = lab.get(k)
        if (row["comment_kind"] == kind) != (v is not None):
            e.append(f"labels.{k} must be {'set' if row['comment_kind'] == kind else 'null'} for {row['comment_kind']}")
        elif v is not None and v not in (0, 1, 2, 3):
            e.append(f"labels.{k} must be 0-3")
    needs_rewrite = lab.get("verdict") in ("rewrite_shorter", "fix_wrong_claim")
    if needs_rewrite != bool(lab.get("rewrite")):
        e.append(("rewrite required for " if needs_rewrite else "rewrite must be null for ") + str(lab.get("verdict")))
    if (lab.get("c9_replaceable") == "fully" or lab.get("verdict") == "rename_instead") and not lab.get("rename_suggestion"):
        e.append("rename_suggestion required when c9_replaceable is fully")
    st = [c.get("status") for c in row["claims"]]
    if any(s not in ("confirmed", "contradicted", "unverifiable") for s in st):
        e.append("claim status not in enum")
    if lab.get("g1_contradicts") != ("contradicted" in st):
        e.append("g1_contradicts disagrees with claims")
    if lab.get("g1_unverifiable") != ("unverifiable" in st and "contradicted" not in st):
        e.append("g1_unverifiable disagrees with claims")
    if lab.get("g1_contradicts") and lab.get("verdict") not in ("fix_wrong_claim", "delete"):
        e.append("g1_contradicts but verdict is not fix_wrong_claim or delete")
    if norm(row["annotated_code"]) not in norm(row["enclosing_scope"]):
        e.append("enclosing_scope does not contain annotated_code")
    return e


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    rows, errors = [], 0
    for n, line in enumerate(open(sys.argv[1]), 1):
        if not line.strip():
            continue
        try:
            rows.append(json.loads(line))
        except ValueError as x:
            print(f"line {n}: bad json: {x}")
            errors += 1
    ids = Counter(r.get("id") for r in rows)
    for i, c in ids.items():
        if c > 1:
            print(f"duplicate id {i} x{c}")
            errors += 1
    for r in rows:
        for m in validate(r):
            print(f"{r.get('id')}: {m}")
            errors += 1
    ok = [r for r in rows if not validate(r)]
    print(f"\nrows {len(rows)}, valid {len(ok)}, errors {errors}")
    for name, f in (("language", lambda r: r["language"]), ("comment_kind", lambda r: r["comment_kind"]),
                    ("verdict", lambda r: r["labels"]["verdict"]), ("split", lambda r: split_of(r["id"]))):
        print(f"{name}: " + ", ".join(f"{k}={v}" for k, v in sorted(Counter(map(f, ok)).items())))
    print(f"hazards: {sum(r['labels']['hazard'] for r in ok)}")
    print(f"contradictions (g1_contradicts): {sum(r['labels']['g1_contradicts'] for r in ok)}")
    sys.exit(1 if errors else 0)


if __name__ == "__main__":
    main()
