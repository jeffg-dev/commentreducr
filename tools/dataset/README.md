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
