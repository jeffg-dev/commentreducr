#!/usr/bin/env python3
"""CPU-only, scenario-disjoint TF-IDF and fine-tuned MiniLM experiments."""

import argparse
import collections
import hashlib
import importlib.metadata
import json
import math
import os
import platform
import random
import subprocess
import sys
import time
from pathlib import Path

from infer import model_input, model_pair


def read_jsonl(path):
    return [json.loads(line) for line in Path(path).read_text().splitlines() if line]


def cpu_name():
    cpuinfo = Path("/proc/cpuinfo")
    if cpuinfo.exists():
        for line in cpuinfo.read_text().splitlines():
            if line.startswith("model name"):
                return line.partition(":")[2].strip()
    return platform.processor() or platform.machine()


def join_labels(corpus, annotations):
    labels = {row["id"]: row for row in annotations}
    if len(labels) != len(annotations):
        raise ValueError("Duplicate annotation IDs")
    if len({row["id"] for row in corpus}) != len(corpus):
        raise ValueError("Duplicate corpus IDs")
    if set(labels) != {row["id"] for row in corpus}:
        raise ValueError("Corpus and annotation IDs must match exactly")
    result = []
    for row in corpus:
        annotation = labels[row["id"]]
        if annotation["verdict"] not in {"PASS", "FLAG"}:
            raise ValueError(f"Invalid verdict for {row['id']}")
        result.append({**row, "annotation": annotation, "label": int(annotation["verdict"] == "FLAG")})
    return result


def write_results(path, value):
    """Keep per-example records compact while preserving readable report structure."""
    def render(item, depth=0):
        indent = "  " * depth
        child = "  " * (depth + 1)
        if isinstance(item, dict):
            if not item or "id" in item or ("confusion" in item and "failures" not in item):
                return json.dumps(item, ensure_ascii=False)
            return "{\n" + ",\n".join(
                child + json.dumps(key) + ": " + render(data, depth + 1)
                for key, data in item.items()
            ) + "\n" + indent + "}"
        if isinstance(item, list):
            if not item or all(not isinstance(data, (dict, list)) for data in item):
                return json.dumps(item, ensure_ascii=False)
            return "[\n" + ",\n".join(child + render(data, depth + 1) for data in item) + "\n" + indent + "]"
        return json.dumps(item, ensure_ascii=False)

    Path(path).write_text(render(value) + "\n")


def normalized_text(row):
    return " ".join(row["text"].lower().split())


def split_rows(rows, seed):
    # Union scenarios sharing identical prose before splitting. This preserves both
    # scenario isolation and exact-prose isolation, including whitespace variants.
    parents = {row["scenario_id"]: row["scenario_id"] for row in rows}

    def root(group):
        while parents[group] != group:
            parents[group] = parents[parents[group]]
            group = parents[group]
        return group

    seen = {}
    for row in rows:
        text = normalized_text(row)
        if text in seen:
            parents[root(row["scenario_id"])] = root(seen[text])
        else:
            seen[text] = row["scenario_id"]
    groups = collections.defaultdict(list)
    for row in rows:
        groups[root(row["scenario_id"])].append(row)
    components = sorted(groups)
    if len(components) < 3:
        raise ValueError("Need at least three independent scenario/text components")
    random.Random(seed).shuffle(components)
    n_train = max(1, min(len(components) - 2, round(len(components) * 0.7)))
    n_dev = max(1, min(len(components) - n_train - 1, round(len(components) * 0.15)))
    partitions = [components[:n_train], components[n_train:n_train + n_dev], components[n_train + n_dev:]]
    split = {
        name: [row for group in partition for row in groups[group]]
        for name, partition in zip(["train", "dev", "test"], partitions)
    }
    for left, right in [("train", "dev"), ("train", "test"), ("dev", "test")]:
        for field in ["scenario_id", "normalized_text"]:
            values = lambda name: {
                normalized_text(row) if field == "normalized_text" else row[field]
                for row in split[name]
            }
            if values(left) & values(right):
                raise AssertionError(f"{field} leakage between {left} and {right}")
    return split, {
        "seed": seed,
        "independent_components": len(components),
        "scenarios": len(parents),
        "duplicate_prose_rows": len(rows) - len(seen),
        "cross_fold_scenario_overlap": 0,
        "cross_fold_normalized_prose_overlap": 0,
        "largest_component_rows": max(map(len, groups.values())),
        "folds": {
            name: {"rows": len(part), "scenarios": len({r["scenario_id"] for r in part}),
                   "labels": dict(collections.Counter(r["annotation"]["verdict"] for r in part)),
                   "kinds": dict(collections.Counter(r["kind"] for r in part))}
            for name, part in split.items()
        },
    }


