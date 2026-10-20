"""The current crate pipeline (Gemma via `reduce --eval`) as a judge: delete / rewrite / keep only."""
import json
import os
import re
import subprocess
import sys
import tempfile

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from render import render_state  # noqa: E402

DEFAULT_ENDPOINT = "http://localhost:8000/v1"
DEFAULT_MODEL = "gemma-4-e2b-it-4bit"
REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))
RUNS = os.path.join(REPO, "tools", "bakeoff", "runs")
MARK = "NOT_A_REAL_LABEL"  # src/eval.rs echoes `output` back, which lets us parse each line
LINE = re.compile(r"^(?:ok  |MISS) (\S+)\s+expected: " + MARK + r"\s+got: (.*)$")


def comment_row(row, state):
    ctx = render_state(row) if state else row["annotated_code"].split("\n", 1)[0]
    return {"id": row["id"], "language": row["language"], "comment": row["comment"],
            "context": ctx, "output": MARK}


def doc_row(row):
    lines = row["annotated_code"].split("\n")
    sig = lines[0]
    return {"id": row["id"], "kind": "module" if row.get("placement") == "module_head"
            else "class" if sig.lstrip().startswith("class") else "function",
            "name": (row.get("symbol") or {}).get("name") or "", "signature": sig,
            "is_test": bool((row.get("symbol") or {}).get("is_test")),
            "in_test_file": "test" in (row.get("source") or {}).get("path", ""),
            "body_lines": len(lines) - 1, "body_preview": lines[1:11],
            "docstring": row["comment"], "output": MARK}


def strip_markers(text):
    text = re.sub(r"^\s*(#+|//+|/\*+|\*+/?|\"\"\"|''')\s?", "", text, flags=re.M)
    return " ".join(re.sub(r"(\*/|\"\"\"|''')\s*$", "", text).lower().split())


def to_result(row, got):
    if got.strip() == "DELETE":
        return {"verdict": "delete", "rewrite": None}
    if strip_markers(got) == strip_markers(row["comment"]):
        return {"verdict": "keep", "rewrite": None}
    return {"verdict": "rewrite_shorter", "rewrite": got}


def run_eval(rows, scope, o):
    if not rows:
        return {}
    os.makedirs(RUNS, exist_ok=True)
    with tempfile.NamedTemporaryFile("w", suffix=".jsonl", dir=RUNS, delete=False) as f:
        f.write("".join(json.dumps(r) + "\n" for r in rows))
    try:
        cmd = os.environ.get("COMMENTREDUCR_BIN", "cargo run --release --quiet --").split()
        cmd += ["reduce", "--scope", scope, "--eval", f.name,
                "--endpoint", o["endpoint"], "--model", o["model"]]
        if o.get("api_key"):
            cmd += ["--api-key", o["api_key"]]
        p = subprocess.run(cmd, cwd=REPO, capture_output=True, text=True)
    finally:
        os.unlink(f.name)
    if p.returncode:
        raise RuntimeError(p.stderr[-2000:])
    got = {}
    for line in p.stdout.splitlines():
        m = LINE.match(line)
        if m:
            got[m.group(1)] = (m.group(2), line)
    return got


def run_batch(rows, o):
    """Returns {id: result}; every row of a batch shares the batch's wall time (set by run_judge)."""
    state = o["judge"] == "gemma-state"
    docs = [r for r in rows if r["comment_kind"] == "api_doc" and r["language"] == "python"]
    rest = [r for r in rows if r not in docs]
    got = run_eval([comment_row(r, state) for r in rest], "comments", o)
    got.update(run_eval([doc_row(r) for r in docs], "docstrings", o))
    out = {}
    for r in rows:
        if r["id"] in got:
            text, line = got[r["id"]]
            out[r["id"]] = {"answers": {}, "input_tokens": None, "raw": line, **to_result(r, text)}
    return out
