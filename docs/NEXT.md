# NEXT: gold set and bake-off (Phase 1)

Audience: the agent working on this repo. Everything here runs outside the crate, in Python
under `tools/`. The maintainer's time is the scarce input: the labelling session is theirs,
everything else is yours. Decision models run on the maintainer's Mac (oMLX serves
`/v1/systemone`) or on Workers AI with their key; this container has neither, so build and
test the harness against a mock endpoint and hand it over runnable.

## Outcome

1. `tools/dataset/v2/gold.jsonl`: 300 real comments with full context and 13-question labels,
   labelled by the maintainer. Split 40/60 dev/test by id hash.
2. `tools/bakeoff/`: a harness that renders the same state for every judge, calls five judges,
   tunes policy thresholds on dev, and reports on test.
3. `docs/decisions/judge-bakeoff.md`: the numbers and the architecture decision.

## Step 1: build the gold set

### 1.1 Dataset format v2

One JSON object per line. This is the format for every row the project will ever hold
(gold, mined, generated, donated), so get it right once. Write it up as
`tools/dataset/v2/SCHEMA.md` with this content.

```json
{
  "id": "own-0001",
  "source": {"repo": "github.com/org/repo", "commit": "<sha>", "path": "src/cache.py",
             "line": 120, "license": "MIT", "pr": null},
  "language": "python",
  "comment_kind": "inline",
  "placement": "before_statement",
  "comment": "# The lock must be acquired before touching _cache; ...",
  "annotated_code": "with self._cache_lock:\n    self._cache[key] = value",
  "enclosing_scope": "def set(self, key, value):\n    ...",
  "symbol": {"name": "Cache.set", "public": false, "is_test": false},
  "change": {"authored_by_agent": true, "agent": "claude-code",
             "comment_age_days": 12, "code_newer_than_comment": false},
  "labels": {
    "kind": "warning",
    "g1_contradicts": false,
    "g1_unverifiable": false,
    "hazard": true,
    "c2_info_gain": 2,
    "c3_rationale": 2,
    "c4_level": "lower_precision",
    "c8_durability": 2,
    "c9_replaceable": "no",
    "leak": false,
    "c5_contract": null,
    "c10_actionable": null,
    "verdict": "rewrite_shorter",
    "rewrite": "# _cache is written by the refresher thread; hold _cache_lock",
    "rename_suggestion": null
  },
  "claims": [
    {"text": "the refresher thread writes _cache concurrently", "status": "confirmed"}
  ],
  "labeler": {"type": "human", "model": null, "confidence": "high", "blind": true},
  "why": "thread-safety hazard not visible at the read site"
}
```

Field rules:

- `comment_kind`: `inline`, `api_doc` (docstring, JSDoc, rustdoc, Go-style doc), `todo`
  (TODO/FIXME/HACK/XXX), `directive` (linter and tool pragmas; kept structural, included only
  for taxonomy coverage).
- `placement`: `before_statement`, `trailing`, `function_head`, `class_head`, `module_head`.
- `annotated_code`: the statement, definition, or block the comment attaches to, with the
  comment itself removed. For a doc comment, the signature plus the first 10 body lines.
- `enclosing_scope`: innermost function or class containing the item, comment removed, capped
  at 80 lines centred on the item; module level gets 40 lines either side.
- `labels` values follow the schema in `docs/reduce-redesign.md` (Proposal 1). `c5_contract`
  is null unless `api_doc`; `c10_actionable` is null unless `todo`. `verdict` is under the
  strict preset. `rewrite` is required when verdict is `rewrite_shorter` or `fix_wrong_claim`.
- `claims`: every factual claim the comment makes, each `confirmed`, `contradicted`, or
  `unverifiable` against the provided code. Empty for pure narration.
- `labeler.blind` is true when the labeller saw no model suggestion.

### 1.2 Sources and sampling

Target 300 items: 120 from the maintainer's own repositories, 180 from agent-authored public
PRs. Stratify the whole set roughly as: Python 40%, TS/JS 35%, Rust 15%, YAML 10%;
`inline` 55%, `api_doc` 25%, `todo` 10%, other 10%. Over-sample likely keeps so the keep class
is not tiny: pull at least 60 candidates whose text contains hazard markers (`must`, `before`,
`after`, `never`, `not thread`, `race`, `ms`, `seconds`, `bytes`, `because`, `workaround`,
`quirk`, `bug in`). Cap at 3 items per file and 15 per repository.

