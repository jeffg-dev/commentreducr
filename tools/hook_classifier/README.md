# Python documentation classifier experiment

This experiment classifies an entire Python comment or docstring as PASS or
FLAG under [RUBRIC.md](RUBRIC.md). Any offending passage flags the whole block.
The inference prototype gives fixed feedback and does not generate replacements.

The experiment lives entirely under `tools/hook_classifier`. It provides corpus
generation, validated annotations, grouped evaluation, and offline model export.
Changed-file selection and a production `commentreducr --hook` are follow-up work.
See [REPORT.md](REPORT.md) for measured quality, CPU performance, and conclusions.

## Data and labels

- `corpus.jsonl`: 954 syntactically valid Python examples from 240 scenarios. Actual
  `claude --model sonnet` generation resolved to `claude-sonnet-5-5`. Each scenario
  contains several styles of the same task. Six invalid examples were excluded.
- `labels.jsonl`: 954 GPT-6 Luna annotations, with rule, exact evidence, rationale,
  and a fingerprint of the model input: 425 FLAG, 529 PASS.
- `curated_holdout.jsonl` and `curated_holdout_labels.jsonl`: 134 earlier handcrafted
  synthetic Python examples, independently labeled by Luna under the hook rubric.
  Their context is limited to the next code line or signature/body preview.
  Revised distribution: 106 FLAG, 28 PASS.
- `generation.json`: generation provenance and response-level usage estimates.
- `pilot_audit.json` and `label_audit.json`: initial labeling agreement and review
  decisions. The 64-row audit deliberately sampled equal numbers of PASS and FLAG;
  it is not population representative.
- `*_initial.json` and `*_initial.jsonl`: preserved first-run results and labels.
  That labeling procedure assigned default PASS to unlisted rows and missed
  disallowed passages. Its quality metrics are exploratory and superseded.
- `explicit_labels_a.jsonl`, `explicit_labels_b.jsonl`, and `explicit_labels_c.jsonl`:
  fresh Luna reviews of all main and curated targets. Every target has an explicit
  decision and a specific rationale, including PASS. No verdict is assigned by
  a script default. Reviewers see only the target, kind, context, and ID.
- `explicit_review.json` and `explicit_audit.jsonl`: replacement labeling provenance
  and a blind 64-row comparison, with 55 matching verdicts. The audit did not alter
  frozen labels or select training parameters.

The classifier sees only `kind`, `text`, and `context`. Labels, scenario IDs,
generator identity, and example ordering are excluded from its input. Targets are
matched against Python AST docstrings or tokenized comment blocks before acceptance.

Scenarios, and scenarios linked by identical normalized target prose, stay within
one fold. The seeded split is 670 train / 142 development / 142 test. The curated
holdout never selects thresholds or checkpoints. Quality metrics measure agreement
with reviewed Luna labels, rather than human-validated accuracy on real repositories.
The exact row IDs in each fold are recorded in `splits.json`.

## Reproduce training

Commands below run from the repository root. Python 3.12 and `uv` were used. The
dependencies are isolated from the Rust CLI.

```sh
uv venv --python 3.12 tools/hook_classifier/.venv
uv pip install --python tools/hook_classifier/.venv/bin/python torch==2.9.1+cpu --index-url https://download.pytorch.org/whl/cpu
uv pip install --python tools/hook_classifier/.venv/bin/python -r tools/hook_classifier/requirements.txt
```

Download the public, pinned pretrained encoder once:

```sh
HF_HOME=tools/hook_classifier/cache tools/hook_classifier/.venv/bin/python - <<'PY'
from huggingface_hub import snapshot_download
snapshot_download(
    "sentence-transformers/all-MiniLM-L6-v2",
    revision="1110a243fdf4706b3f48f1d95db1a4f5529b4d41",
    allow_patterns=["config.json", "model.safetensors", "tokenizer.json", "tokenizer_config.json", "special_tokens_map.json", "vocab.txt"],
)
PY
```

Train and evaluate using the supplied frozen corpus and labels:

```sh
HF_HOME=tools/hook_classifier/cache HF_HUB_OFFLINE=1 tools/hook_classifier/.venv/bin/python tools/hook_classifier/train.py \
  --corpus tools/hook_classifier/corpus.jsonl \
  --labels tools/hook_classifier/labels.jsonl \
  --extra-eval tools/hook_classifier/curated_holdout.jsonl \
  --extra-labels tools/hook_classifier/curated_holdout_labels.jsonl
```

This trains a word/character TF-IDF logistic regression baseline, then fine-tunes
all MiniLM encoder weights for three epochs on CPU. Development data selects the
checkpoint and flag threshold to maximize recall at 95% observed flag precision.
A separate 99% development precision threshold is reported as a predeclared
sensitivity comparison. Neither target guarantees precision on new data.

The complete target text is retained. Only context may be truncated. The run uses
a 384-token budget, which retained all generated targets and their context.
Quantized exports record a singleton inference policy. Their thresholds and
evaluation use that same policy so neighboring blocks do not change scores.

