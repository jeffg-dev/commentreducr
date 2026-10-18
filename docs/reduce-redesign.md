# Rethinking `reduce`: proposals for comment judgement on a codebase and a changeset

Status: proposal, 2026-10-18. Not implemented. Read DESIGN.md first; this document argues for
replacing the "LLM verdict protocol" section of it and for adding a changeset mode.

The research survey this responds to (Judging Code Comments: Research Survey and a Scored
Rubric, hereafter "the rubric") reduces to four findings that matter for this tool:

1. A wrong comment is worse than no comment, for people and even more for LLM readers
   (CodeCrash: an average 23% drop from misleading comments, and telling the model to ignore
   comments does not help). Accuracy against the code is the first gate, not one criterion among
   many.
2. Quality is multi-dimensional and the verdict space is bigger than keep/delete:
   Keep, Improve, Rename/refactor instead, Fix, Delete. "Rename instead" (C9) and "Fix" (G1) have
   no counterpart in the tool today.
3. Inline comments and API/doc comments have different audiences and different weights. A doc
   comment is never penalised for existing; a signature echo scores zero.
4. Comments written by LLMs read well and are wrong roughly a fifth of the time (Kang et al.:
   18% of GPT-4 Javadoc). The reviewer must verify claims against the code, not grade fluency.

## What the current pipeline cannot do

The judgement the tool makes today is made by a 2B generative model that sees the comment's
prose, whitespace-collapsed, plus the first 80 characters of the next code line
(`llm::user_message`, `Item::reduce` in `src/lib.rs`). Everything below follows from that.

| Rubric requirement | Current pipeline |
|---|---|
| G1 / C1: check every claim against the code | Impossible. The judge never sees the code the comment describes, only one trimmed line. A docstring judge sees a 3-line body preview. |
| C2: information gain over names and types | Guessable from the one line, not checkable. "Next code line already converts (* 1000)" is the only form of this the prompt can apply. |
| C9: would a rename replace it | Impossible without the enclosing code. |
| C8: durability, staleness | No git history, no notion of whether the code moved under the comment. |
| Inline vs API weights | Comments and docstrings use separate prompts, but JSDoc, rustdoc and Go-style doc comments are simply structural (kept whole) or treated as inline. |
| Verdict space | Two outcomes: `DELETE` or one line of at most N words. No Fix, no Rename-instead, no "not sure, show a human". |
| Calibrated confidence | None. A class code is emitted at temperature 0; the tool cannot abstain. |
| Verifying the rewrite | None. The replacement line is spliced in if it parses. The rewrite is itself an LLM-written comment, and the research says those are wrong about one time in five. |
| Changeset awareness | `check` screens changed Python blocks with the Python-only classifier and warns. `reduce` has no notion of a diff. |
| Languages | The screener is Python-only; other languages go straight to the LLM. |
| Evaluation | 130 + 68 synthetic rows, exact-label agreement, no per-criterion signal, no calibration, no measurement of hazards lost or errors introduced. |

The tool is also built around a design constraint that no longer holds: a ~2k-token cached
prefix and a ~100-token comment, so that a 2B model answers in 0.4 s. The code context the
rubric needs is 500-3000 tokens per item, and it is different for every item.

## Why a decision model fits

Cloudflare's Clef (27B, Qwen3.8 backbone) and Clef-Flash (9B, Qwen3.5 backbone), released
2026-10-01 under Apache-2.0, take a `state` (string or JSON) plus up to 64 typed `questions` and
return a probability for every option of every question from one prefill-only forward pass. No
text is generated. The three question types are `noul` (true/false), `choice` (2-255 named
options) and `score` (2-10 ordered levels, returned as a probability-weighted expectation plus
the per-level distribution). The backbone is frozen; a joint schema head plus rank-256 LoRA
adapters were trained with label-smoothed cross-entropy, a Brier term for calibration, and an RL
objective that gives partial credit to adjacent `score` levels. The protocol is the Jev/System One
API (`POST /v1/systemone`), also served by SGLang, vLLM, llama.cpp (text-only Clef merged
2026-10), community MLX builds for Apple Silicon, and Workers AI.

