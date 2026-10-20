# Judge bake-off (Phase 1 decision)

Date: 2026-10-19.

## Decision

**A decision model with the deterministic policy becomes the judge.** Zero-shot Clef-Flash
already beats the current Gemma pipeline on both headline numbers, and a fine-tuned Clef-Flash
loses no hazards on the test split. This is the first outcome in `docs/NEXT.md` 2.5: proceed
with Phases 2 and 3 as planned, with a decision model as the default judge.

- **Shipped default:** zero-shot Clef-Flash (a public model) with the strict thresholds tuned on
  the dev split. They are now in `src/schema/thresholds-strict-v1.json`.
- **Best local judge:** Clef-Flash fine-tuned on the private training split. It cannot be
  published, because it was trained on data we may use but not distribute. It runs through
  `judge_thresholds` and a local `/v1/systemone` server (`tools/finetune/serve.py`).
- **Frontier escalation:** a frontier generative model is far ahead on macro-F1 (0.665 vs
  0.43). It is the right escalation tier, as `docs/PLAN.md` Phase 3 already provides.

## How it was measured

- **Gold set.** 1,201 real comments from one production web application (Python, TypeScript,
  JavaScript and YAML; no Rust, no TSX), donated for training and evaluation only. The data is
  private and is not in this repository.
  - Each item was labelled blind by Claude Fable 5.1 against the 13-question strict schema.
  - Fable's labels were compared with two independent earlier labelling rounds. 348 items
    where Fable disagreed with both rounds on verdict, hazard or contradiction, or where it had
    low confidence, were adjudicated by Fable with all opinions visible.
  - No person has reviewed the labels. Every number below measures agreement with an agent
    labeller, not with a human.
- **Split by id hash** (`tools/dataset/v2/split.py`): 599 train, 224 dev, 378 test.
  Thresholds are tuned on dev; everything below is on test.
- **Same input for every judge.** Each judge gets the same rendered state
  (`tools/bakeoff/render.py`, ported byte-for-byte to `src/render.rs`).
- **Structural items are not judged.** TODOs and directives are kept by the structural rules
  for every judge, as the tool does. For the current pipeline, JSDoc blocks are also kept,
  because the tool never sends them to the model.
- **Cost function.** 10 × hazards lost + 3 × errors introduced + wrong deletes + churn +
  0.3 × wrong keeps + 0.5 per review.

## Results on the test split (378 items, 72 gold hazards)

| Judge | Macro-F1 | Hazards lost | Wrong deletes | Churn | Cost | Median latency |
|---|---|---|---|---|---|---|
| Current pipeline (Gemma 4 E2B; JSDoc kept) | 0.314 | 22 (31%) | 24 | 14 | 280.2 | n/a |
| Gemma 4 E2B given the full state | 0.308 | 25 (35%) | 28 | 8 | 308.2 | n/a |
| Clef-Flash zero-shot + policy | 0.391 | 2 (3%) | 4 | 18 | 71.1 | 1.9 s (3090, bf16) |
| Clef 27B zero-shot + policy | 0.358 | 1 (1%) | 3 | 20 | 57.3 | about 15 s (Mac, 4-bit) |
| ModernBERT-large, fine-tuned (control) | 0.373 | 1 (1%) | 5 | 14 | 55.1 | 24 ms |
| **Clef-Flash fine-tuned + policy** | **0.429** | **0 (0%)** | **2** | 16 | **41.4** | 0.74 s |
| Frontier (Claude Sonnet, rubric checklist) | 0.665 | 1 (1%) | 2 | 21 | 49.2 | n/a |

All numbers are measured. The Rust pipeline (`reduce --eval` v2) reproduces the fine-tuned
row exactly: all 378 verdicts and answer probabilities match the Python harness.

### What the numbers say

- **The current pipeline loses almost a third of the hazards** (22 of 72). That is the main
  thing this redesign exists to fix, and any decision model with the policy fixes most of it.
- **The input was not Gemma's problem.** Gemma given the full state does no better than
  Gemma given one line of context. The small generative model is the limit.
- **Fine-tuning helps most on the hard questions.** Per-question accuracy rose from zero-shot to
  fine-tuned: kind 0.46 to 0.71, unverifiable claims 0.57 to 0.91, information gain 0.45 to
  0.74, rationale 0.54 to 0.82, contract quality 0.40 to 0.64. Hazard went from 0.82 to 0.91.
  Contradiction stayed about 0.95-0.97 throughout.
- **The policy now limits the fine-tuned model.** Its own holistic verdict answer is 79%
  accurate (macro-F1 0.51), but it loses 2 hazards. The policy is safer on the cost function
  (41.4 vs 55.2). Keep against rewrite-shorter, and doc-comment verdicts, are where the policy
  is weakest.
- **The 27B does not earn its cost zero-shot.** Clef 27B (macro-F1 0.358, 1 hazard lost) is no
  better than zero-shot Flash on macro-F1 and is far slower locally. It stays an option as an
  escalation tier, but the fine-tuned Flash is the better local judge.
- **The encoder control is cheap but weaker.** ModernBERT is 30 times faster than fine-tuned
  Flash, but its test macro-F1 (0.373) is below even zero-shot Flash, and it lost 2 hazards on
  dev. It remains a candidate screener, not a judge.

### Robustness (fine-tuned Clef-Flash, test split)

| Change to the schema | Verdict flips |
|---|---|
| Every instruction and criterion paraphrased | 12 of 378 (3.2%) |
| One unrelated question appended | 1 of 378 (0.3%) |

The paraphrase flip rate is just above the 3% bar in `docs/NEXT.md` 2.4, even though 30% of
the training rows used the paraphrased schema. Freeze the schema wording before the next
fine-tune, and treat any wording change as a new schema version.

## Caveats

- **The labels come from an agent.** Agreement with Fable is what is measured. Fable agreed
  with the earlier independent labelling rounds on 71-72% of verdicts and 87-88% of hazards.
- **Noise.** One hazard is 10 cost points. On dev, the fine-tuned model lost 1 hazard and
  zero-shot Flash lost none, so the fine-tune's hazard advantage is not proven.
- **One codebase, no Rust.** The private set is one web application. Rust and TSX are untested
  on real data.
- **The verified-rewrite path is not yet working.** The fine-tuned judge rejects every rewrite
  candidate, because it was never trained on the verification questions, and the legacy Gemma
  prompt mostly refuses to rewrite. Until that is fixed, rewrite verdicts behave as keep. Work
  on this is recorded in `docs/PLAN.md`.
- **Clef 27B** ran 4-bit on a Mac and was tuned on the dev split; it is roughly 20 times slower
  than fine-tuned Flash on the 3090.
