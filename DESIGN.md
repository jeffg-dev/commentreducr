# commentreducr design

Quick-and-dirty developer tool. Clean, accurate code; minimal tests; no elaborate edge-case
handling. The one property that matters: **never corrupt the target codebase.** Everything that
is not a comment (strings, template literals, regex literals, JSX text, docstrings) must be
byte-for-byte untouched, and structural comments must be preserved.

## Pipeline (per file)

files::tracked_source_files -> parse::extract_comments -> parse::group_blocks
  -> for each block: prose::analyze, structural::is_structural, policy::decide
  -> Reduce actions: llm::LlmClient::summarize (any failure aborts the run)
  -> rewrite::{delete_edit,reduce_edit} -> rewrite::apply -> write file

## Modes

- `--reduce` (default): structural / trailing / short (< min_lines prose lines) / low-density
  (< min_density words per line) / commented-out-code blocks are kept. Big dense prose blocks
  are replaced by one line `{indent}{prefix} {summary}`.
- `--delete`: every non-structural comment is removed. No LLM involved.

## Structural comments (always kept)

Python: shebang, PEP 263 coding cookie, `# noqa`, `# type:`, `# pyright:`, `# pylint:`,
`# mypy:`, `# ruff:`, `# isort:`, `# fmt:`, `# pragma`, `# nosec`, `# noinspection`,
`# cython:`, `# distutils:`, `# %%` cell markers, `# TODO/FIXME/XXX/HACK/NOTE`.
JS/TS: `eslint-*`, `@ts-ignore/@ts-expect-error/@ts-nocheck/@ts-check`, `prettier-ignore`,
`biome-ignore`, `istanbul ignore`, `c8 ignore`, `v8 ignore`, `@flow`, `@jsx*`, `@license`,
`@preserve`, `/*!`, `/** JSDoc */` (any `/**` block), `/* webpack...*/` and `/* vite */` magic
comments, `#region/#endregion`, `/// <reference`, `//# sourceMappingURL`, `//# sourceURL`,
`@generated`, `TODO/FIXME/XXX/HACK/NOTE`.
Both: license/copyright/SPDX text anywhere in the block; any block that starts at line 0 or 1
of the file and mentions license/copyright.

## LLM verdict protocol

The user's comment philosophy: a comment earns its place only when it says something the code
cannot — a surprise, a danger, a caution, a workaround for an external quirk. Everything else
(narration, history, tickets, descriptions of other code, rationale evident from the code,
library education, commented-out code) should go. So the model does not "summarize": it replies
either `DELETE` or one terse line (<= max_words). `llm::Verdict` carries that; reduce mode deletes
the block on `DELETE`. The system prompt states the rubric and seven few-shot demos (both
languages, majority DELETE) are sent as prior user/assistant turns. Only blocks that pass the
policy gate (own-line, >= min_lines prose lines, >= min_density words/line, not code-like) reach
the model; shorter blocks are kept untouched. Reduce mode requires the LLM: `LlmClient::check`
runs before any file is touched. There is no extractive fallback: if a call fails mid-run the
block is left unchanged with a warning. `--dry-run` applies to `--delete` only.

## Resilience

The run is best-effort after the preflight. A file that cannot be read or parsed, or whose
processing panics (a bug), is skipped with a warning and never written. Byte offsets from
tree-sitter are char boundaries, but derived offsets (`block.end - 1`) may not be, so the
line-scanning helpers work on bytes. Skips and LLM failures are counted in the summary and
make the exit status 1.

`--eval tools/dataset/comments.jsonl` runs the labeled dataset through the exact runtime prompt
path and prints decision accuracy, DELETE precision/recall, and every mismatch — use it to
iterate on the prompt or demos.

## Token budget and prefix caching

oMLX prefix-caches in 512-token blocks: cached tokens per request = the constant prefix rounded
down to a multiple of 512. The constant prefix (system prompt + 12 demos + "Comment:") is
tuned to ~1590 tokens so 1536 are cached and only ~55 tokens of prefix plus the comment
(~100 tokens) are prefilled per request. Completions average ~6 tokens because a delete verdict
is a two-character class code. Measure with `--eval` (prints per-request prompt/cached/completion
tokens); a one-row dataset with a tiny comment gives the prefix size directly.

If you edit the prompt or demos, keep the prefix just above a 512 boundary. Compressing the
taxonomy wording was tried and cost ~8 points of accuracy; adding demos to reach the next
boundary is the better lever. Prose sent to the model is capped at 150 words and the context
line at 80 chars.

## LLM endpoint

OpenAI-compatible `/v1/chat/completions`, default `http://localhost:8000/v1`, model
`gemma-4-e2b-it-4bit` (oMLX). Tested: `temperature: 0`, `max_tokens: 60`, system prompt
"You condense multi-line source code comments into a single line. Reply with exactly one line
of plain text, no more than N words, no quotes, no markdown, no preamble. Preserve identifiers
and technical terms verbatim." Model emits no thinking by default. ~0.4s/request; the server
does continuous batching, 8 in flight gives ~3.7x throughput.
