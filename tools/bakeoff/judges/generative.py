"""Generative judge: rubric prompt + rendered state in, labels JSON out. Also used for silver/frontier."""
import json
import os
import re
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
from . import one_hot, post_json  # noqa: E402

from render import render_state  # noqa: E402

DEFAULT_ENDPOINT = "http://localhost:8000/v1"
DEFAULT_MODEL = "gemma-4-26b-a4b-it-4bit"
PROMPT = os.path.join(os.path.dirname(__file__), "..", "..", "..", "src", "schema", "frontier.md")


def parse_labels(text):
    """Last JSON object in the reply that holds a labels object (bare or under "labels")."""
    text = re.sub(r"<think>.*?</think>", "", text, flags=re.S)
    dec, found = json.JSONDecoder(), None
    for m in re.finditer(r"\{", text):
        try:
            obj, _ = dec.raw_decode(text[m.start():])
        except ValueError:
            continue
        obj = obj.get("labels", obj) if isinstance(obj, dict) else None
        if isinstance(obj, dict) and "verdict" in obj:
            found = obj
    return found


def run_one(row, o):
    with open(PROMPT) as f:
        system = f.read()
    headers = {"Authorization": "Bearer " + o["api_key"]} if o.get("api_key") else {}
    resp = post_json(
        o["endpoint"].rstrip("/") + "/chat/completions",
        {
            "model": o["model"],
            "temperature": 0,
            "max_tokens": 4096,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": render_state(row)},
            ],
        },
        headers,
    )
    text = resp["choices"][0]["message"]["content"] or ""
    labels = parse_labels(text) or {}
    return {
        "answers": one_hot(labels),
        "verdict": labels.get("verdict"),
        "rewrite": labels.get("rewrite"),
        "input_tokens": (resp.get("usage") or {}).get("prompt_tokens"),
        "raw": text,
    }
