# Comment-reduction dataset

Labeled examples for prompt-optimizing the small local LLM (Gemma 4 E2B) that
commentreducr uses to decide what to do with a multi-line source comment:
replace it with one terse line, or delete it. Intended for DSPy-style
optimization and as few-shot demos. All rows are synthetic (hand-written,
embedded in short realistic code) so there are no licensing concerns.

## Rubric

Comments should be reduced to really terse, short lines, and kept only if
helpful. The only time a comment is really helpful is when it explains
something surprising, or a danger or caution. Most of the time "just read the
code" is the right answer, so most blocks are `DELETE`.

Keep (as one line, at most 12 words, no trailing period, no leading
"Note:"/"Warning:"/"This"/"We") only when the comment conveys something the
code does not show and a reader would regret not knowing:

- a surprising or non-obvious behaviour, invariant, or constraint (ordering
  requirements, units, off-by-one reasoning, why the obvious simpler approach
  is wrong)
- a danger or caution: thread safety, security, data loss, performance cliff,
  must-call-X-first, do-not-reorder
- a workaround for an external bug or quirk that would otherwise look like a
  mistake (the line names the quirk, not the history)

`DELETE` when the comment is any of: restating what the code does; narrating
steps; history, changelog, "previously this used to..."; tickets, authors,
dates; describing how other parts of the code or other modules behave; design
rationale evident from the code; general education about a library or
language feature; motivational or apologetic text; commented-out code;
examples that duplicate tests.

If a block mixes fluff with one genuinely surprising fact, the label is the
terse line for that fact only. Tie-breaker: if a strong engineer would be
annoyed that the line survived (e.g. it says what `compare_digest`,
`safe_load`, `useMemo`, or `* 1000` already say), it is `DELETE`.

## File format

`comments.jsonl`, one JSON object per line:

| field      | meaning |
|------------|---------|
| `id`       | `py-NNNN` or `js-NNNN`, unique |
| `language` | `python`, `javascript`, or `typescript` |
| `source`   | `synthetic` (would be `<repo>@<sha>:<path>:<line>` for mined rows) |
| `comment`  | raw comment block including `#`, `//`, or `/* */` delimiters and original line breaks |
| `context`  | first non-blank code line after the block, trimmed, max 120 chars |
| `output`   | `DELETE` or the terse replacement line |
| `why`      | one short clause explaining the label (for humans, not the model) |

## Label distribution

120 rows: 84 `DELETE` (70%), 36 kept.

| language   | rows | DELETE | kept |
|------------|-----:|-------:|-----:|
| python     |   60 |     42 |   18 |
| typescript |   40 |     22 |   18 |
| javascript |   20 |     20 |    0 |

(JS rows lean DELETE because the JS-flavoured synthetic blocks were written as
the narration/history/commented-out-code cases; the TS rows carry the JS-side
danger and quirk cases. Treat `javascript` + `typescript` as one 60-row split.)

## Train/dev split

Split deterministically by id so the split is stable across edits:

```python
import hashlib
def is_dev(row):
    return int(hashlib.sha1(row["id"].encode()).hexdigest(), 16) % 5 == 0  # ~20%
```

Optimize on train, report on dev; never tune the prompt on dev rows.

## Docstring dataset

Labeled examples for prompt-optimizing the same small local LLM's other job: deciding what to do
with one Python docstring (module, class, or function/method) — replace it with a short
contract-only version, or delete it. All rows are synthetic; a handful are paraphrased from a
real "bad" test module supplied for this task (never copied verbatim).

### Rubric

The full rubric, with real good / bad / no-docstring examples, is
[docstring_rubric.md](docstring_rubric.md); the labels and the prompt's demos follow it.
Test code is different from everything else:

- A test function or method docstring says what it guards or tests, in one or two lines.
- A test module (or test class) docstring is concise: zero (`DELETE`) up to about one short
  paragraph — keep it only if something a reader of the individual tests would not otherwise
  see is buried there (a pinned schedule, a fixture requirement, a design constraint on the
  tests themselves).

Everything else follows the contract-not-story rule: a docstring earns its place only when it
states something a **caller** needs — which parameters or fields matter and why, the return
contract, a gotcha, a call-order hazard, an invariant not visible in the signature — and it must
stay true after the body is rewritten. `DELETE` when the docstring narrates the implementation,
restates the name/signature, reads like a pre-code spec (re-typing arguments or a request
schema), carries history/tickets/PR numbers/authors/dates/phases, argues architecture or product
rationale, counts today's callers, sits on a trivial one-liner/pass-through/dunder/plain data
class, or is a "TEMPORARY — delete this file" note. When a genuine contract or hazard is buried
in bloat, the label keeps only that fact, terse, no story.

### File format

`docstrings.jsonl`, one JSON object per line:

| field           | meaning |
|-----------------|---------|
| `id`            | `doc-NNNN`, unique |
| `kind`          | `module`, `class`, or `function` |
| `name`          | the class/function name; `""` for a module |
| `signature`     | the `def`/`class` header, one line; `""` for a module |
| `is_test`       | `true` for a test function/method/class, or a test module |
| `in_test_file`  | `true` if the docstring lives in a `test_*.py` file (even when `is_test` is `false`, e.g. a test-file helper) |
| `body_lines`    | number of lines in the body that follows the docstring |
| `body_preview`  | up to 3 lines of that body |
| `docstring`     | the raw (often bloated) docstring text, `\n`-joined |
| `output`        | `DELETE`, or the replacement docstring text (`\n`-joined) |
| `why`           | one short clause explaining the label (for humans, not the model) |
| `source`        | `synthetic` |

### Label distribution

60 rows: 37 `DELETE` (62%), 23 kept.

| kind       | is_test | rows | kept | DELETE |
|------------|:-------:|-----:|-----:|-------:|
| module     | false   |    8 |    3 |      5 |
| module     | true    |   10 |    4 |      6 |
| class      | false   |   10 |    3 |      7 |
| class      | true    |    2 |    1 |      1 |
| function   | false   |   16 |    6 |     10 |
| function   | true    |   14 |    6 |      8 |

### How to run

```
cargo run -- docstrings --eval tools/dataset/docstrings.jsonl
```

Measured 2026-09-08 against oMLX, 8 requests in flight:

| model                     | decision accuracy | DELETE precision / recall | kept avg lines / words | wall (58 rows) |
|---------------------------|------------------:|--------------------------:|-----------------------:|---------------:|
| `gemma-4-e2b-it-4bit`     |             86.2% |             88.6% / 88.6% |             1.7 / 18.2 |           14 s |
| `gemma-4-26b-a4b-it-4bit` |             87.9% |             86.8% / 94.3% |             3.1 / 27.4 |           56 s |

The decisions tie, but E2B's rewrites tend to drop the one gotcha the docstring exists to
state, so `docstrings` defaults to the 26B model (`docstrings_model` in the config file).
For reference the comment prompt on the same server: E2B 86.7%, 26B 94.2% on the 120-row set.

Prints per-row `ok`/`MISS` (expected label vs. the first line of what the model returned), then
decision accuracy, `DELETE` precision/recall, and — for rows the model decided to keep — the
average line and word count of the replacement text. Requires a reachable LLM endpoint (see
`DESIGN.md`); there is no extractive fallback.
