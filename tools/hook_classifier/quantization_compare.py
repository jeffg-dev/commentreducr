#!/usr/bin/env python3
"""Exploratory ModernBERT export: int8 MatMul, float32 embedding Gather."""

import argparse
import hashlib
import json
import shutil
import time
from pathlib import Path

from infer import Classifier
from modernbert_train import check_singleton_stability
from train import benchmark, choose_threshold, evaluate, join_labels, read_jsonl, sensitivity_99, split_rows, write_results


def file_hash(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    directory = Path(__file__).parent
    parser.add_argument("--source", type=Path, default=directory / "artifacts" / "modernbert" / "modernbert-float.onnx")
    parser.add_argument("--tokenizer", type=Path, default=directory / "artifacts" / "modernbert" / "modernbert-int8")
    parser.add_argument("--output", type=Path, default=directory / "artifacts" / "modernbert" / "modernbert-mixed")
    parser.add_argument("--results", type=Path, default=directory / "results_modernbert_mixed.json")
    parser.add_argument("--threads", type=int, default=8)
    args = parser.parse_args()
    original = json.loads((directory / "results_modernbert.json").read_text())
    paths = {
        "corpus": directory / "corpus.jsonl", "labels": directory / "labels.jsonl",
        "extra_corpus": directory / "curated_holdout.jsonl", "extra_labels": directory / "curated_holdout_labels.jsonl",
    }
    hashes = {name: file_hash(path) for name, path in paths.items()}
    if hashes["corpus"] != original["corpus_sha256"] or hashes["labels"] != original["labels_sha256"]:
        raise ValueError("Frozen main corpus or labels changed")
    if hashes["extra_corpus"] != original["extra_eval_checks"]["corpus_sha256"] or hashes["extra_labels"] != original["extra_eval_checks"]["labels_sha256"]:
        raise ValueError("Frozen curated holdout changed")
    rows = join_labels(read_jsonl(paths["corpus"]), read_jsonl(paths["labels"]))
    extra = join_labels(read_jsonl(paths["extra_corpus"]), read_jsonl(paths["extra_labels"]))
    split, checks = split_rows(rows, 42)
    if {name: [row["id"] for row in part] for name, part in split.items()} != json.loads((directory / "splits.json").read_text()):
        raise ValueError("Frozen splits changed")
    args.output.mkdir(parents=True, exist_ok=True)
    for name in ["tokenizer.json", "tokenizer_config.json", "special_tokens_map.json"]:
        shutil.copyfile(args.tokenizer / name, args.output / name)
    config = json.loads((args.tokenizer / "classifier.json").read_text())
    config["inference_batch_size"] = 1
    config["quantized_ops"] = ["MatMul"]
    config["embedding_dtype"] = "float32"
    (args.output / "classifier.json").write_text(json.dumps(config, indent=2) + "\n")
    source_hash = file_hash(args.source)
    from onnxruntime.quantization import QuantType, quantize_dynamic

    tick = time.perf_counter()
    quantize_dynamic(str(args.source), str(args.output / "model.onnx"),
                     weight_type=QuantType.QInt8, per_channel=True, op_types_to_quantize=["MatMul"])
    export_seconds = time.perf_counter() - tick
    runtime = Classifier(args.output, args.threads)
    dev_probabilities = runtime.probabilities(split["dev"])
    threshold, selected = choose_threshold(split["dev"], dev_probabilities, 0.95)
    config["threshold"] = threshold
    (args.output / "classifier.json").write_text(json.dumps(config, indent=2) + "\n")
    print(json.dumps({"export_seconds": export_seconds, "threshold": threshold, "dev": selected["dev"]}), flush=True)
    # The operating points are now frozen; no test/curated result selects them.
    test_probabilities = runtime.probabilities(split["test"])
    extra_probabilities = runtime.probabilities(extra)
    sizes = {path.name: path.stat().st_size for path in args.output.iterdir() if path.is_file()}
    result = {
        "method": "Exploratory export of frozen ModernBERT: dynamic int8 MatMul, float32 embedding Gather",
        "exploratory": True, "source_float_onnx_sha256": source_hash,
        "source_revision": config["revision"], "corpus_sha256": hashes["corpus"], "labels_sha256": hashes["labels"],
        "extra_eval_checks": {"corpus_sha256": hashes["extra_corpus"], "labels_sha256": hashes["extra_labels"], "rows": len(extra)},
        "split": checks, "inference_batch_size": 1, "quantized_ops": ["MatMul"], "embedding_dtype": "float32",
        "export_seconds": export_seconds, "threshold": threshold, "threshold_selection": selected,
        "test": evaluate(split["test"], test_probabilities, threshold),
        "extra_eval": evaluate(extra, extra_probabilities, threshold),
        "sensitivity_99": sensitivity_99(split, dev_probabilities, test_probabilities, extra, extra_probabilities),
        "artifact_bytes": sizes, "bundle_bytes": sum(sizes.values()),
        "artifact_sha256": {path.name: file_hash(path) for path in args.output.iterdir() if path.is_file()},
        "benchmark": benchmark(args.output, split["test"], args.threads),
    }
    # Reuse the same deployment validation without modifying the primary model.
    wrapper = {"onnx_int8": result}
    result["all_singleton_stability"] = check_singleton_stability(wrapper, split, extra, args.output, args.threads)
    write_results(args.results, result)
    print(f"Results: {args.results}", flush=True)


if __name__ == "__main__":
    main()