**Own repositories.** The maintainer names them. For each, at HEAD: enumerate comments with the
crate's parser (add `reduce --dump-items FILE` to write every non-structural item with byte
range, kind and text as JSON; it is a 30-line addition to `main.rs` and `lib.rs` and is the
only crate change in this phase). Then a Python tool computes the context fields with
py-tree-sitter (the same grammars the crate uses) and git blame.

**Agent-authored PRs.** Use the AIDev dataset (`hao-li/AIDev` on Hugging Face, CC BY 4.0,
932k agent PRs from Codex, Devin, Copilot, Cursor, Claude Code). Inspect its tables first; the
expected path is: filter merged PRs in Python/TS/JS/Rust repos with a permissive licence, whose
diff adds at least three comment lines; take the PR's head commit; fetch the changed files at
that commit from GitHub (raw content, a few hundred requests); run the same extractor. Record
`source.pr` and `change.authored_by_agent = true` with the agent name. Prefer repositories
with more than 50 stars so the code is not a toy, and spread across at least 40 repositories.

### 1.3 Tooling to write

Under `tools/dataset/v2/`:

- `extract.py REPO_DIR --items items.json --out candidates.jsonl`: context fields, symbol
  info, blame ages, agent authorship from trailers (`Co-Authored-By: Claude`, `Cursor`,
  `Copilot`, `Devin`, `Codex`, `openai`). Deterministic, unit-tested on three small fixture
  repos checked into `tools/dataset/v2/fixtures/`.
- `mine_aidev.py --n 400 --out candidates-aidev.jsonl`: the PR sampler and fetcher. Cache
  everything under `tools/dataset/v2/cache/` (gitignored).
- `sample.py candidates*.jsonl --out gold-unlabelled.jsonl --n 300`: stratified sampling with
  the quotas above, deterministic seed, plus a `hard.jsonl` of 30 extra items where two
  cheap heuristics disagree (high lexical overlap but contains a hazard marker, for example).
- `label.py gold-unlabelled.jsonl`: terminal labeller. Shows the comment, annotated code, and
  scope with syntax highlighting (plain text is fine), asks the 13 questions with single-key
  answers in a fixed order (kind, claims, g1, hazard, c2, c3, c4, c8, c9, leak, c5/c10 when
  applicable, verdict, rewrite), writes each item on completion, resumes where it stopped,
  supports `s` to skip and `b` to go back one. Target under 90 seconds per item.
  `--prefill silver.jsonl` shows a model's proposed labels per question and accepts them with
  Enter; record `labeler.blind = false` for those.
- `silver.py gold-unlabelled.jsonl --model <frontier> --out silver.jsonl`: one frontier call
  per item with the Step 6 checklist from the rubric (list claims and check each; propose a
  rename; score C1-C10 with one-sentence justifications; output the labels object). Used both
  for prefill and as the fifth judge in the bake-off, so keep its prompt in one file.
- `check.py gold.jsonl`: schema validation, quota report, label distribution, and the
  dev/test split listing (`int(sha1(id), 16) % 5 < 2` is dev).

### 1.4 The labelling session

Protocol for the maintainer, written into the top of `label.py --help`:

1. Blind-label the first 100 items (no prefill). This calibrates the strict preset against
   real code and gives a human-versus-model agreement number later.
2. Label the remaining 200 with prefill. Change anything you disagree with; the tool records
   which fields were edited.
3. Take the 30 `hard.jsonl` items blind as well.
4. Re-label 30 random items from step 1 a day later. Report self-agreement per question; any
   question below 80% self-agreement gets its definition tightened in `SCHEMA.md` before the
   bake-off.

Labelling guidance, also in `SCHEMA.md`: label the comment as found, not as you would have
written it. `hazard` means a competent first-time reader of the annotated code would be
surprised or hurt without it. `c9_replaceable = fully` means you can name the identifier that
would replace it; write it in `rename_suggestion`. Under strict, `verdict = keep` only when
the comment is already terse; a kept hazard in a long block is `rewrite_shorter` with the
rewrite written out. `fix_wrong_claim` requires at least one `contradicted` claim.

Expected time: about 5 hours of maintainer time across two or three sittings.

## Step 2: the bake-off

### 2.1 Judges

All five receive the same `state` string rendered by `tools/bakeoff/render.py` in this order:
language and kind line, comment, annotated code, symbol, evidence (only what Phase 1 has:
agent authorship, ages), enclosing scope. Cap the whole state at 3,000 tokens by trimming the
scope first.

