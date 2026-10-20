#!/usr/bin/env python3
"""Score a judge's runs against the gold set (default: TEST split). Gold is private; pass its path."""
import argparse, json, os, statistics, sys
from collections import Counter, defaultdict

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path[:0] = [HERE, os.path.join(HERE, "..", "dataset", "v2")]
from policy import DEFAULT_THRESHOLDS, VERDICTS, judge_verdict  # noqa: E402
from split import split_of  # noqa: E402

REMOVE = {"delete", "rename_instead"}
REWRITE = {"rewrite_shorter", "fix_wrong_claim"}
REVIEW_COST = 0.5


def load_gold(path, split=None):
    with open(path) as fh:
        rows = [json.loads(l) for l in fh if l.strip()]
    return [r for r in rows if split in (None, "all") or split_of(r["id"]) == split]


def load_runs(runs_dir, judge):
    d = os.path.join(runs_dir, judge)
    out = {}
    for f in os.listdir(d):
        if f.endswith(".json"):
            with open(os.path.join(d, f)) as fh:
                r = json.load(fh)
            out[r["id"]] = r
    return out


def load_thresholds(path=None):
    th = dict(DEFAULT_THRESHOLDS)
    if path:
        th.update(json.load(open(path)))
    return th


def flags(gold, sys_v, res=None):
    """Per-item event flags for one (gold verdict, system verdict) pair."""
    g, lab = gold["labels"]["verdict"], gold["labels"]
    rs = (res or {}).get("rewrite_scores") or {}
    review = sys_v == "review"
    hz = not review and bool(lab.get("hazard")) and g in ("keep", "rewrite_shorter") and sys_v in REMOVE
    return {
        "review": review,
        "hazard_lost": hz,
        "wrong_delete": not review and not hz and sys_v in REMOVE and g not in REMOVE,
        "churn": sys_v in REWRITE and g == "keep",
        "wrong_keep": sys_v == "keep" and g != "keep",
        # only when the run carries rewrite scores (adds / g1_candidate probabilities)
        "error_introduced": sys_v in REWRITE and (rs.get("adds", 0) > 0.5 or rs.get("g1_candidate", 0) > 0.5),
    }


def cost(f):
    return (REVIEW_COST * f["review"] + 10 * f["hazard_lost"] + 3 * f["error_introduced"]
            + f["wrong_delete"] + f["churn"] + 0.3 * f["wrong_keep"])


def macro_f1(pairs):
    f1s = []
    for c in VERDICTS:
        tp = sum(g == c and s == c for g, s in pairs)
        fp = sum(g != c and s == c for g, s in pairs)
        fn = sum(g == c and s != c for g, s in pairs)
        if tp + fp + fn:
            f1s.append(2 * tp / (2 * tp + fp + fn))
    return sum(f1s) / len(f1s) if f1s else 0.0


def pct(xs, q):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(q * len(xs)))] if xs else None


def opt_key(v):
    return str(v).lower() if isinstance(v, bool) else str(v)


def question_stats(items):
    acc, brier = defaultdict(list), defaultdict(list)
    for row, res, _ in items:
        for q, dist in (res.get("answers") or {}).items():
            gold = row["labels"].get(q)
            if gold is None or not dist:
                continue
            g = opt_key(gold)
            acc[q].append(max(dist, key=dist.get) == g)
            brier[q].append(sum((p - (k == g)) ** 2 for k, p in dist.items()) + (0 if g in dist else 1))
    return {q: {"n": len(a), "acc": sum(a) / len(a), "brier": sum(brier[q]) / len(a)} for q, a in acc.items()}


