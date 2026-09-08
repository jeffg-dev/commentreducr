# commentreducr design

Quick-and-dirty developer tool. Clean, accurate code; minimal tests; no elaborate edge-case
handling. The one property that matters: **never corrupt the target codebase.** Everything that
is not a comment (strings, template literals, regex literals, JSX text, docstrings) must be
byte-for-byte untouched, and structural comments must be preserved. The tool has two targets:
comments (Python, JS/TS, YAML) and Python docstrings (module, class, function).

## CLI

`commentreducr comments <path> --delete|--reduce [...]` and
`commentreducr docstrings <path> --delete|--reduce [...]` are the two subcommands; both wrap the
same set of flags (`Config` carries a `target: Target` field set from which one was invoked).

## Pipeline (per file)

Comments: files::tracked_source_files -> parse::extract_comments -> parse::group_blocks
  -> for each block: prose::analyze, structural::is_structural, policy::decide
  -> Reduce actions: llm::LlmClient::summarize (a failure leaves the block unchanged)
  -> rewrite::{delete_edit,reduce_edit} -> rewrite::apply -> write file

Docstrings: files::tracked_source_files (Python only) -> docstring::extract_docstrings
  -> for each docstring: docstring::is_structural, then the min_lines gate (reduce mode)
  -> Reduce actions: llm::LlmClient::rewrite_docstring (a failure leaves the docstring unchanged)
  -> docstring::{delete_edit,replace_edit} -> rewrite::apply -> write file

## Modes

- `--reduce` (default): structural / trailing / short (< min_lines prose lines) / low-density
  (< min_density words per line) / commented-out-code blocks are kept. Big dense prose blocks
  are replaced by one line `{indent}{prefix} {summary}`.
- `--delete`: every non-structural comment is removed. No LLM involved.
- YAML comments follow the same two modes; commented-out YAML (a `#`-prefixed line that is
  itself valid-looking YAML) is code-like and kept like commented-out code in the other
  languages. tree-sitter-yaml is the parser; a comment is any `comment` node in its grammar.

## Structural comments (always kept)

Python: shebang, PEP 263 coding cookie, `# noqa`, `# type:`, `# pyright:`, `# pylint:`,
`# mypy:`, `# ruff:`, `# isort:`, `# fmt:`, `# pragma`, `# nosec`, `# noinspection`,
`# cython:`, `# distutils:`, `# %%` cell markers, `# TODO/FIXME/XXX/HACK/NOTE`.
JS/TS: `eslint-*`, `@ts-ignore/@ts-expect-error/@ts-nocheck/@ts-check`, `prettier-ignore`,
`biome-ignore`, `istanbul ignore`, `c8 ignore`, `v8 ignore`, `@flow`, `@jsx*`, `@license`,
`@preserve`, `/*!`, `/** JSDoc */` (any `/**` block), `/* webpack...*/` and `/* vite */` magic
comments, `#region/#endregion`, `/// <reference`, `//# sourceMappingURL`, `//# sourceURL`,
`@generated`, `TODO/FIXME/XXX/HACK/NOTE`.
YAML: shebang, `yaml-language-server:`, `yamllint`, `prettier-ignore`, `noqa`, `checkov:skip`,
`bridgecrew:skip`, `kics-scan`, `tflint-ignore`, `trivy:ignore`, `renovate:`, `ansible-lint`,
`kube-linter`, `nosemgrep`, `ruleid:`, `pragma`, `@formatter:`, `region/endregion`, `language=`
(IntelliJ injection), `TODO/FIXME/XXX/HACK/NOTE`.
All: license/copyright/SPDX text anywhere in the block; any block that starts at line 0 or 1
of the file and mentions license/copyright; editor modelines anywhere in the comment
(`vim:`/`vi:`/`ex:` followed by `set`/`settings`, or an Emacs `-*- ... -*-` line).

## Docstrings

A docstring is `expression_statement > string` as the first non-comment statement of a module,
class body, or function body (an `async def` still counts; a decorated def/class is unwrapped
first). Only the `r`/`R`/`u`/`U` prefixes (or none) qualify — an `f`/`b` string in that position
is a formatted or byte string, never a docstring.

Structural (always kept, in both modes): the text contains a doctest prompt (`>>>`); a module
docstring in a file that reads `__doc__` anywhere (argparse/click render it as help text);
license/copyright/SPDX text; or the def/class carries a click/typer command/group decorator
(its docstring becomes the command's help).

`--delete`: `docstring::delete_edit` removes the whole lines the docstring occupies plus any
immediately-following blank lines, so the body never starts blank. A docstring that is the
entire body of its def/class (`only_statement`) becomes `pass` instead, whatever its line shape.
A docstring sharing a line with other code (`def g(self): """x"""; y = 1`, or anything after it
on its own line) is left untouched — too risky to edit safely.

`--reduce`: docstrings with fewer non-blank text lines than `--min-lines` are kept untouched, no
LLM call. Everything else goes to `LlmClient::rewrite_docstring`, which replies `DELETE` or
`KEEP` followed by replacement text; `llm::DocVerdict` carries that, and reduce mode deletes on
`DELETE` the same way `--delete` does. The reply is capped per kind: 2 lines for a test
function/method, 5 for a test module/class, 15 for anything else. `docstring::replace_edit`
preserves the original quote style and prefix and shapes multi-line text PEP 257-style (summary
line, then indented continuation lines); a reply containing the quote sequence or a backslash is
rejected as unsafe to splice in verbatim and the docstring is left unchanged with a warning, like
a failed call. Test detection: `docstring::is_test_file` matches `test_*.py`/`*_test.py`/
`conftest.py` or any `tests`/`test` path component; a function/method is a test by a
`test`/`Test`-prefixed name, a class by a `Test`-prefixed name, and a module docstring is a test
when its file is.

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

Reduce mode runs in two passes: `plan_file` (read, parse, decide) over every file with no LLM
to count the blocks that will be sent, then the real pass. `progress::Progress` shows percent,
blocks, files, ETA and prompt/completion tokens with tokens/s; it redraws in place on a terminal
and prints 10% milestones otherwise. Warnings and verbose output go through it so they never
land inside the progress line. The final `tokens:` line includes the preflight request.

## Resilience

The run is best-effort after the preflight. A file that cannot be read or parsed, or whose
processing panics (a bug), is skipped with a warning and never written. Byte offsets from
tree-sitter are char boundaries, but derived offsets (`block.end - 1`) may not be, so the
line-scanning helpers work on bytes. Skips and LLM failures are counted in the summary and
make the exit status 1.

`comments --eval tools/dataset/comments.jsonl` and `docstrings --eval tools/dataset/docstrings.jsonl`
run their labeled dataset through the exact runtime prompt path and print decision accuracy,
DELETE precision/recall, and every mismatch — use them to iterate on the prompt or demos.

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
`gemma-4-e2b-it-4bit` for comments and `gemma-4-26b-a4b-it-4bit` for docstrings (oMLX; the
config key `docstrings_model` overrides the latter). Measured on the 60-row docstring set the big
model decides better (90% vs 85%) and keeps the gotcha in its rewrites where E2B drops it, at
~4x the time per request; on the 120-row comment set it scores 94% vs 87%. Tested: `temperature: 0`, `max_tokens: 60`, system prompt
"You condense multi-line source code comments into a single line. Reply with exactly one line
of plain text, no more than N words, no quotes, no markdown, no preamble. Preserve identifiers
and technical terms verbatim." Model emits no thinking by default. ~0.4s/request; the server
does continuous batching, 8 in flight gives ~3.7x throughput.
