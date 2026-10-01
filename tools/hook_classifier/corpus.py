#!/usr/bin/env python3
"""Generate Sonnet examples, validate their Python, and assemble Luna labels."""

import argparse
import ast
import concurrent.futures
import fcntl
import hashlib
import inspect
import io
import json
from pathlib import Path
import subprocess
import tokenize

ROOT = Path(__file__).resolve().parent
KINDS = {"comment", "function_docstring", "class_docstring", "module_docstring", "test_docstring"}
RULES = {"narration", "noise", "leaky_reference"}
TOPICS = [
    "cache expiry", "atomic file replacement", "stream buffering", "tenant validation",
    "timezone conversion", "pagination cursors", "retry backoff", "database transactions",
    "thread synchronization", "temporary file cleanup", "unicode normalization", "CSV parsing",
    "HTTP request signing", "rate limiting", "credential redaction", "queue acknowledgement",
    "schema validation", "filesystem symlinks", "decimal rounding", "async cancellation",
    "resource pooling", "configuration precedence", "event deduplication", "binary framing",
    "compression streams", "locale independent sorting", "path validation", "batch processing",
    "environment parsing", "socket timeouts", "feature flags", "memoization",
    "serialization", "identifier normalization", "test fixture isolation", "clock mocking",
    "random seed handling", "permission checks", "incremental decoding", "subprocess cleanup",
    "content hashing", "archive extraction", "numeric overflow", "date boundaries",
    "email address handling", "iterator consumption", "mapping defaults", "ordered updates",
]


def read_jsonl(path):
    return [json.loads(line) for line in Path(path).read_text().splitlines() if line.strip()]


def write_jsonl(path, rows):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("".join(json.dumps(row, ensure_ascii=False) + "\n" for row in rows))


def validate_example(row):
    code = row["code"].rstrip() + "\n"
    text = inspect.cleandoc(row["text"]).strip()
    if row["kind"] not in KINDS or not text:
        raise ValueError("missing text or invalid kind")
    tree = ast.parse(code)
    lines = code.splitlines(keepends=True)
    matches = []
    if row["kind"] == "comment":
        blocks = []
        for token in tokenize.generate_tokens(io.StringIO(code).readline):
            if token.type != tokenize.COMMENT:
                continue
            if blocks and token.start[0] == blocks[-1][-1].start[0] + 1:
                blocks[-1].append(token)
            else:
                blocks.append([token])
        for block in blocks:
            candidate = "\n".join(token.string for token in block)
            if candidate == text:
                matches.append((block[0].start[0], block[-1].end[0], candidate))
    else:
        for node in ast.walk(tree):
            if not isinstance(node, (ast.Module, ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)):
                continue
            candidate = ast.get_docstring(node, clean=True)
            if candidate is None or candidate.strip() != text:
                continue
            actual_kind = (
                "module_docstring" if isinstance(node, ast.Module) else
                "class_docstring" if isinstance(node, ast.ClassDef) else
                "test_docstring" if node.name.startswith("test_") else
                "function_docstring"
            )
            if actual_kind != row["kind"]:
                continue
            target = node.body[0]
            matches.append((target.lineno, target.end_lineno, candidate.strip()))
    if len(matches) != 1:
        raise ValueError(f"target must match exactly one Python block, got {len(matches)}")
    start, end, text = matches[0]
    # Removing the entire documentation span also removes its length from the input context.
    context = "".join(lines[: start - 1] + lines[end:]).strip()
    return {
        **row, "code": code, "text": text, "context": context,
        "target_start_line": start, "target_end_line": end,
        "generator_model": "sonnet", "source": "synthetic",
    }


def response_payload(response):
    if response.get("is_error"):
        raise ValueError(response.get("result", "Claude returned an error"))
    if response.get("structured_output"):
        return response["structured_output"]
    result = response["result"].strip()
    if result.startswith("```"):
        result = result.split("\n", 1)[1].rsplit("```", 1)[0].strip()
    return json.loads(result)


