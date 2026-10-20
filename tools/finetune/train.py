#!/usr/bin/env python3
"""Fine-tune Clef-Flash (LoRA on the backbone + the joint schema head) on the gold TRAIN split.

Same rendered state and questions as the bake-off (render_state, questions_for). Picks the epoch by
dev loss, then writes bake-off run files for dev and test under tools/bakeoff/runs/<name>/.
Checkpoints go to --out (keep it off the repo disk). The gold set is private: pass its path.
"""
import argparse
import glob
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
sys.path[:0] = [BAKEOFF, os.path.join(HERE, "..", "dataset", "v2")]
from judges.systemone import convert  # noqa: E402
from render import questions_for, render_state, render_verify  # noqa: E402
from split import split_of  # noqa: E402

SRC_SCHEMA = os.path.join(HERE, "..", "..", "src", "schema")
SCHEMAS = [os.path.join(SRC_SCHEMA, "strict-v1.json"), os.path.join(BAKEOFF, "schema", "strict-v0-paraphrase.json")]
VERIFY_SCHEMA = os.path.join(SRC_SCHEMA, "verify-v1.json")
STALE_SCHEMA = os.path.join(SRC_SCHEMA, "stale-v2.json")
DEFAULT_MODEL = (glob.glob("/mnt/fast/huggingface/hub/models--Cloudflare--clef-flash/snapshots/*/") or [""])[0]
# Questions the policy's harm-heavy branches read (hazard, contradiction, info gain, rationale, rename).
WEIGHTS = {"hazard": 2.0, "g1_contradicts": 2.0, "verdict": 2.0, "c2_info_gain": 1.5,
           "c3_rationale": 1.5, "c9_replaceable": 1.5, "c5_contract": 1.5}
LORA_TARGETS = r".*language_model\.layers\.\d+\.(self_attn\.[qkvo]_proj|linear_attn\.(in_proj_qkv|in_proj_z|out_proj)|mlp\.(gate|up|down)_proj)"


class CpuEmbedding(torch.nn.Module):
    """Frozen input embeddings kept in RAM: frees the 2 GB a long training row needs on a 24 GB card."""

    def __init__(self, emb):
        super().__init__()
        self.emb = emb.cpu()

    def forward(self, ids):
        return self.emb(ids.cpu()).to(ids.device)


def load(model_path, ckpt=None, lora_r=0, train=False):
    """Clef model with the head in fp32; `ckpt` holds adapter/ and joint_head.safetensors from training."""
    sys.path.insert(0, model_path)
    from joint_schema_model import ClefModel, JointSchemaHead
    from safetensors.torch import load_file
    from transformers import AutoProcessor, Qwen3_5ForConditionalGeneration

    backbone = Qwen3_5ForConditionalGeneration.from_pretrained(model_path, dtype=torch.bfloat16, device_map={"": "cpu"})
    del backbone.model.visual  # text-only states; saves ~1 GB of VRAM
    backbone.config.use_cache = False
    backbone.cuda()
    if ckpt and os.path.isdir(os.path.join(ckpt, "adapter")):
        from peft import PeftModel
        backbone = PeftModel.from_pretrained(backbone, os.path.join(ckpt, "adapter")).merge_and_unload()
    elif lora_r and train:
        from peft import LoraConfig, get_peft_model
        backbone = get_peft_model(backbone, LoraConfig(r=lora_r, lora_alpha=2 * lora_r, lora_dropout=0.05,
                                                       target_modules=LORA_TARGETS))
    else:
        backbone.requires_grad_(False)
    head = JointSchemaHead(**json.load(open(os.path.join(model_path, "joint_head_config.json"))))
    head_file = os.path.join(ckpt, "joint_head.safetensors") if ckpt else os.path.join(model_path, "joint_head.safetensors")
    head.load_state_dict(load_file(head_file), strict=True)
    model = ClefModel(backbone, head.float().cuda())
    if train:
        text = model.language_model.get_base_model() if lora_r else model.language_model
        text.model.language_model.embed_tokens = CpuEmbedding(text.model.language_model.embed_tokens)
        backbone.gradient_checkpointing_enable(gradient_checkpointing_kwargs={"use_reentrant": False})
        model.train()
    else:
        model.eval()
    return model, AutoProcessor.from_pretrained(model_path)


def example(row, schema, tokenizer, max_length=16384):
    from joint_schema_model import encode_record
    qs = row.get("questions") or questions_for(schema, row)  # verify rows carry their own state and questions
    enc = encode_record(tokenizer, {"id": row["id"], "state": row.get("state") or render_state(row), "questions": qs},
                        max_length=max_length)
    targets = []
    for q in enc.questions:
        v = row["labels"].get(q.question_id)
        if v is not None:
            targets.append(q.option_ids.index(str(v).lower() if isinstance(v, bool) else str(v)))
        else:
            targets.append(None)
    return enc, targets