def metrics(labels, predictions):
    predictions = [bool(prediction) for prediction in predictions]
    tn = sum(not actual and not predicted for actual, predicted in zip(labels, predictions))
    fp = sum(not actual and predicted for actual, predicted in zip(labels, predictions))
    fn = sum(actual and not predicted for actual, predicted in zip(labels, predictions))
    tp = sum(actual and predicted for actual, predicted in zip(labels, predictions))
    count = len(labels)
    precision = tp / (tp + fp) if tp + fp else None
    recall = tp / (tp + fn) if tp + fn else None
    # Wilson interval makes tiny, perfect-precision samples visibly uncertain.
    interval = None
    if precision is not None:
        n = tp + fp
        z = 1.96
        denominator = 1 + z * z / n
        center = (precision + z * z / (2 * n)) / denominator
        margin = z * math.sqrt(precision * (1 - precision) / n + z * z / (4 * n * n)) / denominator
        interval = [max(0.0, center - margin), min(1.0, center + margin)]
    return {
        "rows": count, "accuracy": (tn + tp) / count if count else None,
        "flag_precision": precision, "flag_precision_wilson_95": interval,
        "flag_recall": recall, "false_flag_rate": fp / (tn + fp) if tn + fp else None,
        "confusion": {"tn": tn, "fp": fp, "fn": fn, "tp": tp},
    }


def choose_threshold(rows, probabilities, target):
    values = sorted(set(map(float, probabilities)))
    # Midpoints avoid choosing a cutoff equal to one particular dev sample's
    # floating-point score when an entire interval yields the same decisions.
    candidates = [0.0, 1.000001] + [(left + right) / 2 for left, right in zip(values, values[1:])]
    qualifying = []
    for threshold in candidates:
        score = metrics([row["label"] for row in rows], [p >= threshold for p in probabilities])
        if score["flag_precision"] is not None and score["flag_precision"] >= target:
            qualifying.append((score["flag_recall"] or 0, score["flag_precision"], threshold, score))
    if not qualifying:
        threshold = 1.000001
        score = metrics([r["label"] for r in rows], [False] * len(rows))
    else:
        _, _, threshold, score = max(qualifying, key=lambda item: (item[0], item[1], -abs(item[2] - 0.5)))
    return float(threshold), {
        "target_precision": target, "achieved_nonzero_dev_flags": bool(qualifying),
        "selection": "Maximize dev recall at target precision; use score-gap midpoint, tie nearest 0.5",
        "dev": score,
    }


