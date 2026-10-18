# PLAN: executing the `reduce` redesign

Audience: the agent (or person) working on this repo. The reasoning behind each phase is in
`docs/reduce-redesign.md`; this file is the execution order, the deliverables, and the exit
criteria. `docs/NEXT.md` expands Phase 1 into tasks. Update this file when a phase closes;
record decisions in the log at the bottom.

The property that outranks everything in this plan is unchanged from DESIGN.md: **never corrupt
the target codebase.** Every phase below changes what is judged and how; none may weaken the
parse-validate-atomic-write path or touch anything that is not a comment or docstring.

## Phases

| # | Phase | Deliverable | Exit criterion |
|---|---|---|---|
| 0 | Research and proposals | `docs/reduce-redesign.md` | Done 2026-10-18 |
| 1 | Gold set and bake-off | `tools/dataset/v2/gold.jsonl` (300 items, human-labelled), `tools/bakeoff/`, `docs/decisions/judge-bakeoff.md` | A judge architecture is chosen on measured harm rates, not on preference |
| 2 | State and evidence extraction in Rust | `context` and `evidence` modules, `--diagnose` output, SQLite `states` table, per-language fixtures | Rust extractor reproduces the Python mining tool's fields on all 300 gold items |
| 3 | Judge backend, policy, verified rewrite | `judge.rs` (System One client), `src/schema/strict-v1.json`, `policy.rs`, eval v2 | On gold test split: hazards-lost ≤ current pipeline, errors-introduced = 0 in a 50-item spot check, verdict macro-F1 above current |
| 4 | Changeset mode | `reduce --changed`, `check` for all languages, findings JSON + SARIF, hook upgrade | Runs report-only on 30 real agent-authored PRs with no false deletes in tier 2 |
| 5 | Data flywheel and fine-tune | 3-5k labelled rows, mutation and rename generators, ms-swift LoRA run, encoder control, `docs/decisions/finetune.md` | Fine-tuned judge beats zero-shot on the held-out gold set, or the decision is recorded that the encoder wins |

Phases 1-3 are sequential. Phase 4 can start as soon as Phase 2 lands, because report-only
changeset mode is useful with evidence alone. Phase 5 starts only when Phases 3 and 4 are in
normal use, because that use is where the training rows come from.

## Phase 1: gold set and bake-off

See `docs/NEXT.md`. Summary: 300 real comments with full context, half from the maintainer's own
repositories and half from agent-authored public PRs, labelled by the maintainer against the
13-question schema. Then every candidate judge scores the same rendered state and is ranked by
hazards-lost, errors-introduced, verdict F1, latency. Candidates: Clef-Flash (oMLX), Clef 27B,
the current Gemma pipeline as is, Gemma given the full state, one frontier model running the
rubric's Step 6 checklist.

Nothing in the crate changes in this phase except, if useful, a `--dump-items` flag that writes
parsed items as JSON so the Python tools do not have to re-implement comment grouping.

## Phase 2: state and evidence extraction in Rust

Goal: every item carries the state record defined in NEXT.md before any judge sees it.

- `src/context.rs`: for each `Item`, compute `comment_kind` (inline, api_doc, todo, directive),
  `placement`, `annotated_code` (the statement, definition, or block the comment attaches to),
  `enclosing_scope` (function or class, comment removed, capped at 80 lines around the item),
  and `symbol` (name, public, is_test). Tree-sitter for all five languages. Python already has
  `hook_git::context_for_range`; generalise rather than duplicate.
- `src/evidence.rs`: identifier-shaped tokens in the comment resolved against scope, file and
  imports; path and dotted names checked against the repo; numeric and string literals compared
  with `annotated_code`; token overlap after identifier splitting; signature echo for doc
  comments; `git blame` ages for comment lines and annotated lines; agent authorship from
  commit trailers and committer; TODO owner and tracker regex. Everything optional: a missing
  `.git` or a shallow clone degrades to nulls, never to an error.
- `state.rs`: a `states` table keyed by item id holding the rendered state and the evidence
  JSON. `--diagnose` prints them redacted as today.
- Tests: one fixture file per language under `tests/fixtures/context/` with expected JSON. Plus
  the Phase 1 gold set as an integration check: the Rust output must match the Python mining
  tool's `annotated_code` and `enclosing_scope` on all 300 items, byte for byte after trimming.

Keep the existing MiniLM screener untouched in this phase.

## Phase 3: judge backend, policy, verified rewrite

- `src/judge.rs`: `trait Judge { fn judge(&self, state: &str, schema: &Schema) -> Result<Answers> }`
  with one implementation, `SystemOne` (`POST /v1/systemone`, `{model, state, questions}`,
  answers keyed by question id). Check `usage.input_tokens` against the local token estimate
  and warn on silent truncation. Concurrency through the existing `parallel`.
- `src/schema/strict-v1.json` (and `rubric-v1.json`): the questions, instructions and criteria
  from `docs/reduce-redesign.md`, embedded with `include_str!` and part of the LLM fingerprint
  so a schema edit invalidates cached verdicts.