def row_loss(logits, enc, targets, smoothing, brier, weighted=True):
    total = wsum = 0.0
    for q, lg, t in zip(enc.questions, logits, targets):
        if t is None:
            continue
        lg = lg.float()
        ce = F.cross_entropy(lg[None], torch.tensor([t], device=lg.device), label_smoothing=smoothing)
        br = ((lg.softmax(-1) - F.one_hot(torch.tensor(t, device=lg.device), len(lg))) ** 2).sum()
        w = WEIGHTS.get(q.question_id, 1.0) if weighted else 1.0
        total, wsum = total + w * (ce + brier * br), wsum + w
    return total / wsum


def forward(model, enc, tokenizer):
    from joint_schema_model import collate_records
    with torch.autocast("cuda", dtype=torch.bfloat16):
        return model(collate_records([enc], tokenizer.pad_token_id, torch.device("cuda")))[0]


@torch.no_grad()
def dev_loss(model, dev, tokenizer):
    model.eval()
    out = sum(row_loss(forward(model, e, tokenizer), e, t, 0.0, 1.0, weighted=False).item() for e, t in dev)
    model.train()
    return out / len(dev)


def save(model, path):
    from safetensors.torch import save_file
    os.makedirs(path, exist_ok=True)
    if hasattr(model.language_model, "save_pretrained"):
        model.language_model.save_pretrained(os.path.join(path, "adapter"))
    save_file({k: v.contiguous() for k, v in model.head.state_dict().items()}, os.path.join(path, "joint_head.safetensors"))


def verify_rows(path_glob, gold_rows):
    """Verify-set candidates as training rows: render_verify state, verify-v1 questions, their labels."""
    gold = {r["id"]: r for r in gold_rows}
    qs = questions_for(json.load(open(VERIFY_SCHEMA)), {})
    out = []
    for f in sorted(glob.glob(path_glob)):
        for line in open(f):
            if line.strip():
                v = json.loads(line)
                g = gold[v["row_id"]]
                out.append({"id": v["row_id"], "questions": qs, "labels": v["labels"],
                            "state": render_verify(g["comment"], v["candidate"], g["annotated_code"], g.get("enclosing_scope", ""))})
    return out


def stale_rows(path_glob):
    """Stale-set rows (change.annotated_code_before + new annotated_code) as training rows: render_state,
    the stale-v2 question, label g1_contradicts = stale_label."""
    qs = questions_for(json.load(open(STALE_SCHEMA)), {})
    out = []
    for f in sorted(glob.glob(path_glob)):
        for line in open(f):
            if line.strip():
                r = json.loads(line)
                out.append({**r, "questions": qs, "labels": {"g1_contradicts": r["stale_label"]}})
    return out