That shape matches this problem unusually well:

- **The rubric is a schema.** Five gates and nine criteria become 12-16 questions, scored
  jointly in one call. With a generative model, scoring ten criteria means a long JSON reply,
  parse failures, and a model that is rewarded for fluency. With a decision model, the criteria
  text is input, scoring is output, and the per-criterion probabilities are the explanation.
- **Rubric anchors are `score` levels.** The rubric's 0-3 anchors for C2, C3, C8, C10 paste
  directly into `criteria` lists. The returned expectation is the rubric score.
- **The philosophy becomes data, not code.** The question wording carries the user's stance
  (strict: an inline comment survives only on a hazard, quirk, ordering or contract). Changing
  the stance is editing criteria text, and a fine-tune sharpens it rather than fighting it.
- **Calibrated probabilities allow abstention.** Delete above a threshold, keep below one, show a
  human in between. The binary verdict today has no middle.
- **No rewrite by the judge.** The judge cannot write a comment, which is a feature: the
  rewrite becomes an optional, separately verified step instead of the default output.
- **Context fits.** 64k tokens (16k default in the reference encoder) takes the whole enclosing
  function, the annotated statement, callers, and the diff hunk.
- **Fine-tuning is a real option.** Cloudflare's own fine-tuning is a design-partner RL service,
  but three community trainers already reproduce the recipe (LoRA on the backbone plus the joint
  head, label-smoothed CE + Brier) and the training record is exactly `{state, questions,
  labels}`, which is what the eval dataset already is in spirit.

Protocol facts that shape the design (from the shipped `joint_schema_model.py` and the cards):

- The prompt is a fixed chat template: system text, then `STATE:`, then `SCHEMA FIELDS:`
  with each question's id, type, instruction and allowed options. JSON states are rendered
  compact with sorted keys. The state is silently cut from the end to fit `max_length`
  (16,384 default in the reference encoder and vLLM; 24,576 hosted for Flash, 65,536 for
  Clef). So the state must be a string we lay out ourselves, most important fields first, and
  the enclosing scope last where truncation lands.
- `choice` options are sorted alphabetically before encoding, so option order cannot bias a
  `choice`. `score` levels are ordered by definition.
- The joint head cross-attends between questions, so answers are not independent: adding or
  rewording one question can move the others. The schema is therefore a versioned artifact,
  thresholds are tuned per schema version, and the schema text belongs in the SQLite
  fingerprint the way the prompt text is today.
- `choice` and `noul` forms of the same question return probabilities that differ by about
  0.05 (JEV-as-a-Judge, arXiv 2609.26550). Pick one type per question and keep it.

The honest caveats, also from the model cards and community write-ups:

- Every benchmark is vendor-run. None is on code comments. Clef-Flash is weak on exactly the
  shape we care most about: RAGTruth (does a text agree with its evidence) is 35.6 for Flash
  versus 79.4 for the 27B, and out-of-scope rejection on CLINC150 is 66.8 versus 97.4. Flash
  does score 86.1 on CRUXEval, so code reasoning is not hopeless, but the only independent study
  of a decision model as a judge of code (JEV-as-a-Judge) found it within 3 points of a frontier
  model when the verdict can be read off the text, and 13-21 points behind when it has to be
  derived. Zero-shot G1 (comment contradicts code) on the 9B should be assumed weak until
  measured. This pushes G1 toward the 27B, a fine-tune, or deterministic evidence (Proposal 2),
  and it is why the state should make facts readable rather than derivable.
