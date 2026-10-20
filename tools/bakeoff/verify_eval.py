#!/usr/bin/env python3
"""Measure a System One verifier on the verify set (candidate rewrites per gold row).

  run     POST each candidate's verify state (render_verify + src/schema/verify-v1.json) to --endpoint;
          one file per candidate under runs/verify-<name>/. Skips candidates already done.
  report  per-question accuracy/AUC, and accept rates at --th (default: the shipped thresholds).
  tune    pick thresholds on --split (default dev): fewest false accepts of drop/invent first,
          then most gold accepts.

A candidate "should accept" when its labels say preserves and not adds and not g1_candidate.
The gold set and the verify set are private: pass their paths.
"""
import argparse
import glob
import itertools
import json
import os
import sys
import time
from concurrent.futures import ThreadPoolExecutor

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path[:0] = [HERE, os.path.join(HERE, "..", "dataset", "v2")]
from judges import post_json  # noqa: E402
from render import render_verify  # noqa: E402
from split import split_of  # noqa: E402

QS = ("preserves", "adds", "g1_candidate")
_TH = json.load(open(os.path.join(HERE, "..", "..", "src", "schema", "thresholds-strict-v1.json")))
SHIPPED = (_TH["verify_preserves"], _TH["verify_adds"], _TH["verify_g1"])  # preserves >, adds <, g1_candidate <
SCHEMA = json.load(open(os.path.join(HERE, "..", "..", "src", "schema", "verify-v1.json")))


def load(verify_glob, gold_path, split):
    gold = {}
    for line in open(gold_path):
        if line.strip():
            r = json.loads(line)
            gold[r["id"]] = r
    rows = [json.loads(line) for f in sorted(glob.glob(verify_glob)) for line in open(f) if line.strip()]
    return [(v, gold[v["row_id"]]) for v in rows if split == "all" or split_of(v["row_id"]) == split]


def verify_state(v, g):
    return render_verify(g["comment"], v["candidate"], g["annotated_code"], g.get("enclosing_scope", ""))


def questions():
    return {q: {k: x for k, x in d.items() if k in ("type", "instructions", "criteria")} for q, d in SCHEMA["questions"].items()}


def out_dir(name):
    return os.path.join(HERE, "runs", "verify-" + name)


def fname(name, vid):
    return os.path.join(out_dir(name), vid.replace("#", "__") + ".json")


def run(a, items):
    os.makedirs(out_dir(a.name), exist_ok=True)
    todo = [(v, g) for v, g in items if not os.path.exists(fname(a.name, v["id"]))]

    def one(item):
        v, g = item
        t = time.time()
        try:
            resp = post_json(a.endpoint, {"model": a.model, "state": verify_state(v, g), "questions": questions()})
        except Exception as e:  # no file, so a rerun retries it
            print(f"{v['id']}: {e}", file=sys.stderr)
            return 0
        p = {q: x["noul"] for q, x in resp["answers"].items() if x.get("type") == "noul"}
        rec = {"id": v["id"], "p_true": p, "latency_ms": (time.time() - t) * 1000,
               "input_tokens": (resp.get("usage") or {}).get("input_tokens")}
        json.dump(rec, open(fname(a.name, v["id"]), "w"))
        return 1

    with ThreadPoolExecutor(a.workers) as ex:
        done = sum(ex.map(one, todo))
    print(f"{a.name}: {done}/{len(todo)} new, {len(items) - len(todo)} already done -> {out_dir(a.name)}")


def scored(name, items):
    out = []
    for v, _ in items:
        f = fname(name, v["id"])
        if os.path.exists(f):
            out.append((v, json.load(open(f))["p_true"]))
    return out


def good(v):
    lab = v["labels"]
    return lab["preserves"] and not lab["adds"] and not lab["g1_candidate"]


def accepts(p, th):
    tp, ta, tg = th
    return p.get("preserves", 0) > tp and p.get("adds", 1) < ta and p.get("g1_candidate", 1) < tg


def auc(pos, neg):
    if not pos or not neg:
        return float("nan")
    return sum((x > y) + 0.5 * (x == y) for x in pos for y in neg) / (len(pos) * len(neg))