- `src/policy.rs`: pure function from `Answers` plus evidence to `Verdict` (`Keep`,
  `RewriteShorter`, `Delete`, `RenameInstead`, `Fix`, `Review`) with thresholds loaded from the
  schema file, tuned in Phase 1 on the dev split. Unit-tested on recorded answer vectors.
- Rewrite path: the generative client stays for `RewriteShorter` and `Fix`. Every candidate is
  re-judged with the four verification questions before splicing. Failure keeps the original.
- Escalation: `escalation_model` config key; on a near-threshold or inconsistent answer,
  re-judge once with it, then `Review` if still inconsistent.
- Ledger: store the full answer vector per item. Progress and summary lines gain a `review`
  count.
- Config keys: `judge` (`systemone` | `generative`), `judge_endpoint`, `judge_model`,
  `escalation_model`, `policy` (`strict` | `rubric`). The old prompt path stays selectable as
  `judge = "generative"` until Phase 3 exit, then is deleted along with `DEMOS` and
  `DOC_DEMOS`.
- Eval v2: `reduce --eval tools/dataset/v2/gold.jsonl` reports per-question accuracy and Brier,
  verdict macro-F1, hazards-lost, churn, and latency. The v1 `--eval` is removed with the old
  path.

## Phase 4: changeset mode

- `reduce --changed <base>..<head>` and `check` (all languages) classify items into the three
  tiers from the proposal: added or modified (full schema, strict), unchanged with changed
  scope (G1, unverifiable, durability, evidence only), untouched (nothing).
- `state.change` with before and after blocks for tier 2; `authored_by_agent` from the commits
  in range.
- Output: in-place edits for `reduce`; for `check`, findings as JSON and SARIF with verdict,
  the probabilities that drove it, and the evidence line. Exit code policy as today.
- `install-git-hook` upgrades the wrapper; the hook stays warning-only by default.
- Validation: 30 real agent-authored PRs (from the Phase 1 AIDev sample, different repos),
  run report-only, every tier 2 finding checked by hand. Any false "stale" finding that would
  have deleted is a blocker.

## Phase 5: data flywheel and fine-tune

- Collection: every `reduce` and `check` run can write its state records and answers to a
  local export (`--export-dataset FILE`, off by default). Maintainer-reviewed rows join the
  training set.
- Generators under `tools/dataset/gen/`: `mutate.py` (rename, literal change, condition
  inversion, argument swap under an accurate comment, labelled contradicting) and `rename.py`
  (apply a proposed rename, label C9 by whether the comment survives).
- Donations: `docs/HOW-TO-HELP.md` defines the contribution format; contributions land under
  `tools/dataset/contrib/<donor>/` with their own README and licence line.
- Training: ms-swift Clef LoRA (`--task_type decision --loss_type smooth_brier`), rank 8-64,
  criteria paraphrased on a fraction of rows, one to three epochs, on a rented GPU. Control: a
  ModernBERT-large multi-head classifier on identical rows. Both evaluated on the Phase 1 gold
  test split, which is never trained on.
- Decision recorded in `docs/decisions/finetune.md`: which model ships as the default judge and
  which as the screener.

## Cross-cutting rules

- Dataset format v2 (NEXT.md) is the one format for gold, mined, generated and donated rows.
  Split by id hash as the current README does; thresholds are tuned on dev, reported on test.
- DESIGN.md is updated at the end of each phase to describe what exists, not what is planned.
  AGENTS.md's eval instruction changes when eval v2 lands.
- Dependencies: Phase 2 needs no new crates. Phase 3 needs none (minreq and serde_json
  suffice for System One). Phase 4 may need a SARIF serialiser; write the handful of fields by
  hand rather than add a crate.
- Small diffs, one phase deliverable per PR where possible.

## Risks

| Risk | Signal | Response |
|---|---|---|
| Zero-shot decision models do not beat the current pipeline with context | Phase 1 bake-off | Ship Phases 2 and 4 anyway (they help any judge); use Gemma-with-state as the interim judge; move the fine-tune earlier |
| Flash is weak on G1 | Per-question accuracy on `g1_contradicts` | Route G1 to evidence plus escalation; or default to the 27B for api_doc items only |
| Local latency unacceptable for whole-tree runs | Phase 3 timing on a 5k-item repo | Changeset mode first; screener in front; Workers AI as an opt-in backend |
| Schema drift silently changes verdicts | Any schema edit | Schema text in the fingerprint; re-run eval v2 and record the delta in the PR |
| Labelling time | Phase 1 slips | Blind-label 100, prefilled for the rest (NEXT.md); do not reduce the set below 200 |
| Rewrites introduce errors | 50-item spot check in Phase 3 | Raise `preserves`/`adds` thresholds; if still non-zero, disable rewrite in strict mode and delete or keep only |

## Decision log

- 2026-10-18: Proposals accepted as the plan of record (this document). Phase order: gold set
  and bake-off before any crate change.