def evaluate(rows, probabilities, threshold):
    predictions = [bool(p >= threshold) for p in probabilities]
    result = metrics([r["label"] for r in rows], predictions)
    result["per_kind"] = {}
    for kind in sorted({r["kind"] for r in rows}):
        indices = [i for i, row in enumerate(rows) if row["kind"] == kind]
        result["per_kind"][kind] = metrics([rows[i]["label"] for i in indices], [predictions[i] for i in indices])
    result["per_rule_recall"] = {}
    for rule in ["narration", "noise", "leaky_reference"]:
        indices = [i for i, row in enumerate(rows) if rule in row["annotation"].get("rules", [])]
        result["per_rule_recall"][rule] = {
            "rows": len(indices),
            "recall": sum(predictions[i] for i in indices) / len(indices) if indices else None,
        }
    result["failures"] = [
        {"id": row["id"], "kind": row["kind"], "text": row["text"],
         "expected": row["annotation"]["verdict"], "predicted": "FLAG" if prediction else "PASS",
         "probability_flag": float(probability), "rules": row["annotation"].get("rules", [])}
        for row, probability, prediction in zip(rows, probabilities, predictions)
        if row["label"] != prediction
    ]
    return result


def sensitivity_99(split, dev_probabilities, test_probabilities, extra, extra_probabilities):
    threshold, selected = choose_threshold(split["dev"], dev_probabilities, 0.99)
    result = {"threshold": threshold, "threshold_selection": selected,
              "test": evaluate(split["test"], test_probabilities, threshold)}
    if extra:
        result["extra_eval"] = evaluate(extra, extra_probabilities, threshold)
    return result


def train_baseline(split, output, target, threads, extra):
    import joblib
    from sklearn.feature_extraction.text import TfidfVectorizer
    from sklearn.linear_model import LogisticRegression
    from sklearn.pipeline import FeatureUnion, Pipeline

    pipeline = Pipeline([
        ("features", FeatureUnion([
            ("word", TfidfVectorizer(ngram_range=(1, 2), max_features=30000, sublinear_tf=True)),
            ("char", TfidfVectorizer(analyzer="char_wb", ngram_range=(3, 5), max_features=30000, sublinear_tf=True)),
        ])),
        ("classifier", LogisticRegression(C=4, class_weight="balanced", max_iter=2000, random_state=42)),
    ])
    started = time.perf_counter()
    pipeline.fit([model_input(r) for r in split["train"]], [r["label"] for r in split["train"]])
    trained = time.perf_counter() - started
    probabilities = {name: pipeline.predict_proba([model_input(r) for r in part])[:, 1] for name, part in split.items()}
    threshold, selected = choose_threshold(split["dev"], probabilities["dev"], target)
    path = output / "tfidf.joblib"
    joblib.dump({"pipeline": pipeline, "threshold": threshold}, path, compress=3)
    result = {
        "method": "word/character TF-IDF + logistic regression", "training_seconds": trained,
        "threshold": threshold, "threshold_selection": selected, "serialized_bytes": path.stat().st_size,
        "test_at_0_5": evaluate(split["test"], probabilities["test"], 0.5),
        "test": evaluate(split["test"], probabilities["test"], threshold),
        "benchmark": benchmark(path, split["test"], threads),
    }
    extra_probabilities = pipeline.predict_proba([model_input(row) for row in extra])[:, 1] if extra else []
    if extra:
        result["extra_eval"] = evaluate(extra, extra_probabilities, threshold)
    result["sensitivity_99"] = sensitivity_99(split, probabilities["dev"], probabilities["test"], extra, extra_probabilities)
    return result


def benchmark(directory, rows, threads):
    sample = (directory if directory.is_dir() else directory.parent) / "benchmark.jsonl"
    sample.write_text("".join(json.dumps(row) + "\n" for row in rows[:20]))
    runs = []
    for _ in range(3):
        tick = time.perf_counter()
        process = subprocess.Popen(
            [sys.executable, str(Path(__file__).with_name("infer.py")), "--model", str(directory),
             "--input", str(sample), "--benchmark", "--threads", str(threads)],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            env={**os.environ, "HF_HUB_OFFLINE": "1", "TRANSFORMERS_OFFLINE": "1"},
        )
        first_line = process.stdout.readline()
        first_ms = (time.perf_counter() - tick) * 1000
        stdout, stderr = process.communicate()
        if process.returncode:
            raise RuntimeError(f"Inference benchmark failed: {stderr}")
        json.loads(first_line)
        run = json.loads(stdout)
        run["fresh_process_first_result_ms"] = first_ms
        run["fresh_process_total_ms"] = (time.perf_counter() - tick) * 1000
        runs.append(run)
    sample.unlink()
    return {"cpu": cpu_name(), "threads": threads, "runs": runs,
            "note": "New processes; OS file cache may be warm. Single-example timings include tokenization."}


