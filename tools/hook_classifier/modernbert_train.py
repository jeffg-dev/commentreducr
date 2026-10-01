#!/usr/bin/env python3
"""Prespecified ModernBERT comparison using the frozen original data and splits."""

import argparse
import hashlib
import importlib.metadata
import json
import platform
import sys
from pathlib import Path

from train import evaluate, join_labels, read_jsonl, split_rows, train_encoder, write_results


def check_singleton_stability(result, split, extra, directory, threads):
    """Posthoc deployment check; retain all previously selected thresholds."""
    import numpy as np
    from infer import Classifier

    runtime = Classifier(directory, threads=threads)
    threshold = result["onnx_int8"]["threshold"]
    sensitivity_threshold = result["onnx_int8"]["sensitivity_99"]["threshold"]
    checks = {"note": "Posthoc deployment validation only; model/checkpoint/thresholds unchanged"}
    for name, rows in [("test", split["test"]), ("extra_eval", extra)]:
        batched = runtime.probabilities(rows)
        singles = np.asarray([runtime.probabilities([row])[0] for row in rows])
        changed = (batched >= threshold) != (singles >= threshold)
        checks[name] = {
            "rows": len(rows), "verdict_differences": int(changed.sum()),
            "max_probability_difference": float(np.max(np.abs(batched - singles))),
            "mean_probability_difference": float(np.mean(np.abs(batched - singles))),
            "singleton_metrics": evaluate(rows, singles, threshold),
            "singleton_metrics_at_frozen_99_threshold": evaluate(rows, singles, sensitivity_threshold),
            "changed_verdicts": [
                {"id": row["id"], "expected": row["annotation"]["verdict"],
                 "batch_probability": float(batch_probability), "single_probability": float(single_probability),
                 "batch_verdict": "FLAG" if batch_probability >= threshold else "PASS",
                 "single_verdict": "FLAG" if single_probability >= threshold else "PASS"}
                for row, batch_probability, single_probability, change in zip(rows, batched, singles, changed)
                if change
            ],
        }
    return checks


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    directory = Path(__file__).parent
    parser.add_argument("--corpus", type=Path, default=directory / "corpus.jsonl")
    parser.add_argument("--labels", type=Path, default=directory / "labels.jsonl")
    parser.add_argument("--extra-eval", type=Path, default=directory / "curated_holdout.jsonl")
    parser.add_argument("--extra-labels", type=Path, default=directory / "curated_holdout_labels.jsonl")
    parser.add_argument("--output", type=Path, default=directory / "artifacts" / "modernbert")
    parser.add_argument("--results", type=Path, default=directory / "results_modernbert.json")
    parser.add_argument("--encoder", default="answerdotai/ModernBERT-base")
    parser.add_argument("--revision", default="8949b909ec900327062f0ebf497f51aef5e6f0c8")
    parser.add_argument("--seed", type=int, default=42)
    parser.add_argument("--target-precision", type=float, default=0.95)
    parser.add_argument("--epochs", type=int, default=3)
    parser.add_argument("--batch-size", type=int, default=16)
    parser.add_argument("--inference-batch-size", type=int, default=1)
    parser.add_argument("--max-length", type=int, default=384)
    parser.add_argument("--learning-rate", type=float, default=3e-5)
    parser.add_argument("--threads", type=int, default=8)
    args = parser.parse_args()
    args.progress_steps = True
    args.keep_checkpoint = True
    rows = join_labels(read_jsonl(args.corpus), read_jsonl(args.labels))
    extra = join_labels(read_jsonl(args.extra_eval), read_jsonl(args.extra_labels))
    split, checks = split_rows(rows, args.seed)
    original = json.loads((directory / "results.json").read_text())
    hashes = {
        "corpus_sha256": hashlib.sha256(args.corpus.read_bytes()).hexdigest(),
        "labels_sha256": hashlib.sha256(args.labels.read_bytes()).hexdigest(),
    }
    if any(original[key] != value for key, value in hashes.items()):
        raise ValueError("Frozen corpus or labels changed")
    if any(original["extra_eval_checks"][key] != hashlib.sha256(path.read_bytes()).hexdigest()
           for key, path in [("corpus_sha256", args.extra_eval), ("labels_sha256", args.extra_labels)]):
        raise ValueError("Frozen curated holdout changed")
    split_ids = {name: [row["id"] for row in part] for name, part in split.items()}
    if split_ids != json.loads((directory / "splits.json").read_text()):
        raise ValueError("Frozen splits changed")
    args.output.mkdir(parents=True, exist_ok=True)
    (args.output / "split.json").write_text(json.dumps(split_ids, indent=2) + "\n")
    result = {
        **hashes, "input_fields": ["kind", "text", "context"], "split": checks,
        "python": sys.version, "platform": platform.platform(),
        "configuration": {key: str(value) if isinstance(value, Path) else value for key, value in vars(args).items()},
        "packages": {package: importlib.metadata.version(package) for package in
                     ["numpy", "torch", "transformers", "onnx", "onnxruntime", "tokenizers", "psutil"]},
        "extra_eval_checks": {
            "rows": len(extra),
            "corpus_sha256": hashlib.sha256(args.extra_eval.read_bytes()).hexdigest(),
            "labels_sha256": hashlib.sha256(args.extra_labels.read_bytes()).hexdigest(),
            "note": "Extra evaluation does not select thresholds, checkpoints, or hyperparameters",
        },
    }
    write_results(args.results, result)
    result["modernbert"] = train_encoder(split, args.output, args, extra)
    result["modernbert"]["onnx_int8"]["all_singleton_stability"] = check_singleton_stability(
        result["modernbert"], split, extra, args.output / "modernbert-int8", args.threads,
    )
    write_results(args.results, result)
    print(f"Results: {args.results}", flush=True)


if __name__ == "__main__":
    main()