def scenarios(count):
    result = []
    for number in range(count):
        kind = sorted(KINDS)[number % len(KINDS)]
        topic = TOPICS[number % len(TOPICS)]
        setting = ["small command-line application", "web service", "background worker", "data pipeline", "developer utility"][number // len(TOPICS) % 5]
        result.append({"scenario_id": f"py-{number:04d}", "kind": kind, "task": f"{topic} in a {setting}", "variation": number // len(TOPICS)})
    return result


def generator_prompt(batch):
    return """Generate synthetic Python snippets for a documentation-quality experiment.
Return JSON only: {"scenarios": [{"scenario_id": "...", "examples": [{"kind": "...", "code": "...", "text": "..."}]}]}.
For EACH requested scenario below produce FOUR independently implemented examples
of the SAME task, each using its requested kind. Vary identifiers and concrete data.
1. Write documentation naturally as you would for an ordinary coding request.
2. Write concise documentation focused on a meaningful usage contract, constraint,
   unexpected behavior, or test behavior. Prefer specific facts over name restatement.
3. Write expanded explanatory documentation that includes a useful constraint and
   describes implementation or explains a historical or internal integration reason.
4. Write documentation focused on a different meaningful contract or subtle constraint;
   vary its length, sometimes using several sentences of necessary information.
For comment targets, usage contracts should convey a hazard or surprising constraint
beyond what the nearby code plainly shows. For test_docstring targets, document the
behavior or regression being guarded; valid test descriptions need not be hazards.
Do not label or assess examples, and do not include version/style/quality fields.
Each code field must be a complete valid Python snippet, approximately 6-20 lines,
with exactly one target documentation block and no other comments or docstrings.
For comments, text is the exact # prefixed block without indentation. For docstrings,
text is the content without quote delimiters, with continuation indentation removed.
The target must occur exactly once in code. Modules document the whole snippet;
classes document the class; functions document a def; tests document a test_* def.
Use credible behavior, not trivial bodies invented just to attach documentation.
No TODO/NOTE markers, structural directives, licenses, doctests, tool decorators,
prompt/help docstrings, or __doc__ access. History and internal references may appear
in the expanded explanatory examples. No markdown fences around the JSON.
Requested scenarios:\n""" + json.dumps(batch)


def generate_batch(batch_number, batch, raw_dir, timeout):
    output = raw_dir / f"batch_{batch_number:03d}.json"
    prompt_path = raw_dir / f"batch_{batch_number:03d}.prompt.txt"
    request_path = raw_dir / f"batch_{batch_number:03d}.request.json"
    prompt = generator_prompt(batch)
    request = {"prompt_sha256": hashlib.sha256(prompt.encode()).hexdigest(),
               "model": "sonnet", "effort": "low"}
    if output.exists() and output.stat().st_size:
        cached = json.loads(output.read_text())
        if cached.get("is_error"):
            output.rename(raw_dir / f"batch_{batch_number:03d}.interrupted.json")
        elif not request_path.exists() or json.loads(request_path.read_text()) != request:
            raise ValueError("cached response has different or missing request provenance; use a fresh output directory")
    if not output.exists() or not output.stat().st_size:
        prompt_path.write_text(prompt)
        request_path.write_text(json.dumps(request, indent=2) + "\n")
        command = ["claude", "--safe-mode", "--print", "--model", "sonnet", "--effort", "low", "--tools", "", "--no-session-persistence", "--output-format", "json"]
        proc = subprocess.run(command, input=prompt, text=True, capture_output=True, timeout=timeout)
        (raw_dir / f"batch_{batch_number:03d}.stderr.txt").write_text(proc.stderr)
        if proc.returncode:
            raise RuntimeError(f"Claude exit {proc.returncode}: {proc.stderr[-300:]}")
        output.write_text(proc.stdout)
    response = json.loads(output.read_text())
    payload = response_payload(response)
    expected = {item["scenario_id"]: item for item in batch}
    rows, rejected = [], []
    seen = set()
    for scenario in payload["scenarios"]:
        scenario_id = scenario["scenario_id"]
        if scenario_id not in expected or scenario_id in seen:
            raise ValueError(f"unexpected/duplicate scenario {scenario_id}")
        seen.add(scenario_id)
        if len(scenario["examples"]) != 4:
            raise ValueError(f"expected four examples for {scenario_id}")
        for index, example in enumerate(scenario["examples"]):
            row = {**example, "id": f"{scenario_id}-{index}", "scenario_id": scenario_id}
            try:
                if row["kind"] != expected[scenario_id]["kind"]:
                    raise ValueError("kind differs from requested scenario")
                rows.append(validate_example(row))
            except (ValueError, SyntaxError, KeyError, tokenize.TokenError) as exc:
                rejected.append({"id": row["id"], "error": str(exc)})
    if seen != set(expected):
        raise ValueError(f"missing scenarios: {sorted(set(expected) - seen)}")
    meta = {"batch": batch_number, "requested_scenarios": len(batch), "returned_scenarios": len(seen), "rows": len(rows), "rejected": rejected, "model_usage": response.get("modelUsage", {}), "duration_ms": response.get("duration_ms"), "cost_usd": response.get("total_cost_usd")}
    write_jsonl(raw_dir / f"batch_{batch_number:03d}.validated.jsonl", rows)
    (raw_dir / f"batch_{batch_number:03d}.meta.json").write_text(json.dumps(meta, indent=2))
    print(f"batch {batch_number:03d}: {len(rows)} valid, {len(rejected)} rejected", flush=True)
    return rows, meta


def generate(args):
    destination = args.output_dir
    destination.mkdir(parents=True, exist_ok=True)
    with (destination / ".generation.lock").open("a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise SystemExit("Generation is already running in this output directory")
        generate_locked(args, destination)


def generate_locked(args, destination):
    raw = destination / "raw"
    raw.mkdir(exist_ok=True)
    tasks = scenarios(args.scenarios)
    batches = [tasks[i:i + args.batch_scenarios] for i in range(0, len(tasks), args.batch_scenarios)]
    rows, metadata, errors = [], [], []
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.workers) as pool:
        futures = {pool.submit(generate_batch, number, batch, raw, args.timeout): number for number, batch in enumerate(batches)}
        for future in concurrent.futures.as_completed(futures):
            try:
                batch_rows, meta = future.result()
                rows.extend(batch_rows)
                metadata.append(meta)
            except Exception as exc:
                error = {"batch": futures[future], "error": str(exc)}
                errors.append(error)
                print(json.dumps(error), flush=True)
    rows.sort(key=lambda row: row["id"])
    if len({row["id"] for row in rows}) != len(rows):
        raise ValueError("duplicate ids")
    manifest = {"generator": "claude --model sonnet", "scenario_count": args.scenarios, "returned_scenario_count": len({row['scenario_id'] for row in rows}), "rows": len(rows), "rubric_sha256": hashlib.sha256((ROOT / "RUBRIC.md").read_bytes()).hexdigest(), "batches": sorted(metadata, key=lambda item: item["batch"]), "errors": errors}
    if errors:
        (raw / "generation.failed.json").write_text(json.dumps(manifest, indent=2) + "\n")
        raise SystemExit(1)
    write_jsonl(destination / "corpus.jsonl", rows)
    # Partition by scenario, not row, so each labeler sees complete contextual groups.
    for suffix, parity in [("a", 0), ("b", 1)]:
        selected = [row for row in rows if int(row["scenario_id"].split("-")[1]) % 2 == parity]
        write_jsonl(destination / f"batch_{suffix}.jsonl", selected)
    (destination / "generation.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"wrote {len(rows)} examples from {len({row['scenario_id'] for row in rows})} scenarios", flush=True)


def assemble(args):
    corpus = read_jsonl(args.corpus)
    by_id = {row["id"]: row for row in corpus}
    annotations = [row for path in args.labels for row in read_jsonl(path)]
    seen = set()
    for row in annotations:
        if row["id"] not in by_id or row["id"] in seen:
            raise ValueError(f"unknown/duplicate label id {row['id']}")
        seen.add(row["id"])
        if row["verdict"] not in {"PASS", "FLAG"}:
            raise ValueError(f"invalid verdict {row['id']}")
        if row["labeler_model"] != "gpt-6-luna":
            raise ValueError(f"unexpected labeler {row['id']}")
        if not row["reason"].strip():
            raise ValueError(f"missing reason {row['id']}")
        if not isinstance(row["rules"], list) or not isinstance(row["evidence"], list):
            raise ValueError(f"rules and evidence must be arrays {row['id']}")
        if row["verdict"] == "PASS" and (row["rules"] or row["evidence"]):
            raise ValueError(f"PASS with evidence/rules {row['id']}")
        if row["verdict"] == "FLAG":
            if len(row["rules"]) != 1 or row["rules"][0] not in RULES or not row["evidence"]:
                raise ValueError(f"FLAG without valid primary rule/evidence {row['id']}")
            if any(not isinstance(span, str) or not span or span not in by_id[row["id"]]["text"] for span in row["evidence"]):
                raise ValueError(f"evidence is not a target substring {row['id']}")
        inputs = {field: by_id[row["id"]][field] for field in ["kind", "text", "context"]}
        row["input_sha256"] = hashlib.sha256(json.dumps(inputs, sort_keys=True).encode()).hexdigest()
    missing = set(by_id) - seen
    if missing:
        raise ValueError(f"missing {len(missing)} labels: {sorted(missing)[:10]}")
    write_jsonl(args.output, sorted(annotations, key=lambda row: row["id"]))
    print(f"validated {len(annotations)} independent Luna annotations")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    generate_parser = sub.add_parser("generate")
    generate_parser.add_argument("--scenarios", type=int, default=240)
    generate_parser.add_argument("--batch-scenarios", type=int, default=4)
    generate_parser.add_argument("--workers", type=int, default=3)
    generate_parser.add_argument("--timeout", type=int, default=600)
    generate_parser.add_argument("--output-dir", type=Path, default=ROOT)
    generate_parser.set_defaults(run=generate)
    assemble_parser = sub.add_parser("assemble")
    assemble_parser.add_argument("labels", nargs="+")
    assemble_parser.add_argument("--corpus", type=Path, default=ROOT / "corpus.jsonl")
    assemble_parser.add_argument("--output", type=Path, default=ROOT / "labels.jsonl")
    assemble_parser.set_defaults(run=assemble)
    args = parser.parse_args()
    args.run(args)


if __name__ == "__main__":
    main()