### ModernBERT comparison

The second experiment uses ModernBERT-base on exactly the same frozen inputs,
folds, three epochs, and development threshold policy. It fine-tunes all encoder
weights with eight CPU threads. The scripts share evaluation and export helpers;
the architecture has its own attention configuration, padding ID, and ONNX inputs.
No additional exporter dependency is required.

Download the pinned encoder, then run the comparison:

```sh
HF_HOME=tools/hook_classifier/cache tools/hook_classifier/.venv/bin/python - <<'PY'
from huggingface_hub import snapshot_download
snapshot_download(
    "answerdotai/ModernBERT-base",
    revision="8949b909ec900327062f0ebf497f51aef5e6f0c8",
    allow_patterns=["config.json", "model.safetensors", "tokenizer.json", "tokenizer_config.json", "special_tokens_map.json"],
)
PY
HF_HOME=tools/hook_classifier/cache HF_HUB_OFFLINE=1 tools/hook_classifier/.venv/bin/python tools/hook_classifier/modernbert_train.py
```

The run writes `results_modernbert.json` and a separate
`artifacts/modernbert/modernbert-int8` inference bundle. It verifies input hashes
and fold membership against this experiment before training. MiniLM results and
artifacts are preserved.

One additional exploratory export keeps embeddings in float32 and quantizes only
MatMul. It uses the same frozen checkpoint and singleton policy, selecting its
threshold from development data. After the ModernBERT run above:

```sh
tools/hook_classifier/.venv/bin/python tools/hook_classifier/quantization_compare.py
```

This writes `results_modernbert_mixed.json` and
`artifacts/modernbert/modernbert-mixed`, preserving the full int8 export. It adds
substantial bundle size with little quality improvement in this experiment.

## Offline inference

The exported `artifacts/minilm-int8` bundle contains quantized ONNX weights, the
tokenizer, and the frozen threshold. Regular inference needs `numpy`, `tokenizers`,
and `onnxruntime`; `psutil` is used only for benchmarks. It needs no server, Python
training packages, or network access after export.

```sh
tools/hook_classifier/.venv/bin/python tools/hook_classifier/infer.py \
  --model tools/hook_classifier/artifacts/minilm-int8 \
  --input tools/hook_classifier/curated_holdout.jsonl
```

Input is JSONL with `kind`, `text`, and `context`; `id` is optional. Output contains
the verdict, raw flag score, and generic rubric feedback for flags. The raw score
is not a calibrated confidence estimate. The binary model does not predict a
specific violated rule or quote an offending span. Long targets that exceed the
token budget require chunking or abstention in a future hook integration.
Initial ModernBERT int8 decisions changed with batch composition. The replacement
export records `inference_batch_size: 1` and processes each block independently;
its threshold is selected using that same policy. Older bundles without this
setting retain their original batching behavior.

Add `--benchmark` to measure fresh-process load and warm inference. Benchmark
results use new processes with potentially warm OS file caches, and RSS is sampled.
Model artifacts and caches remain local and are excluded from Git.
To use ModernBERT, give `infer.py` its exported bundle directory instead.

## Generate another corpus

Generation uses the installed Claude CLI and its existing login, with tools
disabled. The default is 240 scenarios, four requested examples per scenario.
Generation currently requires a POSIX system. Raw responses are cached under
`raw/` in the output directory. Use a fresh output directory for a new independent
run rather than replacing this experiment's frozen inputs. An exclusive lock
prevents simultaneous generators from changing the same cached sources.
Cached responses must match the recorded prompt, model, and effort. Partial
responses fail validation; invalid individual Python targets are recorded and
excluded. Older caches without request fingerprints require a fresh directory.
Failed batches leave any existing assembled corpus intact and record failure
details under `raw/`.

```sh
python3 tools/hook_classifier/corpus.py generate --scenarios 240 --workers 3 \
  --output-dir tools/hook_classifier/artifacts/new-corpus
```

Label the resulting rows independently with GPT-6 Luna under `RUBRIC.md`, without
showing it the generator prompts or any intended styles. Each annotation requires
`id`, `verdict`, `rules`, `evidence`, `reason`, and `labeler_model: "gpt-6-luna"`.
Give each row an explicit decision and a target-specific rationale, including PASS.
Do not assign PASS to rows omitted from a list of flags. `rules` and `evidence`
must be arrays. Validate and combine the resulting JSONL output with:

```sh
python3 tools/hook_classifier/corpus.py assemble path/to/labels_a.jsonl path/to/labels_b.jsonl \
  --corpus tools/hook_classifier/artifacts/new-corpus/corpus.jsonl \
  --output tools/hook_classifier/artifacts/new-corpus/labels.jsonl
```

Validation requires exact evidence substrings and exactly one label per example.
The audited decisions and original labeling files are retained for inspection.

## Checks

```sh
tools/hook_classifier/.venv/bin/python -m unittest discover -s tools/hook_classifier -p 'test_*.py'
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
```
