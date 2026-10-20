"""System One decision-model judge: one POST per item, probabilities straight from the model."""
import json
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
from . import post_json  # noqa: E402

from render import questions_for, render_state  # noqa: E402

DEFAULT_ENDPOINT = "http://127.0.0.1:8100/v1/systemone"
DEFAULT_SCHEMA = os.path.join(os.path.dirname(__file__), "..", "..", "..", "src", "schema", "strict-v1.json")
DEFAULT_MODELS = {"flash": "clef-flash-4bit", "clef27": "clef-4bit"}
EXTRA_QID = "extra_question"  # NEXT 2.4 robustness check: an unrelated question added to the schema


def convert(answers):
    out = {}
    for q, a in answers.items():
        if a["type"] == "noul":
            out[q] = {"true": a["noul"], "false": 1 - a["noul"]}
        else:
            out[q] = dict(a["probabilities"])
    return out


def run_one(row, o):
    with open(o.get("schema") or DEFAULT_SCHEMA) as f:
        schema = json.load(f)
    questions = dict(questions_for(schema, row))
    if o.get("append_question"):
        questions[EXTRA_QID] = {"type": "noul", "instructions": o["append_question"]}
    resp = post_json(
        o["endpoint"], {"model": o["model"], "state": render_state(row), "questions": questions}
    )
    answers = convert(resp["answers"])
    v = answers.get("verdict")
    return {
        "answers": answers,
        "verdict": max(v, key=v.get) if v else None,
        "rewrite": None,
        "input_tokens": (resp.get("usage") or {}).get("input_tokens"),
        "raw": resp,
    }