def train(a, rows):
    model, proc = load(a.model, lora_r=a.lora_r, train=True)
    tok = proc.tokenizer
    schemas = [json.load(open(p)) for p in SCHEMAS]
    tr = [r for r in rows if split_of(r["id"]) == "train"]
    dev = [example(r, schemas[0], tok) for r in rows if split_of(r["id"]) == "dev"]
    for kind, extra in (("verify", verify_rows(a.verify, rows) if a.verify else []),
                        ("stale", stale_rows(a.stale) if a.stale else [])):
        sp = [split_of(r["id"].split("#")[0]) for r in extra]  # gold row id: a row's variants stay in its split
        tr += [r for r, s in zip(extra, sp) if s == "train"]
        dev += [example(r, None, tok) for r, s in zip(extra, sp) if s == "dev"]
        if extra:
            print(f"{kind} rows: {sp.count('train')} train, {sp.count('dev')} dev", flush=True)
    lora = [p for n, p in model.language_model.named_parameters() if p.requires_grad]
    groups = [{"params": list(model.head.parameters()), "lr": a.head_lr, "weight_decay": 0.01}]
    if lora:
        groups.append({"params": lora, "lr": a.lr, "weight_decay": 0.0})
    opt = torch.optim.AdamW(groups)
    steps = a.epochs * math.ceil(len(tr) / a.accum)
    warm = max(1, steps // 20)
    sched = torch.optim.lr_scheduler.LambdaLR(opt, lambda s: min((s + 1) / warm, max(0.0, (steps - s) / (steps - warm))))
    rng = random.Random(a.seed)
    best = dev_loss(model, dev, tok)
    print(f"epoch 0 dev loss {best:.4f} (zero-shot)", flush=True)
    log = [{"epoch": 0, "dev_loss": best}]
    t0, step = time.time(), 0
    for ep in range(1, a.epochs + 1):
        rng.shuffle(tr)
        run = 0.0
        for i, r in enumerate(tr):
            # shortcut: 4096 cuts the state tail of 1/599 train rows to fit 24 GB; raise on a bigger card.
            enc, tg = example(r, schemas[rng.random() < a.paraphrase], tok, max_length=4096)
            loss = row_loss(forward(model, enc, tok), enc, tg, a.smoothing, a.brier)
            (loss / a.accum).backward()
            run += loss.item()
            if (i + 1) % a.accum == 0 or i + 1 == len(tr):
                torch.nn.utils.clip_grad_norm_([p for g in groups for p in g["params"]], 1.0)
                opt.step(), sched.step(), opt.zero_grad(set_to_none=True)
                step += 1
                if step % 10 == 0:
                    print(f"ep {ep} step {step}/{steps} train loss {run / (i + 1):.4f} "
                          f"{(time.time() - t0) / 60:.1f} min, peak {torch.cuda.max_memory_allocated() / 2**30:.1f} GiB", flush=True)
        dl = dev_loss(model, dev, tok)
        log.append({"epoch": ep, "train_loss": run / len(tr), "dev_loss": dl, "minutes": (time.time() - t0) / 60})
        print(f"epoch {ep} dev loss {dl:.4f}", flush=True)
        if dl < best:
            best = dl
            save(model, os.path.join(a.out, "best"))
            log[-1]["saved"] = True
    json.dump({"args": vars(a), "log": log}, open(os.path.join(a.out, "train_log.json"), "w"), indent=1)
    del model, opt
    torch.cuda.empty_cache()


def predict(a, rows):
    from joint_schema_model import systemone
    ckpt = os.path.join(a.out, "best")
    model, proc = load(a.model, ckpt=ckpt if os.path.isdir(ckpt) else None)
    schema = json.load(open(SCHEMAS[0]))
    out_dir = os.path.join(BAKEOFF, "runs", a.name)
    os.makedirs(out_dir, exist_ok=True)
    for r in rows:
        if split_of(r["id"]) not in ("dev", "test"):
            continue
        t = time.time()
        with torch.autocast("cuda", dtype=torch.bfloat16):
            resp = systemone(model, proc, {"model": a.name, "state": render_state(r), "questions": questions_for(schema, r)})
        answers = convert(resp["answers"])
        v = answers.get("verdict")
        rec = {"id": r["id"], "judge": a.name, "answers": answers, "verdict": max(v, key=v.get) if v else None,
               "rewrite": None, "latency_ms": (time.time() - t) * 1000,
               "input_tokens": resp["usage"]["input_tokens"], "raw": None}
        with open(os.path.join(out_dir, r["id"] + ".json"), "w") as f:
            json.dump(rec, f)
    print(f"wrote runs to {out_dir}")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--gold", required=True)
    ap.add_argument("--model", default=DEFAULT_MODEL, help="Clef-Flash bf16 snapshot directory")
    ap.add_argument("--out", default=os.path.expanduser("~/ft/flash-ft"))
    ap.add_argument("--name", default="flash-ft", help="judge name for the run files")
    ap.add_argument("--epochs", type=int, default=3)
    ap.add_argument("--lora-r", type=int, default=16, help="0 trains the head only")
    ap.add_argument("--lr", type=float, default=1e-4, help="LoRA learning rate")
    ap.add_argument("--head-lr", type=float, default=2e-5)
    ap.add_argument("--accum", type=int, default=8)
    ap.add_argument("--smoothing", type=float, default=0.1)
    ap.add_argument("--brier", type=float, default=1.0)
    ap.add_argument("--paraphrase", type=float, default=0.3, help="fraction of rows with paraphrased questions")
    ap.add_argument("--seed", type=int, default=0)
    ap.add_argument("--verify", help="glob of verify-set jsonl parts to mix into training (private)")
    ap.add_argument("--stale", help="glob of stale-set jsonl parts to mix into training (private)")
    ap.add_argument("--predict-only", action="store_true")
    a = ap.parse_args()
    torch.manual_seed(a.seed)
    os.makedirs(a.out, exist_ok=True)
    rows = [json.loads(line) for line in open(a.gold) if line.strip()]
    if not a.predict_only:
        train(a, rows)
    predict(a, rows)


if __name__ == "__main__":
    main()