| id | Judge | How |
|---|---|---|
| `flash` | Clef-Flash, zero-shot | `/v1/systemone` on oMLX (or Workers AI), schema `strict-v0.json` |
| `clef27` | Clef 27B, zero-shot | same, if the maintainer's machine fits it; else Workers AI |
| `gemma-asis` | current pipeline, unchanged | convert gold to v1 rows (`comment`, `context` = first line of `annotated_code`) and run `cargo run -- reduce --scope comments --eval`; parse the `ok`/`MISS` lines; docstrings through the docstring eval |
| `gemma-state` | current model, full state | the current system prompt and demos, but the user message carries the rendered state instead of the 80-char line |
| `frontier` | frontier generative model with the Step 6 checklist | `silver.py`'s prompt; outputs the labels object |

`strict-v0.json` is the 13-question schema from the proposal, transcribed verbatim into the
System One request format. Write it once in `tools/bakeoff/schema/strict-v0.json`; Phase 3
embeds the same file.

Adapters live in `tools/bakeoff/judges/`, one file each, all returning the same answer shape:
per question either a probability vector (decision models) or a one-hot (generative judges).
Record raw responses to `tools/bakeoff/runs/<judge>/<id>.json` so nothing is re-queried.

### 2.2 Policy and thresholds

`tools/bakeoff/policy.py` implements the policy from Proposal 1 as a pure function of the
answer vector with named thresholds. `tune.py` grid-searches thresholds on the dev split to
minimise a cost: 10 × hazards-lost + 3 × errors-introduced + 1 × wrong-delete + 1 × churn +
0.3 × wrong-keep. Generative judges have no probabilities, so their verdict is used directly
and they skip tuning. Report everything on the test split only.

### 2.3 Metrics

`score.py` prints, per judge, per language, and per `comment_kind`:

- Verdict macro-F1 over the five classes, plus the confusion matrix.
- Hazards-lost: gold `hazard = true` and gold verdict in {keep, rewrite_shorter}, system verdict
  `delete` or `rename_instead`. Report as a count and a rate; this is the headline number.
- Wrong-delete: gold keep or rewrite, system delete, hazard false.
- Churn: gold keep, system anything else.
- Review rate: how often the policy abstained (decision models only), and the accuracy of the
  non-abstained remainder.
- Per-question accuracy and Brier score for decision models; accuracy only for generative ones.
- Agreement with the human on the 100 blind items versus the 200 prefilled items, to see how
  much the prefill anchored the labels.
- Median and p95 latency per item, and tokens per item.

Errors-introduced needs rewrites: for `gemma-asis`, `gemma-state` and `frontier`, collect every
rewrite the judge produced, have `frontier` score `g1_candidate`, `preserves` and `adds` for
each against the gold claims, and have the maintainer spot-check 30. Decision models produce no
rewrites in this phase, so they are scored on verdicts only; note this asymmetry in the report.

### 2.4 Robustness checks (decision models only)

Run `flash` twice more on the test split: once with every question instruction paraphrased
(write `strict-v0-paraphrase.json` by hand), once with one unrelated question appended
(`"Is the code written in a compiled language?"`). Report the verdict flip rate for each.
Above 3% means the schema needs stabilising before Phase 3.

### 2.5 Decision

Write `docs/decisions/judge-bakeoff.md` with the tables above and one of these outcomes:

- A decision model matches or beats `gemma-asis` on hazards-lost and beats it on macro-F1:
  proceed to Phase 2 and 3 as planned, with that model as the default judge.
- `gemma-state` beats `gemma-asis` but decision models do not beat `gemma-state`: the input was
  the problem. Proceed to Phase 2; in Phase 3 make `generative` the interim judge over the new
  state and move the fine-tune forward.
- `frontier` dominates everything by a wide margin and the decision models are far behind:
  Phase 3 uses the frontier model as the escalation tier from day one, and the fine-tune
  becomes the priority for a local judge.

Whatever the outcome, the gold set and the extractor are kept; they are Phase 2's test suite.

## Order of work

1. `--dump-items` in the crate (small PR, with the usual fmt/clippy/test).
2. `extract.py` with fixtures, then run on the maintainer's repos.
3. `mine_aidev.py`, cache, run.
4. `sample.py`, hand `gold-unlabelled.jsonl` and `hard.jsonl` to the maintainer.
5. While they label: `render.py`, `schema/strict-v0.json`, the five adapters against a mock
   `/v1/systemone` (a tiny Flask or `http.server` stub that returns uniform probabilities),
   `policy.py`, `tune.py`, `score.py`, and `silver.py`.
6. After labelling: `check.py`, run `silver.py`, then the bake-off, then the decision memo.