- Scores depend on option wording and descriptions (a bare label and a described label routed
  the same input differently in llama.cpp's own demo), and the model cannot say "none of
  these" unless given the option. Paraphrasing a rubric changed 4 of 48 decisions in the same
  study.
- Local throughput is the practical risk, though smaller than it first looks: oMLX 0.7.1
  already serves decision models on `/v1/systemone`, and third-party measurements put 4-bit
  Flash at about 0.1 s for a short request and about 4 s for 20 questions over a 6k-token
  state on an M5-class machine. Numbers are in the cost section.
- The 27B needs about 32 GB at 4-bit; Flash runs on a 16 GB Mac (11 GB at 8-bit).

## Proposal 1: the rubric as a question schema, with a verified rewrite path

Replace `llm::summarize` and `llm::rewrite_docstring` with one judge call per item that returns
a probability vector, a deterministic policy that maps the vector to a verdict, and a separate,
verified generator used only when the verdict is Rewrite or Fix.

### State

One record per item. Everything in it is computed by the parser and git, not by the model. It is
shown here as JSON for clarity; in practice it is rendered as a labelled text block in this
order (comment, annotated code, symbol, evidence, change, enclosing scope) so that the
truncation-from-the-end behaviour drops the least important part first, and so that the
reference encoder's sorted-key rendering does not reorder it.

```json
{
  "language": "python",
  "comment_kind": "inline",            // inline | api_doc | todo | directive (deterministic)
  "placement": "before_statement",     // before_statement | trailing | function_head | class_head | module_head
  "comment": "# The lock must be acquired before touching _cache; the background\n# refresher thread writes to it concurrently ...",
  "annotated_code": "with self._cache_lock:\n    self._cache[key] = value",
  "enclosing_scope": "def set(self, key, value):\n    ...",   // comment removed; capped (see budget)
  "symbol": {"name": "Cache.set", "public": false, "is_test": false},
  "evidence": { ... },                 // Proposal 2
  "change": { ... }                    // Proposal 3; absent in whole-tree mode
}
```

### Questions

Instructions name state fields in backticks, which the community guidance says matters. The
list below is the strict preset; the criteria text is where the user's philosophy lives.

| id | type | instruction (abridged) | options / levels |
|---|---|---|---|
| `kind` | choice | What kind of comment is `comment`? | rationale, summary, contract, invariant, warning, alternatives_rejected, todo, directive, restating, commented_out_code, journal_or_history, banner, cross_reference_leak, change_narration |
| `g1_contradicts` | noul | Does any claim in `comment` contradict `annotated_code` or `enclosing_scope`? | |
| `g1_unverifiable` | noul | Does `comment` make a claim about behaviour that neither `annotated_code` nor `enclosing_scope` can confirm? | |
| `hazard` | noul | Does `comment` state a trap, danger, ordering requirement, unit mismatch, external quirk, or caller precondition that a competent reader of `annotated_code` would not see? | |
| `c2_info_gain` | score | How much does `comment` say that the names, types and code in `annotated_code` do not? | 0 restates the code; 1 paraphrases; 2 adds one fact the code cannot show; 3 adds facts a careful reader could not recover from the code |
| `c3_rationale` | score | Does `comment` explain why? | 0 none / sleep-a-bit; 1 vague; 2 specific; 3 specific plus the rejected alternative or consequence |
| `c4_level` | choice | At what level does `comment` sit relative to `annotated_code`? | same_level_narration, higher_summary, lower_precision, mental_model |
| `c8_durability` | score | How likely is `comment` to become wrong when nearby code changes? | 0 hard-codes volatile facts (line numbers, literal values, other files' internals); 1 tied to detail that changes often; 2 stable intent or constraint; 3 stable and machine-checked or linked to a durable record |
| `c9_replaceable` | choice | Could a rename, a type, an assertion, or an extracted function carry what `comment` says? | fully (a name would replace it), partly, no |
| `leak` | noul | Does `comment` explain this code by naming another module, file, caller, or sibling and what it expects or mirrors? | |
| `c5_contract` | score | (api_doc only) Could a caller use `symbol` correctly from `comment` alone? | 0 echoes the signature; 1 summary only; 2 inputs, outputs, errors; 3 also side effects, units, ownership, thread-safety |
| `c10_actionable` | score | (todo only) | 0 "TODO fix"; 1 problem only; 2 owner or tracker; 3 problem, owner, tracker, exit condition |
| `verdict` | choice | Under the strict policy, what should happen to `comment`? | keep, rewrite_shorter, delete, rename_instead, fix_wrong_claim |

The last question is deliberately redundant with the policy below. Disagreement between the
model's holistic verdict and the computed one is itself a signal: route those items to review.
It also gives a single label to fine-tune against when per-criterion labels are missing.

### Policy

Thresholds are set on the dev split, not guessed; these are starting points.

1. Deterministic first. `commented_out_code`, `banner`, `journal_or_history` by the existing
   `prose`/`structural` detectors, and G2 from Proposal 2, delete without asking the model.
   Structural comments stay structural.
2. `P(g1_contradicts) > 0.6` → **Fix**: run the generator; if verification fails, in strict mode
   delete (a wrong comment is worth less than none), otherwise report.
3. `comment_kind == api_doc`: compute the API-weighted quality from the rubric weights (C2 2,
   C5 3, C8 2, C9 0.5; C1 is the gate above, and clarity is handled by the length cap and the
   rewrite rather than scored). Below 0.30 or `E[c5] < 0.5` → delete; 0.30-0.55 → rewrite to
   the contract; otherwise keep. A doc comment is never deleted only because it is long.
4. `comment_kind == inline`:
   - `E[c2_info_gain] < 0.75` → **Delete**.
   - `P(c9_replaceable = fully) > 0.7` → **Rename instead** (in `reduce`, delete and report the
     suggested identifier; in review mode, a finding, not an edit).
   - strict preset: `P(hazard) < 0.5 and E[c3_rationale] < 1.5` → **Delete**.
   - `P(hazard) ≥ 0.5` or inline quality ≥ 0.55 → **Keep**, or **Rewrite shorter** when the
     block is longer than the configured cap (today's one-line rule).
   - `P(leak) > 0.6` with a hazard → rewrite in this code's own terms (today's D4 rule).
5. `todo`: `E[c10] < 1` → report as untracked debt; never delete (structural today, stays so).
6. Escalate, then abstain: if the deciding probability is within ±0.15 of its threshold, or
   the holistic `verdict` disagrees with the computed one, re-judge once with the escalation
   backend (Clef 27B, or a generative frontier model running the rubric's Step 6 checklist with
   the same state). If the two still disagree, mark the item `review` and leave it untouched.
   `reduce` prints these; the changeset mode (Proposal 3) annotates them. Confidence-gated
   cascades of this shape are the documented way to use decision models as judges: in
   JEV-as-a-Judge the cascade beat the frontier model alone at 41% of its cost.

### Verified rewrite

The generator (today's Gemma, or anything) writes the candidate. The judge then scores a second
state `{original, candidate, annotated_code, enclosing_scope}` with four questions:

- `preserves`: Does `candidate` keep every hazard, contract or precondition stated in `original`?
- `adds`: Does `candidate` assert anything that is in neither `original` nor the code?
- `g1_candidate`: Does `candidate` contradict the code?
- `shorter`: deterministic, not a question.

Accept only if `P(preserves) > 0.8`, `P(adds) < 0.2`, `P(g1_candidate) < 0.1`. On failure keep
the original untouched and report; never fall back to delete from a failed rewrite. This closes
the gap the research is loudest about: the tool must not be a source of fluent wrong comments.

### Presets

`policy = "strict"` (default; the user's stance from DESIGN.md) and `policy = "rubric"` (the
research default: inline comments with specific rationale survive even without a hazard). Both
are the same questions with different thresholds and a few different criteria sentences. The
difference between the two, on the same codebase, is a useful report in itself.

### Cost and latency

Per item: schema ≈ 1.0-1.3k tokens, state ≈ 0.5-2.5k. Call it 2.5k average, 1.5k after
capping `enclosing_scope` at 60 lines around the comment. The schema is a fixed prefix, so it
is cacheable in principle; llama.cpp lists shared-prefix support as a TODO and the others do not
document it, so assume full prefill for now.

| Backend | per item | 5,000 items | note |
|---|---|---|---|
| Workers AI, clef-flash ($0.038/M input, output free) | ~40 ms server, ~200-300 ms round trip | ~$0.50, ~10 min at 8 in flight | sends source to Cloudflare |
| Workers AI, clef 27B ($0.24/M) | ~210-300 ms | ~$3 | |
| Local 24 GB GPU, vLLM/SGLang FP8 Flash | ~0.1-0.3 s | ~15-40 min | `vllm serve Cloudflare/clef-flash --max-model-len 16384` |
| Apple Silicon, 4-bit Flash via oMLX or mlx-decision | ~0.5-2 s, prefill-bound | 1-3 h | measured: 0.25 s short, 3.8 s for 6k tokens on M5 Pro |
| Apple Silicon, llama.cpp `ggml-org/Clef-Flash-GGUF` | similar; one sequence per batch, text only | | raise `-ub` above the longest input |

Mitigations for the local path, in order: run changeset mode (tens of items, not thousands);
keep a cheap screener in front (the MiniLM for Python today; Proposal 4 retrains one for all
languages on the same labels); cap the state; batch. The 0.8-2B members of the vllm-sr d3
family (Apache-2.0, one forward pass per question with prefix caching, no joint head) are
candidate screeners worth one measurement; LiquidAI's d1 models are not, because their LFM
licence restricts commercial use.

### What stays

Parsing, structural rules, docstring safety rules, atomic rewrite, the SQLite ledger (which
should now store the full probability vector per item: it is the audit trail and, later,
training data), the resumable job model.

## Proposal 2: deterministic evidence before the model reads anything

The rubric's own advice for an automated judge is to prefer evidence over reading, because
reading-only detectors failed to separate accurate from inaccurate comments (Kang et al., nine
detectors; DocPrism's reproduction of C4RLLaMA at 0.05-0.14 precision). Most of the strongest
signals are computable with tree-sitter and git, and a decision model reads structured JSON
well. Put them in `state.evidence` and, where precision is high enough, act on them directly.

| Evidence | How | Feeds | Act directly? |
|---|---|---|---|
| Mentioned identifiers that do not resolve | Extract identifier-shaped tokens from the comment (backticked, snake/camel case, dotted, `f()`); resolve against the enclosing scope, file, and imports via tree-sitter | G1/C1, C8 | Report as stale; strong prior for Fix |
| Mentioned files, modules, classes elsewhere | Path-shaped and dotted names; check existence in the repo | D4 leak, C7 | Report; model decides leak vs contract |
| Literal mismatch | Numbers, units, string literals in the comment vs literals in `annotated_code` ("300ms" above `debounce(500)`) | G1, C8 | Report; strong prior for Fix |
| Lexical overlap | Token overlap between comment and `annotated_code` after splitting identifiers (Steidl's coherence) | C2, G5 | Overlap ≥ 0.8 on a one-statement comment → restating, delete |
| Signature echo | Doc comment whose content tokens are a subset of the signature's | G5 | Delete in strict preset |
| Comment age vs code age | `git blame` the comment lines and the annotated lines; `code_newer_than_comment_days` | C8, G1 | Prior only |
| Authored by an agent | Commit trailers and committers (`Co-Authored-By: Claude`, Cursor, Copilot, Devin, Codex), or a commit that adds > N comment lines at once | strictness | Raise thresholds for these items; they are the reason the tool exists |
| TODO without tracker or owner | Regex | C10 | Report |
| Density proportionality | Comment lines per statement in the file vs repo median | file-level prior | Report only |

These cost nothing per item, work for every language the parser handles, and make the G1
decision partly verifiable instead of entirely inferred. They also give the `check` hook
something precise to say ("mentions `retry_count`, which no longer exists in `fetch()`") without
any model at all.

## Proposal 3: judge the changeset, not just the tree

The problem statement is a changeset problem: an agent drafts code with narration comments,
and the comments arrive in a PR. Whole-tree cleanup is the fallback; prevention at review time is
where the value is. The research adds a second reason: cleanup PRs are poorly received (27% of
Jabrayilzade et al.'s smell-removal PRs were accepted), and edits to control flow are the changes
most likely to leave comments stale (Wen et al.).

Add `reduce --changed <base>..<head>` (and make `check` use it) with three tiers of item:

| Tier | Items | Judge with | Default action |
|---|---|---|---|
| 1 | Comments added or modified in the diff | Full schema, strict thresholds, `evidence.authored_by_agent` set from the commits | Edit (reduce) or annotate (check / CI) |
| 2 | Comments unchanged whose `annotated_code` or `enclosing_scope` changed | G1, `g1_unverifiable`, C8 only, plus Proposal 2 evidence | Report as possibly stale; edit only on a confirmed contradiction |
| 3 | Everything else | Nothing | Untouched. Never churn a PR with unrelated comment edits. |

`state.change` carries `comment_new`, `comment_modified`, `scope_changed`, and for tier 2 the
annotated code before and after the change as two whole blocks rather than a unified diff: the
one published experiment with a decision model on code review found before/after blocks
roughly doubled the model's detection of a planted bug compared with a raw hunk.

A new taxonomy option, `change_narration`, catches the characteristic AI comment that explains
the edit rather than the code ("Changed to use a dict here because the previous list approach
was O(n)"). That text belongs in the commit message; the policy deletes it. The best-documented practitioner rubric for agent-written
comments (a CLAUDE.md plus cleanup skill circulating as a gist) uses the same ordered checks:
narration, change narration, moving-target references ("the spec"), bloated why, stale, keep.
Prompting the agent not to write these comments is reported to be unreliable (Claude Code issue
65961), which is the case for a post-hoc check on the changeset.

Outputs: the existing in-place edit for `reduce`; for `check` and CI, a findings file (JSON, and
SARIF so GitHub shows it inline) with the verdict, the probabilities that drove it, and the
evidence line. A review comment that says "info gain 0.3, replaceable by a name
(`is_eligible_for_full_benefits`), delete or rename" is actionable in a way that a warning
count is not. A per-PR summary line, comment lines added per code line added, is a cheap
leading indicator of the drafting habit.

This mode is also the cheapest way to run the local Clef path: a PR has tens of comments.

## Proposal 4: the data flywheel, the fine-tune, and an evaluation that measures harm

Zero-shot Clef-Flash with Proposals 1-3 is the first milestone and must be measured before any
training. But the eval set today is 198 synthetic rows labelled with a single string, and the
model families above improve most with a few thousand in-domain labels. The research gives a
labelling procedure (rubric Step 6) that produces exactly the multi-question labels the schema
needs.

### Sources

1. **Mined, in-distribution.** The AIDev dataset (CC BY 4.0, arXiv 2602.09185) holds 932k
   agent-authored pull requests from Codex, Devin, Copilot, Cursor and Claude Code, with commit
   details and review comments. Extract every comment those PRs added, in Python, TS/JS and
   Rust, with the full state of Proposal 1 and the change record of Proposal 3. This is the
   actual target distribution, which the synthetic set is not, and it needs no trailer
   heuristics. Keep repository, commit and path so rows can be re-derived. Nothing public labels
   comments on this verdict scale, and nothing labelled exists for Rust, YAML, or JS/TS at all,
   so this mining step is not optional.
2. **Contrastive mutation for G1.** Take comments that are accurate, mutate the annotated code
   mechanically (rename an identifier the comment mentions, change a literal, invert a
   condition, swap argument order), and label the pair as contradicting. Verified by
   construction, cheap, and the only way to get enough positives for the gate that matters most.
3. **The rename test for C9.** Have a strong model propose a rename or extraction; apply it
   mechanically; ask whether the comment is now redundant. The answer labels C9.
4. **Public datasets** as auxiliary data for specific questions, not as the main set. For G1:
   CCIBench (22,360 Java pairs with old and new method; it is the cleaned version of the
   Panthaplackel et al. JITData, whose "comment edited in the same commit" proxy label is noisy)
   and CoDocBench (4,573 Python docstring/code change pairs with diffs and commit messages,
   unlabelled, the only Python history set). For restating and obviousness: the Jabrayilzade et
   al. smells replication (2,211 Java/Python comments with a code snippet; the "obvious" and
   "beautification" labels) and FIRE IRSE (about 11k C and 10k Java comment/code pairs labelled
   useful or not useful, where not useful means the concepts are already in the code). The NLBSE
   class-comment sets carry type labels but no code. None of these carries this rubric, and all
   are Java-heavy, so they help the `kind` and G1 heads only.
5. The existing 198 rows, re-labelled to the schema, and the user's own repositories, which are
   the ground truth for the strict preset.

### Labelling

Two or three strong models fill the whole schema independently, each required to list the
claims the comment makes and whether the code confirms, contradicts or cannot confirm each one
(rubric Step 6). Their agreement becomes a soft label: the per-option distribution, which is what
Clef is trained against (its reported metrics are KL-from-gold and Brier, and the LocalLLaMA
typed-decisions format stores gold as distributions). Disagreements and a stratified 300-500
row gold set are adjudicated by hand, by the user, since the strict preset is a stance. Never
tune on the gold set.

Target size: 3-5k rows for the fine-tune, balanced across language, `comment_kind`, and verdict,
with at least 500 G1 positives from mutation. Community LoRA runs on Clef-Flash moved domain
accuracy from ~75-80% to ~95% with sets of that order, though those numbers are self-reported
and on easier tasks.

### Fine-tune recipe

Clef-Flash, LoRA rank 8-64 on the backbone's linear projections plus the joint head in fp32,
label-smoothed cross-entropy with a Brier term (ms-swift ships this as `--model_type clef
--task_type decision --loss_type smooth_brier --tuner_type lora`; MersivMedia's clef-finetune is
the same recipe but has not been run on real weights), one to three epochs, criteria wording
paraphrased for a fraction of rows so the head does not overfit one phrasing. 24 GB with QLoRA
or one rented H100 for an hour or two; the closest published analogue (LLM-as-Jev, arXiv
2610.02076) improved calibration with rank-16 LoRA and a few hundred gradient steps. Export
merged weights plus `joint_head.safetensors`; serve with the same `/v1/systemone` endpoint. The
fine-tuned model is only usable where the serving stack loads the custom head, which today
means transformers, vLLM, SGLang and the MLX loaders; a re-quantised GGUF is extra work. Compare against four
baselines on the same gold set: zero-shot Clef-Flash, zero-shot Clef 27B, the current Gemma
pipeline, and a ModernBERT-large encoder with one classification head per question trained on
the same labels. The encoder is the honest control: on a narrow in-distribution task it may
match a 9B decision model at a fraction of the latency, and it would also be the screener.

### Evaluation

Replace "decision accuracy, DELETE precision/recall" with:

- Per-question accuracy and Brier score, with reliability diagrams for the thresholds the policy
  uses.
- Verdict macro-F1 over the five classes, per language and per `comment_kind`.
- Two harm rates, reported first because they dominate the cost matrix: hazards lost (a gold
  Keep with `hazard = true` that the tool deleted) and errors introduced (a rewrite that the
  gold judge marks contradicting). A deleted hazard costs about ten deleted noise comments.
- Churn: edits to items the gold says to leave alone.
- Robustness: accuracy under one paraphrase of each criterion's instruction and description,
  and with one unrelated question added to the schema (the joint head couples answers). If
  either moves accuracy by more than a couple of points, the schema is fragile. Option order
  does not need testing for `choice` on Clef, which sorts options.
- A fixed "AI-drafted PR" set of 30 real changesets, scored end to end in changeset mode.

## Rollout

1. Build the state (Proposal 1) and evidence (Proposal 2) extractors behind `--diagnose`, and
   store them in SQLite. No model change yet. Measure how often evidence alone decides.
2. Zero-shot Clef-Flash and Clef 27B behind a `judge = systemone` backend, strict preset,
   against the re-labelled 198 rows plus 300 mined rows. Keep the Gemma generator for rewrites,
   add the verification loop. Ship if hazards-lost is at or below today's and nothing is
   introduced.
3. Changeset mode (Proposal 3) in `check` and CI, report-only.
4. Collect 3-5k labelled rows, fine-tune, compare against the encoder control, and pick the
   screener/judge pair by the harm rates and local latency.

Steps 1 and 3 are worth doing even if decision models disappoint, because they fix the input
problem for any judge.

## Open questions

- Which local serving path to standardise on for Apple Silicon. oMLX 0.7.1 serving
  `/v1/systemone` is the obvious choice given the tool already targets oMLX; mlx-decision and
  llama.cpp (`ggml-org/Clef-Flash-GGUF`, text-only, one sequence per batch) are the
  alternatives. Whichever it is, the client should check `usage.input_tokens` against its own
  count so silent truncation is caught.
- Whether sending source to Workers AI is acceptable for any of the user's repositories. At
  about 50 cents per 5,000 items it would remove the latency problem entirely.
- Whether a fine-tuned ModernBERT-class encoder ends up beating the fine-tuned decision model on
  this data. No published comparison exists; the control in Proposal 4 answers it.
- Whether "Rename instead" should ever edit (it cannot be done safely by this tool) or only
  report. The proposal says report.

## Sources

Decision models and serving:

- Cloudflare/clef-flash model card: https://huggingface.co/Cloudflare/clef-flash (protocol,
  `joint_schema_model.py`, benchmarks, latency)
- Cloudflare/clef model card: https://huggingface.co/Cloudflare/clef
- Launch post: https://blog.cloudflare.com/clef-decision-models/ (training recipe, RL service)
- Follow-up post: https://blog.cloudflare.com/clef-faster-cheaper-multimodal (long-state latency,
  SGLang, pricing)
- Workers AI docs and changelog: https://developers.cloudflare.com/workers-ai/models/clef-flash/,
  https://developers.cloudflare.com/changelog/post/2026-10-01-clef-workers-ai/
- vLLM structured-decision models: https://docs.vllm.ai/en/latest/models/structured_decisions_models/
- llama.cpp `/v1/systemone`: https://github.com/ggml-org/llama.cpp/pull/29818 and Clef support
  https://github.com/ggml-org/llama.cpp/pull/29831; write-up
  https://huggingface.co/blog/ggml-org/decision-models-in-llamacpp (response example)
- Apple Silicon: https://github.com/armbues/mlx-decision; oMLX measurement
  https://jacar.es/en/qwen-clef-omlx-decision-models/; https://huggingface.co/mlx-community/clef-8bit
- Fine-tuning: ms-swift https://swift.readthedocs.io/en/latest/BestPractices/Decision.html;
  https://github.com/MersivMedia/clef-finetune; https://github.com/harsha3187/clef-fine-tuning
- Other families: https://huggingface.co/vllm-sr/d3, https://huggingface.co/LiquidAI/d1-3B,
  https://huggingface.co/perplexity-ai/pplx-decider-v1.1-27b
- Decision models as judges: JEV-as-a-Judge https://arxiv.org/abs/2609.26550; LLM-as-Jev
  https://arxiv.org/abs/2610.02076; https://github.com/equationalapplications/system-one-reviewer
- Ecosystem index: https://github.com/rohinrohin/awesome-clef
- Typed-decision data format: https://huggingface.co/datasets/LocalLLaMA/typed-decisions

Datasets and prior tools:

- AIDev agent-authored PRs: https://huggingface.co/datasets/hao-li/AIDev (arXiv 2602.09185)
- CCIBench / CCISolver: https://arxiv.org/abs/2506.20558; JITData
  https://github.com/panthap2/deep-jit-inconsistency-detection
- CoDocBench: https://doi.org/10.5281/zenodo.14251622
- Jabrayilzade et al. smells and the replication with code: https://arxiv.org/abs/2504.18956
- FIRE IRSE useful/not-useful comments: https://arxiv.org/pdf/2311.03374
- NLBSE comment classification: https://huggingface.co/datasets/NLBSE/nlbse25-code-comment-classification
- DocPrism (C4RLLaMA precision collapse): https://arxiv.org/abs/2511.00215
- Kang, Milliken & Yoo (LLM comment inaccuracy): https://arxiv.org/abs/2406.14836
- CodeCrash (misleading comments degrade LLMs): https://arxiv.org/abs/2504.14119
- Agent comment cleanup rubric (gist): https://gist.github.com/bavanws/123e0343f8a79cec825d9141124a0a83;
  https://github.com/anthropics/claude-code/issues/65961
