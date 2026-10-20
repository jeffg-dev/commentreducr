#!/usr/bin/env python3
"""Encoder control: ModernBERT with one softmax head per schema question, trained on the gold TRAIN split.

Input is the bake-off's render_state text (the questions are the heads, so no question text). Same
loss as train.py (CE with label smoothing + Brier, same question weights). Picks the epoch by dev
loss, then writes bake-off run files for dev and test under tools/bakeoff/runs/<name>/.
"""
import argparse
import json
import math
import os
import random
import sys
import time

import torch
import torch.nn.functional as F

HERE = os.path.dirname(os.path.abspath(__file__))
BAKEOFF = os.path.join(HERE, "..", "bakeoff")
sys.path[:0] = [HERE, BAKEOFF, os.path.join(HERE, "..", "dataset", "v2")]
from render import questions_for, render_state  # noqa: E402
from split import split_of  # noqa: E402
from train import WEIGHTS  # noqa: E402

SCHEMA = json.load(open(os.path.join(BAKEOFF, "schema", "strict-v0.json")))


def options(q):
    return ["true", "false"] if q["type"] == "noul" else ["0", "1", "2", "3"] if q["type"] == "score" else list(q["criteria"])


OPTIONS = {qid: options(q) for qid, q in SCHEMA["questions"].items()}


class Encoder(torch.nn.Module):
    def __init__(self, model):
        super().__init__()
        from transformers import AutoModel
        self.enc = AutoModel.from_pretrained(model, attn_implementation="sdpa")
        h = self.enc.config.hidden_size
        self.drop = torch.nn.Dropout(0.1)
        self.heads = torch.nn.ModuleDict({q: torch.nn.Linear(h, len(o)) for q, o in OPTIONS.items()})

    def forward(self, ids):
        pooled = self.drop(self.enc(input_ids=ids).last_hidden_state.mean(1))  # one row, no padding
        return {q: head(pooled)[0].float() for q, head in self.heads.items()}


def example(row, tok, max_length):
    # right truncation drops the scope end first: enclosing_scope is the last block of the state
    ids = tok(render_state(row), truncation=True, max_length=max_length, return_tensors="pt")["input_ids"]
    qs = questions_for(SCHEMA, row)
    targets = {}
    for q in qs:
        v = row["labels"].get(q)
        if v is not None:
            targets[q] = OPTIONS[q].index(str(v).lower() if isinstance(v, bool) else str(v))
    return ids, qs, targets


def row_loss(logits, targets, smoothing, brier, weighted=True):
    total = wsum = 0.0
    for q, t in targets.items():
        lg = logits[q]
        tt = torch.tensor(t, device=lg.device)
        ce = F.cross_entropy(lg[None], tt[None], label_smoothing=smoothing)
        br = ((lg.softmax(-1) - F.one_hot(tt, len(lg))) ** 2).sum()
        w = WEIGHTS.get(q, 1.0) if weighted else 1.0
        total, wsum = total + w * (ce + brier * br), wsum + w
    return total / wsum


def forward(model, ids):
    with torch.autocast("cuda", dtype=torch.bfloat16):
        return model(ids.cuda())


@torch.no_grad()
def dev_loss(model, dev):
    model.eval()
    out = sum(row_loss(forward(model, ids), t, 0.0, 1.0, weighted=False).item() for ids, _, t in dev)
    model.train()
    return out / len(dev)