def train_encoder(split, output, args, extra):
    import numpy as np
    import torch
    from onnxruntime.quantization import QuantType, quantize_dynamic
    from transformers import AutoConfig, AutoModelForSequenceClassification, AutoTokenizer

    torch.set_num_threads(args.threads)
    torch.manual_seed(args.seed)
    np.random.seed(args.seed)
    tokenizer = AutoTokenizer.from_pretrained(args.encoder, revision=args.revision, local_files_only=True)
    model_config = AutoConfig.from_pretrained(args.encoder, revision=args.revision, local_files_only=True)
    modernbert = model_config.model_type == "modernbert"
    model_options = {"reference_compile": False} if modernbert else {}
    model = AutoModelForSequenceClassification.from_pretrained(
        args.encoder, num_labels=2, id2label={0: "PASS", 1: "FLAG"},
        label2id={"PASS": 0, "FLAG": 1}, attn_implementation="eager", local_files_only=True, revision=args.revision,
        **model_options,
    )
    family = model_config.model_type if modernbert else "minilm"
    watched_weight = (model.model.layers[0].attn.Wqkv.weight if modernbert
                      else model.bert.encoder.layer[0].attention.self.query.weight)
    all_folds = {**split, **({"extra": extra} if extra else {})}
    lengths = {}
    tokenized = {}
    special_tokens = tokenizer.num_special_tokens_to_add(pair=True)
    for name, part in all_folds.items():
        pairs = [model_pair(row) for row in part]
        target_lengths = [len(tokenizer.encode(primary, add_special_tokens=False)) for primary, _ in pairs]
        context_lengths = [len(tokenizer.encode(context, add_special_tokens=False)) for _, context in pairs]
        if max(target_lengths) + special_tokens >= args.max_length:
            raise ValueError(f"{name}: target needs {max(target_lengths) + special_tokens} tokens; increase --max-length")
        tokenized[name] = tokenizer(
            [primary for primary, _ in pairs], [context for _, context in pairs],
            truncation="only_second", max_length=args.max_length,
        )
        lengths[name] = {
            "target_max": max(target_lengths), "target_median": float(np.median(target_lengths)),
            "target_p95": float(np.percentile(target_lengths, 95)),
            "context_max": max(context_lengths), "context_median": float(np.median(context_lengths)),
            "context_p95": float(np.percentile(context_lengths, 95)),
            "context_truncated_rows": sum(t + c + special_tokens > args.max_length for t, c in zip(target_lengths, context_lengths)),
            "target_truncated_rows": 0,
        }

    def batch(name, indices):
        return tokenizer.pad(
            [{key: values[i] for key, values in tokenized[name].items()} for i in indices],
            return_tensors="pt",
        )

    def predict(name):
        model.eval()
        probabilities = []
        with torch.no_grad():
            for start in range(0, len(all_folds[name]), args.batch_size):
                inputs = batch(name, range(start, min(start + args.batch_size, len(all_folds[name]))))
                probabilities.extend(torch.softmax(model(**inputs).logits, dim=-1)[:, 1].tolist())
        return np.asarray(probabilities)

    classifier_params = list(model.classifier.parameters())
    classifier_ids = {id(parameter) for parameter in classifier_params}
    optimizer = torch.optim.AdamW([
        {"params": [p for p in model.parameters() if id(p) not in classifier_ids], "lr": args.learning_rate},
        {"params": classifier_params, "lr": 0.001},
    ], weight_decay=0.01)
    counts = collections.Counter(row["label"] for row in split["train"])
    weights = torch.tensor([len(split["train"]) / (2 * counts[label]) for label in [0, 1]])
    loss_function = torch.nn.CrossEntropyLoss(weight=weights)
    history = []
    best_score = (-1, -1)
    best_path = output / f"best-{family}.pt"
    started = time.perf_counter()
    encoder_before = watched_weight.detach().clone()
    print(json.dumps({"model": args.encoder, "parameters": sum(p.numel() for p in model.parameters()),
                      "input_token_lengths": lengths}), flush=True)
    for epoch in range(args.epochs):
        model.train()
        order = list(range(len(split["train"])))
        random.Random(args.seed + epoch).shuffle(order)
        losses = []
        epoch_started = time.perf_counter()
        last_update = epoch_started
        for start in range(0, len(order), args.batch_size):
            indices = order[start:start + args.batch_size]
            inputs = batch("train", indices)
            labels = torch.tensor([split["train"][i]["label"] for i in indices])
            optimizer.zero_grad()
            loss = loss_function(model(**inputs).logits, labels)
            loss.backward()
            torch.nn.utils.clip_grad_norm_(model.parameters(), 1.0)
            optimizer.step()
            losses.append(loss.item())
            if (getattr(args, "progress_steps", False) and
                (len(losses) == 5 or time.perf_counter() - last_update >= 35)):
                elapsed = time.perf_counter() - epoch_started
                print(json.dumps({"epoch_progress": epoch + 1, "batches": len(losses),
                                  "batches_per_epoch": math.ceil(len(order) / args.batch_size),
                                  "seconds_per_batch": elapsed / len(losses),
                                  "projected_training_seconds": elapsed / len(losses) * math.ceil(len(order) / args.batch_size) * args.epochs,
                                  "elapsed_seconds": time.perf_counter() - started}), flush=True)
                last_update = time.perf_counter()
        probabilities = predict("dev")
        threshold, chosen = choose_threshold(split["dev"], probabilities, args.target_precision)
        score = chosen["dev"]
        candidate = (score["flag_recall"] or 0, score["flag_precision"] or 0)
        if candidate > best_score:
            best_score = candidate
            torch.save(model.state_dict(), best_path)
            best_epoch = epoch + 1
        record = {"epoch": epoch + 1, "mean_loss": float(np.mean(losses)),
                  "threshold": threshold, "dev": score,
                  "elapsed_seconds": time.perf_counter() - started}
        history.append(record)
        print(json.dumps(record), flush=True)
    training_seconds = time.perf_counter() - started
    model.load_state_dict(torch.load(best_path, weights_only=True))
    if not getattr(args, "keep_checkpoint", False):
        best_path.unlink()
    model.eval()
    encoder_change = float((watched_weight.detach() - encoder_before).abs().max())
    if not encoder_change:
        raise AssertionError("Encoder weights did not change; this was not encoder fine-tuning")
    dev_probabilities = predict("dev")
    threshold, selected = choose_threshold(split["dev"], dev_probabilities, args.target_precision)
    test_probabilities = predict("test")
    extra_probabilities = predict("extra") if extra else []
    float_result = {
        "method": f"Full {args.encoder} encoder fine-tuning with supervised binary cross-entropy",
        "parameters": sum(parameter.numel() for parameter in model.parameters()),
        "training_seconds": training_seconds, "epochs": history, "selected_epoch": best_epoch,
        "max_encoder_weight_change": encoder_change, "max_length": args.max_length,
        "input_token_lengths": lengths, "threshold": threshold, "threshold_selection": selected,
        "test_at_0_5": evaluate(split["test"], test_probabilities, 0.5),
        "test": evaluate(split["test"], test_probabilities, threshold),
        "sensitivity_99": sensitivity_99(split, dev_probabilities, test_probabilities, extra, extra_probabilities),
    }
    if extra:
        float_result["extra_eval"] = evaluate(extra, extra_probabilities, threshold)
    (output / "float_results.json").write_text(json.dumps(float_result, indent=2) + "\n")
    exported = output / f"{family}-int8"
    exported.mkdir(exist_ok=True)
    tokenizer.save_pretrained(exported)
    float_path = output / f"{family}-float.onnx"
    example = batch("test", [0])

    class Export(torch.nn.Module):
        def __init__(self, classifier):
            super().__init__()
            self.classifier = classifier

        def forward(self, input_ids, attention_mask, token_type_ids=None):
            inputs = {"input_ids": input_ids, "attention_mask": attention_mask}
            if token_type_ids is not None:
                inputs["token_type_ids"] = token_type_ids
            return self.classifier(**inputs).logits

    fields = ["input_ids", "attention_mask"] + ([] if modernbert else ["token_type_ids"])
    torch.onnx.export(
        Export(model), tuple(example[name] for name in fields), str(float_path),
        input_names=fields, output_names=["logits"], opset_version=17, dynamo=False,
        dynamic_axes={**{name: {0: "batch", 1: "sequence"} for name in fields}, "logits": {0: "batch"}},
    )
    quantize_dynamic(str(float_path), str(exported / "model.onnx"),
                     weight_type=QuantType.QInt8, per_channel=True,
                     op_types_to_quantize=["MatMul", "Gather"])
    config = {"threshold": threshold, "max_length": args.max_length,
              "inference_batch_size": args.inference_batch_size,
              "labels": ["PASS", "FLAG"], "encoder": args.encoder, "revision": args.revision,
              "pad_token_id": tokenizer.pad_token_id, "pad_token": tokenizer.pad_token,
              "input": "BERT pair: kind + complete target text, Python context; only context truncated"}
    (exported / "classifier.json").write_text(json.dumps(config, indent=2) + "\n")
    from infer import Classifier

    runtime = Classifier(exported, threads=args.threads)
    quant_dev = runtime.probabilities(split["dev"])
    quant_threshold, quant_selected = choose_threshold(split["dev"], quant_dev, args.target_precision)
    config["threshold"] = quant_threshold
    (exported / "classifier.json").write_text(json.dumps(config, indent=2) + "\n")
    quant_test = runtime.probabilities(split["test"])
    consistency_rows = split["test"][:20]
    batch_probabilities = runtime.probabilities(consistency_rows)
    single_probabilities = np.asarray([runtime.probabilities([row])[0] for row in consistency_rows])
    sizes = {path.name: path.stat().st_size for path in exported.iterdir() if path.is_file()}
    result = {
        **float_result,
        "onnx_int8": {
            "threshold": quant_threshold, "threshold_selection": quant_selected,
            "test": evaluate(split["test"], quant_test, quant_threshold),
            "float_int8_mean_probability_difference": float(np.mean(np.abs(test_probabilities - quant_test))),
            "float_int8_max_probability_difference": float(np.max(np.abs(test_probabilities - quant_test))),
            "float_int8_verdict_differences_at_selected_thresholds": int(np.sum((test_probabilities >= threshold) != (quant_test >= quant_threshold))),
            "batch_single_consistency": {
                "rows": len(consistency_rows),
                "max_probability_difference": float(np.max(np.abs(batch_probabilities - single_probabilities))),
                "verdict_differences": int(np.sum((batch_probabilities >= quant_threshold) != (single_probabilities >= quant_threshold))),
            },
            "artifact_bytes": sizes, "bundle_bytes": sum(sizes.values()),
            "benchmark": benchmark(exported, split["test"], args.threads),
        },
    }
    if extra:
        quant_extra_probabilities = runtime.probabilities(extra)
        result["onnx_int8"]["extra_eval"] = evaluate(extra, quant_extra_probabilities, quant_threshold)
    else:
        extra_probabilities, quant_extra_probabilities = [], []
    result["onnx_int8"]["sensitivity_99"] = sensitivity_99(split, quant_dev, quant_test, extra, quant_extra_probabilities)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--corpus", type=Path, required=True)
    parser.add_argument("--labels", type=Path, required=True)
    parser.add_argument("--extra-eval", type=Path)
    parser.add_argument("--extra-labels", type=Path)
    parser.add_argument("--output", type=Path, default=Path(__file__).parent / "artifacts")
    parser.add_argument("--results", type=Path, default=Path(__file__).parent / "results.json")
    parser.add_argument("--encoder", default="sentence-transformers/all-MiniLM-L6-v2",
                        help="BERT-compatible checkpoint; use modernbert_train.py for ModernBERT")
    parser.add_argument("--revision", default="1110a243fdf4706b3f48f1d95db1a4f5529b4d41")
    parser.add_argument("--seed", type=int, default=42)
    parser.add_argument("--target-precision", type=float, default=0.95)
    parser.add_argument("--epochs", type=int, default=3)
    parser.add_argument("--batch-size", type=int, default=16)
    parser.add_argument("--inference-batch-size", type=int, default=1,
                        help="Fixed ONNX evaluation/inference policy; singleton avoids dynamic-quantization batch dependence")
    parser.add_argument("--max-length", type=int, default=384)
    parser.add_argument("--learning-rate", type=float, default=3e-5)
    parser.add_argument("--threads", type=int, default=4)
    parser.add_argument("--baseline-only", action="store_true")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    rows = join_labels(read_jsonl(args.corpus), read_jsonl(args.labels))
    if bool(args.extra_eval) != bool(args.extra_labels):
        raise ValueError("--extra-eval and --extra-labels must be supplied together")
    extra = join_labels(read_jsonl(args.extra_eval), read_jsonl(args.extra_labels)) if args.extra_eval else []
    split, checks = split_rows(rows, args.seed)
    if any(len({row["label"] for row in part}) != 2 for part in split.values()):
        raise ValueError("Every fold must contain both labels")
    split_ids = {name: [row["id"] for row in part] for name, part in split.items()}
    (args.output / "split.json").write_text(json.dumps(split_ids, indent=2) + "\n")
    result = {
        "corpus_sha256": hashlib.sha256(args.corpus.read_bytes()).hexdigest(),
        "labels_sha256": hashlib.sha256(args.labels.read_bytes()).hexdigest(),
        "input_fields": ["kind", "text", "context"], "split": checks,
        "python": sys.version, "platform": platform.platform(),
        "packages": {package: importlib.metadata.version(package) for package in
                     ["numpy", "scikit-learn", "torch", "transformers", "onnx", "onnxruntime", "tokenizers", "psutil"]},
        "configuration": {key: str(value) if isinstance(value, Path) else value for key, value in vars(args).items()},
    }
    if extra:
        result["extra_eval_checks"] = {
            "rows": len(extra),
            "corpus_sha256": hashlib.sha256(args.extra_eval.read_bytes()).hexdigest(),
            "labels_sha256": hashlib.sha256(args.extra_labels.read_bytes()).hexdigest(),
            "normalized_prose_overlap_with_training_corpus": len({normalized_text(r) for r in rows} & {normalized_text(r) for r in extra}),
            "note": "Extra evaluation does not select thresholds, checkpoints, or hyperparameters",
        }
    result["tfidf"] = train_baseline(split, args.output, args.target_precision, args.threads, extra)
    write_results(args.results, result)
    print(json.dumps({"baseline_test": {key: result["tfidf"]["test"][key] for key in
                                       ["flag_precision", "flag_recall", "confusion"]}}), flush=True)
    if not args.baseline_only:
        result["minilm"] = train_encoder(split, args.output, args, extra)
        write_results(args.results, result)
    print(f"Results: {args.results}", flush=True)


if __name__ == "__main__":
    main()
