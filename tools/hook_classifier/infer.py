#!/usr/bin/env python3
"""Offline binary inference for the experiment's exported ONNX classifier."""

import argparse
import json
import statistics
import time
from pathlib import Path


def model_input(row):
    # Explicit allowlist: labels, scenario IDs, and generation metadata are excluded.
    return f"Kind: {row['kind']}\nText:\n{row['text']}\nPython context:\n{row['context']}"


def model_pair(row):
    return (f"Kind: {row['kind']}\nText:\n{row['text']}", f"Python context:\n{row['context']}")


class Classifier:
    def __init__(self, directory, threads=4):
        import numpy as np

        self.np = np
        directory = Path(directory)
        self.baseline = None
        if directory.is_file():
            import joblib

            bundle = joblib.load(directory)
            self.baseline = bundle["pipeline"]
            self.config = {"threshold": bundle["threshold"]}
            return
        import onnxruntime as ort
        from tokenizers import Tokenizer

        self.config = json.loads((directory / "classifier.json").read_text())
        self.tokenizer = Tokenizer.from_file(str(directory / "tokenizer.json"))
        self.tokenizer.enable_truncation(max_length=self.config["max_length"], strategy="only_second")
        self.tokenizer.enable_padding(
            pad_id=self.config.get("pad_token_id", 0),
            pad_token=self.config.get("pad_token", "[PAD]"),
        )
        options = ort.SessionOptions()
        options.intra_op_num_threads = threads
        options.inter_op_num_threads = 1
        self.session = ort.InferenceSession(
            str(directory / "model.onnx"), options, providers=["CPUExecutionProvider"]
        )

    def probabilities(self, rows):
        if not rows:
            return self.np.array([])
        if self.baseline is not None:
            return self.baseline.predict_proba([model_input(row) for row in rows])[:, 1]
        batch_size = self.config.get("inference_batch_size", 32)
        if len(rows) > batch_size:
            return self.np.concatenate([self.probabilities(rows[start:start + batch_size])
                                        for start in range(0, len(rows), batch_size)])
        tokens = self.tokenizer.encode_batch([model_pair(row) for row in rows])
        fields = {
            "input_ids": [token.ids for token in tokens],
            "attention_mask": [token.attention_mask for token in tokens],
            "token_type_ids": [token.type_ids for token in tokens],
        }
        feed = {
            item.name: self.np.asarray(fields[item.name], dtype="int64")
            for item in self.session.get_inputs()
        }
        logits = self.session.run(None, feed)[0]
        shifted = logits - logits.max(axis=1, keepdims=True)
        exp = self.np.exp(shifted)
        return (exp / exp.sum(axis=1, keepdims=True))[:, 1]

    def predict(self, rows):
        threshold = self.config["threshold"]
        return [
            {
                "id": row.get("id"),
                "probability_flag": float(probability),
                "verdict": "FLAG" if probability >= threshold else "PASS",
                "feedback": (
                    "Revise this block: it may contain implementation narration, noise, "
                    "or references to a named caller or other module."
                    if probability >= threshold
                    else None
                ),
            }
            for row, probability in zip(rows, self.probabilities(rows))
        ]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model", type=Path, required=True, help="ONNX bundle directory or trusted TF-IDF joblib file")
    parser.add_argument("--input", type=Path, required=True)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--threads", type=int, default=4)
    parser.add_argument("--benchmark", action="store_true")
    parser.add_argument("--limit", type=int, default=20)
    args = parser.parse_args()
    rows = [json.loads(line) for line in args.input.read_text().splitlines() if line]
    started = time.perf_counter()
    classifier = Classifier(args.model, args.threads)
    loaded = time.perf_counter()
    if args.benchmark:
        import psutil

        process = psutil.Process()
        rss_after_load = process.memory_info().rss / 1024**2
        rss_samples = [rss_after_load]
        rows = rows[: args.limit]
        first = classifier.predict(rows[:1])
        first_done = time.perf_counter()
        print(json.dumps({"first": first, "load_and_first_ms": (first_done - started) * 1000}), flush=True)
        rss_after_first = process.memory_info().rss / 1024**2
        rss_samples.append(rss_after_first)
        single_times = []
        for row in rows:
            tick = time.perf_counter()
            classifier.predict([row])
            single_times.append((time.perf_counter() - tick) * 1000)
            rss_samples.append(process.memory_info().rss / 1024**2)
        tick = time.perf_counter()
        classifier.predict(rows)
        batch_ms = (time.perf_counter() - tick) * 1000
        rss_samples.append(process.memory_info().rss / 1024**2)
        print(json.dumps({
            "load_ms": (loaded - started) * 1000,
            "load_and_first_ms": (first_done - started) * 1000,
            "warm_single_median_ms": statistics.median(single_times),
            "warm_single_max_ms": max(single_times),
            "batch_rows": len(rows),
            "warm_batch_ms": batch_ms,
            "rss_after_load_mib": rss_after_load,
            "rss_after_first_mib": rss_after_first,
            "sampled_peak_rss_mib": max(rss_samples),
            "first": first,
        }))
    else:
        predictions = classifier.predict(rows)
        output = "".join(json.dumps(row) + "\n" for row in predictions)
        if args.output:
            args.output.write_text(output)
        else:
            print(output, end="")


if __name__ == "__main__":
    main()