def summarize(items):
    n = len(items)
    pairs = [(r["labels"]["verdict"], v) for r, _, v in items]
    fl = [flags(r, v, res) for r, res, v in items]
    cnt = lambda k: sum(f[k] for f in fl)
    rest = [(g, s) for g, s in pairs if s != "review"]
    lat = [res["latency_ms"] for _, res, _ in items if res.get("latency_ms") is not None]
    tok = [res["input_tokens"] for _, res, _ in items if res.get("input_tokens") is not None]
    hz_pool = sum(bool(r["labels"].get("hazard")) and r["labels"]["verdict"] in ("keep", "rewrite_shorter")
                  for r, _, _ in items)
    return {
        "n": n,
        "macro_f1": macro_f1(pairs),
        "confusion": {g: dict(Counter(s for gg, s in pairs if gg == g)) for g in VERDICTS},
        "hazards_lost": cnt("hazard_lost"),
        "hazard_pool": hz_pool,
        "hazards_lost_rate": cnt("hazard_lost") / hz_pool if hz_pool else None,
        "wrong_delete": cnt("wrong_delete"),
        "churn": cnt("churn"),
        "review_rate": cnt("review") / n if n else None,
        "rest_accuracy": sum(g == s for g, s in rest) / len(rest) if rest else None,
        "cost": sum(cost(f) for f in fl),
        "questions": question_stats(items),
        "latency_median_ms": statistics.median(lat) if lat else None,
        "latency_p95_ms": pct(lat, 0.95),
        "mean_input_tokens": sum(tok) / len(tok) if tok else None,
    }


def score(gold, runs, th):
    items = [(r, runs[r["id"]], judge_verdict(runs[r["id"]], r, th)) for r in gold if r["id"] in runs]
    out = {"missing": len(gold) - len(items), "overall": summarize(items)}
    for key in ("language", "comment_kind"):
        out[key] = {v: summarize([i for i in items if i[0].get(key) == v])
                    for v in sorted({i[0].get(key) for i in items})}
    return out


def fmt(x, nd=3):
    return "-" if x is None else f"{x:.{nd}f}" if isinstance(x, float) else str(x)


def print_report(judge, rep):
    print(f"== {judge} (missing runs: {rep['missing']})")
    cols = ["n", "macro_f1", "hazards_lost", "hazards_lost_rate", "wrong_delete", "churn",
            "review_rate", "rest_accuracy", "cost"]
    print(f"{'group':<22}" + "".join(f"{c:>18}" for c in cols))
    groups = [("overall", rep["overall"])]
    for key in ("language", "comment_kind"):
        groups += [(f"{key[:4]}:{k}", v) for k, v in rep[key].items()]
    for name, s in groups:
        print(f"{name:<22}" + "".join(f"{fmt(s[c]):>18}" for c in cols))
    o = rep["overall"]
    print("\nconfusion (rows gold, cols system):")
    cl = VERDICTS + ["review"]
    print(f"{'':<17}" + "".join(f"{c[:11]:>12}" for c in cl))
    for g in VERDICTS:
        print(f"{g:<17}" + "".join(f"{o['confusion'][g].get(c, 0):>12}" for c in cl))
    if o["questions"]:
        print("\nper-question:        n    acc   brier")
        for q, s in o["questions"].items():
            print(f"{q:<18}{s['n']:>4}{s['acc']:>7.3f}{s['brier']:>8.3f}")
    print(f"\nlatency median {fmt(o['latency_median_ms'], 1)} ms, p95 {fmt(o['latency_p95_ms'], 1)} ms, "
          f"mean input tokens {fmt(o['mean_input_tokens'], 0)}\n")


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--gold", required=True)
    ap.add_argument("--judge", action="append", required=True, help="repeatable")
    ap.add_argument("--runs", default=os.path.join(HERE, "runs"))
    ap.add_argument("--split", default="test", choices=["train", "dev", "test", "all"])
    ap.add_argument("--thresholds", help="JSON file of threshold overrides (e.g. from tune.py)")
    ap.add_argument("--json", help="write full report here")
    a = ap.parse_args()
    gold, th = load_gold(a.gold, a.split), load_thresholds(a.thresholds)
    reports = {j: score(gold, load_runs(a.runs, j), th) for j in a.judge}
    for j, rep in reports.items():
        print_report(j, rep)
    if a.json:
        json.dump(reports, open(a.json, "w"), indent=1)


if __name__ == "__main__":
    main()
