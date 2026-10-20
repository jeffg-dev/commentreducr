#!/usr/bin/env python3
"""Compare `--dump-items` context fields with a dataset-v2 gold file. Writes nothing.

Usage: parity.py GOLD.jsonl CHECKOUTS_DIR [--bin commentreducr]
CHECKOUTS_DIR/<repo basename> must already be checked out at each row's source.commit.
"""
import argparse, json, os, subprocess, sys
from collections import Counter

FIELDS = ("comment_kind", "placement", "annotated_code", "enclosing_scope", "symbol")


def norm(v):
    if isinstance(v, str):
        return "\n".join(l.strip() for l in v.splitlines()).rstrip("\n")
    return v


def head(repo, commit):
    r = subprocess.run(["git", "-C", repo, "rev-parse", "HEAD"], capture_output=True, text=True)
    return r.stdout.strip() == commit


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("gold")
    ap.add_argument("checkouts")
    ap.add_argument("--bin", default="commentreducr")
    a = ap.parse_args()
    rows = [json.loads(l) for l in open(a.gold) if l.strip()]
    found, by_field, cache = 0, Counter(), {}
    for row in rows:
        src = row["source"]
        repo = os.path.join(a.checkouts, os.path.basename(src["repo"]))
        path = os.path.join(repo, src["path"])
        if not os.path.isfile(path) or not head(repo, src["commit"]):
            print(f"{row['id']} not found at {src['commit'][:10]}")
            continue
        if path not in cache:
            r = subprocess.run([a.bin, "reduce", "--dump-items", path], capture_output=True, text=True)
            cache[path] = json.loads(r.stdout) if r.returncode == 0 else []
        item = next((i for i in cache[path] if i["start_line"] <= src["line"] <= i["end_line"]), None)
        if item is None:
            print(f"{row['id']} no item at line {src['line']}")
            continue
        found += 1
        for f in FIELDS:
            if norm(item.get(f)) != norm(row.get(f)):
                by_field[f] += 1
                print(f"{row['id']} {f}")
    print(f"{len(rows)} rows, {found} found, {sum(by_field.values())} mismatches by field {dict(by_field)}")


if __name__ == "__main__":
    sys.exit(main())