def rates(sc, th):
    """(gold accepted, gold total, bad accepted, bad total, drop acc, drop n, invent acc, invent n)."""
    gold = [p for v, p in sc if v["variant"] == "gold" and good(v)]
    bad = {k: [p for v, p in sc if v["variant"] == k] for k in ("drop", "invent")}
    acc = lambda ps: sum(accepts(p, th) for p in ps)  # noqa: E731
    return (acc(gold), len(gold), acc(bad["drop"] + bad["invent"]), len(bad["drop"]) + len(bad["invent"]),
            acc(bad["drop"]), len(bad["drop"]), acc(bad["invent"]), len(bad["invent"]))


def fmt_rates(r):
    ga, gn, ba, bn, da, dn, ia, inn = r
    return (f"gold accepted {ga}/{gn} ({ga / max(gn, 1):.3f}); false accepts {ba}/{bn} ({ba / max(bn, 1):.3f}) "
            f"[drop {da}/{dn}, invent {ia}/{inn}]")


def report(name, items, ths):
    sc = scored(name, items)
    print(f"== {name}: {len(sc)}/{len(items)} candidates scored")
    print(f"{'question':14} {'n':>4} {'pos':>4} {'acc@0.5':>8} {'auc':>6} {'mean p(gold-variant)':>21} {'mean p(drop/invent)':>20}")
    for q in QS:
        lab = [(v["labels"][q], p[q]) for v, p in sc if q in p]
        acc = sum((x > 0.5) == y for y, x in lab) / max(len(lab), 1)
        a_ = auc([x for y, x in lab if y], [x for y, x in lab if not y])
        mg = [p[q] for v, p in sc if v["variant"] == "gold" and q in p]
        mb = [p[q] for v, p in sc if v["variant"] != "gold" and q in p]
        print(f"{q:14} {len(lab):4} {sum(y for y, _ in lab):4} {acc:8.3f} {a_:6.3f} "
              f"{sum(mg) / max(len(mg), 1):21.3f} {sum(mb) / max(len(mb), 1):20.3f}")
    for th in ths:
        print(f"th preserves>{th[0]} adds<{th[1]} g1<{th[2]}: {fmt_rates(rates(sc, th))}")


GRID_P = [0.0, 0.01, 0.02, 0.05, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 0.95, 0.98, 0.99]
GRID_LT = [0.01, 0.02, 0.05, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 0.95, 0.98, 0.99, 1.01]


def tune(name, items, max_fa):
    sc = scored(name, items)
    best = None
    for th in itertools.product(GRID_P, GRID_LT, GRID_LT):
        r = rates(sc, th)
        if r[2] > max_fa:
            continue
        # most gold accepts; ties: the strictest thresholds (highest preserves, lowest adds/g1)
        key = (r[0], th[0], -th[1], -th[2])
        if best is None or key > best[0]:
            best = (key, th, r)
    return best


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("cmd", choices=["run", "report", "tune"])
    ap.add_argument("--verify", required=True, help="glob of verify-set jsonl parts")
    ap.add_argument("--gold", required=True)
    ap.add_argument("--name", required=True, action="append", help="verifier name (repeatable for report/tune)")
    ap.add_argument("--split", default="test", choices=["train", "dev", "test", "all"])
    ap.add_argument("--endpoint")
    ap.add_argument("--model", default="clef-flash")
    ap.add_argument("--workers", type=int, default=1)
    ap.add_argument("--th", action="append", default=[], help="preserves,adds,g1 (repeatable)")
    ap.add_argument("--max-fa", type=int, action="append", default=[], help="tune: false accepts allowed (repeatable)")
    a = ap.parse_args()
    items = load(a.verify, a.gold, a.split)
    if a.cmd == "run":
        a.name = a.name[0]
        return run(a, items)
    ths = [SHIPPED] + [tuple(float(x) for x in t.split(",")) for t in a.th]
    for name in a.name:
        if a.cmd == "report":
            report(name, items, ths)
        else:
            for m in a.max_fa or [0]:
                best = tune(name, items, m)
                if best:
                    th = best[1]
                    print(f"{name} {a.split} max_fa {m}: th {th[0]},{th[1]},{th[2]} -> {fmt_rates(best[2])}")
                else:
                    print(f"{name} {a.split} max_fa {m}: no thresholds")


if __name__ == "__main__":
    main()
