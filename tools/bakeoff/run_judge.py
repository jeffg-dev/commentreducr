#!/usr/bin/env python3
"""Run one judge over gold rows; one result file per id under runs/<name>/. Skips ids already done."""
import argparse
import importlib
import json
import os
import sys
import time
from concurrent.futures import ThreadPoolExecutor

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
sys.path.insert(0, os.path.join(HERE, "..", "dataset", "v2"))
from split import split_of  # noqa: E402

JUDGES = {"flash": "systemone", "clef27": "systemone", "gemma-asis": "gemma_v1",
          "gemma-state": "gemma_v1", "generative": "generative"}


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--judge", required=True, choices=JUDGES)
    ap.add_argument("--gold", required=True)
    ap.add_argument("--split", default="all", choices=["test", "dev", "all"])
    ap.add_argument("--model")
    ap.add_argument("--endpoint")
    ap.add_argument("--workers", type=int, default=4)
    ap.add_argument("--name")
    ap.add_argument("--schema", help="systemone: question schema JSON (default src/schema/strict-v1.json)")
    ap.add_argument("--append-question", help="systemone: add one unrelated yes/no question")
    ap.add_argument("--api-key")
    a = ap.parse_args(argv)

    mod = importlib.import_module("judges." + JUDGES[a.judge])
    o = {**vars(a), "judge": a.judge, "append_question": a.append_question}
    o["endpoint"] = a.endpoint or mod.DEFAULT_ENDPOINT
    o["model"] = a.model or getattr(mod, "DEFAULT_MODELS", {}).get(a.judge) or mod.DEFAULT_MODEL
    out_dir = os.path.join(HERE, "runs", a.name or a.judge)
    os.makedirs(out_dir, exist_ok=True)

    with open(a.gold) as f:
        rows = [json.loads(line) for line in f if line.strip()]
    if a.split != "all":
        rows = [r for r in rows if split_of(r["id"]) == a.split]
    todo = [r for r in rows if not os.path.exists(os.path.join(out_dir, r["id"] + ".json"))]

    def save(row, res, ms):
        rec = {"id": row["id"], "judge": a.judge, "answers": {}, "verdict": None, "rewrite": None,
               "latency_ms": ms, "input_tokens": None, "raw": None, **res}
        with open(os.path.join(out_dir, row["id"] + ".json"), "w") as f:
            json.dump(rec, f)

    def one(row):
        t = time.time()
        try:
            res = mod.run_one(row, o)
        except Exception as e:  # leave no file so a rerun retries it
            print(f"{row['id']}: {e}", file=sys.stderr)
            return 0
        save(row, res, (time.time() - t) * 1000)
        return 1

    t = time.time()
    if hasattr(mod, "run_batch"):
        results = mod.run_batch(todo, o)
        ms = (time.time() - t) * 1000 / max(len(todo), 1)
        for r in todo:
            if r["id"] in results:
                save(r, results[r["id"]], ms)
        done = len(results)
    else:
        with ThreadPoolExecutor(a.workers) as ex:
            done = sum(ex.map(one, todo))
    print(f"{a.judge}: {done}/{len(todo)} new, {len(rows) - len(todo)} already done -> {out_dir}")
    return 0 if done == len(todo) else 1


if __name__ == "__main__":
    sys.exit(main())
