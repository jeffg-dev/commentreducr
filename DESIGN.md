# commentreducr design

Quick-and-dirty developer tool. Clean, accurate code; minimal tests; no elaborate edge-case
handling. The one property that matters: **never corrupt the target codebase.** Everything that
is not a comment (strings, template literals, regex literals, JSX text, docstrings) must be
byte-for-byte untouched, and structural comments must be preserved.

## Pipeline (per file)

files::tracked_source_files -> parse::extract_comments -> parse::group_blocks
  -> for each block: prose::analyze, structural::is_structural, policy::decide
  -> Reduce actions: llm::LlmClient::summarize (fallback = extractive) 
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

## LLM

OpenAI-compatible `/v1/chat/completions`, default `http://localhost:8000/v1`, model
`gemma-4-e2b-it-4bit` (oMLX). Tested: `temperature: 0`, `max_tokens: 60`, system prompt
"You condense multi-line source code comments into a single line. Reply with exactly one line
of plain text, no more than N words, no quotes, no markdown, no preamble. Preserve identifiers
and technical terms verbatim." Model emits no thinking by default. ~0.4s/request; the server
does continuous batching, 8 in flight gives ~3.7x throughput.
