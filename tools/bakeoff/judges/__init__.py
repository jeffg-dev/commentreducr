import json
import urllib.request


def post_json(url, body, headers=None, timeout=600):
    req = urllib.request.Request(
        url, json.dumps(body).encode(), {"Content-Type": "application/json", **(headers or {})}
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def one_hot(labels):
    """labels object -> {qid: {option: prob}} one-hot answers (null labels are skipped)."""
    out = {}
    for q, v in labels.items():
        if v is None or q in ("rewrite", "rename_suggestion"):
            continue
        out[q] = {("true" if v else "false") if isinstance(v, bool) else str(v): 1.0}
    return out
