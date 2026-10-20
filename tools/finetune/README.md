# Fine-tuning Clef-Flash

`train.py` fine-tunes Clef-Flash (bf16) on the gold TRAIN split with the bake-off's rendered state
and questions (`render_state`, `questions_for`), keeps the epoch with the lowest dev loss, and
writes bake-off run files for dev and test. `serve.py` serves the result on `/v1/systemone`.

The gold set is private: pass its path, keep checkpoints off the repo (default `~/ft/flash-ft`).

## Recipe

- LoRA rank 16 (alpha 32, dropout 0.05) on every backbone linear projection (full attention,
  linear attention, MLP); the joint schema head trained in full, in fp32. bf16 autocast.
- Loss per question: cross-entropy with label smoothing 0.1 plus a Brier term, averaged over the
  questions the row has labels for. Hazard, contradiction and the verdict count double; info gain,
  rationale, replaceability and contract count 1.5.
- 30% of training rows use `strict-v0-paraphrase.json` so the head does not learn one wording.
- AdamW, LoRA lr 1e-4, head lr 2e-5, 8 rows per step, 5% warmup then linear decay, 3 epochs.
- Fits one 24 GB card: gradient checkpointing, the vision tower dropped, input embeddings kept in
  RAM, and training rows capped at 4096 tokens (cuts one row). Dev and test use full length.

## Run

```sh
pip install peft flash-linear-attention   # on top of torch + transformers with Qwen3.5
PYTORCH_CUDA_ALLOC_CONF=expandable_segments:True \
  python tools/finetune/train.py --gold private/gold/gold.jsonl --out ~/ft/flash-ft
python3 tools/bakeoff/tune.py --gold private/gold/gold.jsonl --judge flash-ft --out private/bakeoff/th-flash-ft.json
python3 tools/bakeoff/score.py --gold private/gold/gold.jsonl --judge flash-ft --thresholds private/bakeoff/th-flash-ft.json
python tools/finetune/serve.py --ckpt ~/ft/flash-ft/best --port 8102
```

`--verify 'private/verify/part-*.jsonl'` mixes the TRAIN verify candidates (render_verify state,
`verify-v1.json` questions) into training and the DEV ones into the epoch-picking loss (flash-ft2).
`--stale 'private/stale/part-*.jsonl'` does the same for the stale set: render_state with the code
before and after a change, the `stale-v2.json` question, label `stale_label` (flash-ft3).
`--lora-r 0` trains the head only. `--predict-only` rewrites the run files from `--out/best`.
The checkpoint is `best/adapter/` (PEFT LoRA) plus `best/joint_head.safetensors` (fp32); the
server merges the adapter into the bf16 backbone at load time.

# Encoder control (ModernBERT)

`encoder.py` trains ModernBERT-large with one softmax head per schema question (mean-pooled, no
question text) on the same `render_state` text and the same loss and question weights as
`train.py`. Full fine-tune, bf16 autocast, one row per forward, 8 rows per step, encoder lr 3e-5,
head lr 1e-3, 10% warmup then linear decay, up to 10 epochs, stops after 3 epochs without a
better dev loss and keeps the best. Fits in 15 GB; about 1 minute per epoch on a 3090.

```sh
python tools/finetune/encoder.py --gold private/gold/gold.jsonl --out ~/ft/modernbert
python3 tools/bakeoff/tune.py --gold private/gold/gold.jsonl --judge modernbert --out private/bakeoff/th-modernbert.json
python3 tools/bakeoff/score.py --gold private/gold/gold.jsonl --judge modernbert --thresholds private/bakeoff/th-modernbert.json
```

The checkpoint is `--out/model.safetensors` (encoder and heads). `--predict-only` rewrites the run
files from it; `latency_ms` is one item at batch size 1, tokenizing included.
