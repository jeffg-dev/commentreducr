#!/usr/bin/env python3
"""Grid-search policy thresholds on the DEV split for one judge.

Cost = 10*hazards_lost + 3*errors_introduced + 1*wrong_delete + 1*churn + 0.3*wrong_keep
       + 0.5 per abstention ("review").
errors_introduced is 0 unless the run files carry rewrite_scores.
"""
import argparse, itertools, json, os, sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from policy import DEFAULT_THRESHOLDS, GENERATIVE_JUDGES, judge_verdict  # noqa: E402
from score import cost, flags, load_gold, load_runs  # noqa: E402

GRID = {
    "g1": [0.4, 0.5, 0.6, 0.7, 0.8],
    "c2_min": [0.5, 0.75, 1.0, 1.25],
    "rename": [0.5, 0.7, 0.9],
    "hazard": [0.3, 0.5, 0.7],
    "c3_min": [1.0, 1.5, 2.0],
    "api_delete": [0.2, 0.3, 0.4],
    "api_rewrite": [0.45, 0.55, 0.65],
    "margin": [0.0, 0.05, 0.15],
    "use_holistic": [True, False],
}


def total_cost(items, th):
    return sum(cost(flags(r, judge_verdict(res, r, th), res)) for r, res in items)


def tune(items):
    keys = list(GRID)
    best = (total_cost(items, DEFAULT_THRESHOLDS), DEFAULT_THRESHOLDS)
    for combo in itertools.product(*GRID.values()):
        th = {**DEFAULT_THRESHOLDS, **dict(zip(keys, combo))}
        c = total_cost(items, th)
        if c < best[0]:  # strict: ties keep the default / earlier (grid is ordered)
            best = (c, th)
    return best


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--gold", required=True)
    ap.add_argument("--judge", required=True)
    ap.add_argument("--runs", default=os.path.join(HERE, "runs"))
    ap.add_argument("--out", required=True)
    a = ap.parse_args()
    runs = load_runs(a.runs, a.judge)
    items = [(r, runs[r["id"]]) for r in load_gold(a.gold, "dev") if r["id"] in runs]
    if not items:
        sys.exit("no dev items with runs")
    if all(res.get("judge") in GENERATIVE_JUDGES or not res.get("answers") for _, res in items):
        print("generative judge (no answers): policy is bypassed, nothing to tune", file=sys.stderr)
    base = total_cost(items, DEFAULT_THRESHOLDS)
    c, th = tune(items)
    os.makedirs(os.path.dirname(os.path.abspath(a.out)), exist_ok=True)
    json.dump(th, open(a.out, "w"), indent=1)
    print(f"dev items {len(items)}: cost default {base:.1f} -> tuned {c:.1f}; wrote {a.out}")


if __name__ == "__main__":
    main()