def train(a, rows, tok):
    model = Encoder(a.model).cuda()
    if a.grad_ckpt:
        model.enc.gradient_checkpointing_enable(gradient_checkpointing_kwargs={"use_reentrant": False})
    tr = [example(r, tok, a.max_length) for r in rows if split_of(r["id"]) == "train"]
    dev = [example(r, tok, a.max_length) for r in rows if split_of(r["id"]) == "dev"]
    opt = torch.optim.AdamW([{"params": model.enc.parameters(), "lr": a.lr},
                             {"params": model.heads.parameters(), "lr": a.head_lr}], weight_decay=0.01)
    steps = a.epochs * math.ceil(len(tr) / a.accum)
    warm = max(1, steps // 10)
    sched = torch.optim.lr_scheduler.LambdaLR(opt, lambda s: min((s + 1) / warm, max(0.0, (steps - s) / (steps - warm))))
    rng = random.Random(a.seed)
    best, best_state, bad = float("inf"), None, 0
    log, t0 = [], time.time()
    model.train()
    for ep in range(1, a.epochs + 1):
        rng.shuffle(tr)
        run = 0.0
        for i, (ids, _, tg) in enumerate(tr):
            loss = row_loss(forward(model, ids), tg, a.smoothing, a.brier)
            (loss / a.accum).backward()
            run += loss.item()
            if (i + 1) % a.accum == 0 or i + 1 == len(tr):
                torch.nn.utils.clip_grad_norm_(model.parameters(), 1.0)
                opt.step(), sched.step(), opt.zero_grad(set_to_none=True)
        dl = dev_loss(model, dev)
        log.append({"epoch": ep, "train_loss": run / len(tr), "dev_loss": dl, "minutes": (time.time() - t0) / 60})
        print(f"epoch {ep} train loss {run / len(tr):.4f} dev loss {dl:.4f} {(time.time() - t0) / 60:.1f} min, "
              f"peak {torch.cuda.max_memory_allocated() / 2**30:.1f} GiB", flush=True)
        if dl < best:
            best, bad, log[-1]["saved"] = dl, 0, True
            best_state = {k: v.detach().cpu().clone() for k, v in model.state_dict().items()}
        else:
            bad += 1
            if bad >= a.patience:
                break
    from safetensors.torch import save_file
    os.makedirs(a.out, exist_ok=True)
    save_file(best_state, os.path.join(a.out, "model.safetensors"))
    json.dump({"args": vars(a), "log": log}, open(os.path.join(a.out, "train_log.json"), "w"), indent=1)
    del model, opt
    torch.cuda.empty_cache()


@torch.no_grad()
def predict(a, rows, tok):
    from safetensors.torch import load_file
    model = Encoder(a.model)
    model.load_state_dict(load_file(os.path.join(a.out, "model.safetensors")))
    model.cuda().eval()
    out_dir = os.path.join(BAKEOFF, "runs", a.name)
    os.makedirs(out_dir, exist_ok=True)
    rows = [r for r in rows if split_of(r["id"]) in ("dev", "test")]
    forward(model, example(rows[0], tok, a.max_length)[0])  # warm-up, not timed
    for r in rows:
        torch.cuda.synchronize()
        t = time.time()
        ids, qs, _ = example(r, tok, a.max_length)
        logits = forward(model, ids)
        answers = {q: dict(zip(OPTIONS[q], logits[q].softmax(-1).tolist())) for q in qs}
        torch.cuda.synchronize()
        ms = (time.time() - t) * 1000
        v = answers["verdict"]
        rec = {"id": r["id"], "judge": a.name, "answers": answers, "verdict": max(v, key=v.get), "rewrite": None,
               "latency_ms": ms, "input_tokens": ids.shape[1], "raw": None}
        with open(os.path.join(out_dir, r["id"] + ".json"), "w") as f:
            json.dump(rec, f)
    print(f"wrote {len(rows)} runs to {out_dir}")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--gold", required=True)
    ap.add_argument("--model", default="answerdotai/ModernBERT-large")
    ap.add_argument("--out", default=os.path.expanduser("~/ft/modernbert"))
    ap.add_argument("--name", default="modernbert", help="judge name for the run files")
    ap.add_argument("--max-length", type=int, default=8192)
    ap.add_argument("--epochs", type=int, default=10)
    ap.add_argument("--patience", type=int, default=3, help="stop after this many epochs without a better dev loss")
    ap.add_argument("--lr", type=float, default=3e-5, help="encoder learning rate")
    ap.add_argument("--head-lr", type=float, default=1e-3)
    ap.add_argument("--accum", type=int, default=8, help="rows per optimizer step")
    ap.add_argument("--smoothing", type=float, default=0.1)
    ap.add_argument("--brier", type=float, default=1.0)
    ap.add_argument("--grad-ckpt", action="store_true")
    ap.add_argument("--seed", type=int, default=0)
    ap.add_argument("--predict-only", action="store_true")
    a = ap.parse_args()
    torch.manual_seed(a.seed)
    from transformers import AutoTokenizer
    tok = AutoTokenizer.from_pretrained(a.model)
    rows = [json.loads(line) for line in open(a.gold) if line.strip()]
    if not a.predict_only:
        train(a, rows, tok)
    predict(a, rows, tok)


if __name__ == "__main__":
    main()
