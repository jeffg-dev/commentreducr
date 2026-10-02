# commentreducr design

Quick-and-dirty developer tool. Clean, accurate code; minimal tests; no elaborate edge-case
handling. The one property that matters: **never corrupt the target codebase.** Everything that
is not a comment (strings, template literals, regex literals, JSX text, docstrings) must be
byte-for-byte untouched, and structural comments must be preserved. The tool has two targets:
comments (Python, JS/TS, YAML, Rust) and Python docstrings (module, class, function).

## CLI and selection

`reduce [path] --scope comments|docstrings|all --language python|typescript|rust|all --workers N`
classifies every non-structural Python item and sends flagged items to the LLM. Other supported
languages bypass the Python-only classifier and go directly to the LLM. `delete` uses the same
scope/language selection and removes all safely editable non-structural items without inference.
TypeScript includes TSX; all also includes JavaScript and YAML. There are no line-count, density,
trailing-comment, or code-like gates. Every build includes local classifier inference and SQLite.

`install-git-hook` installs/upgrades the warning-only pre-push wrapper. `check [--warn]` checks
changed committed Python blocks, with whole surviving blocks selected when an interior deletion
changes them. Manual checks use upstream..HEAD. Git push stdin determines all actual pushed
refs. Existing hooks are chained with their original arguments, stdin, and status. Checks leave
source untouched and share the classifier cache with reduction.

Tracked source selection and config list layering remain unchanged: Git-tracked files minus
repo/config ignores, Rust UI snapshots skipped, defaults -> config -> flags. Scope and language
also support config keys. Paths preserve symlinks for the planner to reject before any writes.

## Reduction and durable state

`plan_file` reads one source snapshot and collects comment blocks and Python docstrings together,
preserving structural items. MiniLM-L12 receives the same frozen kind/text/context pair as the
hook. All Python targets are screened, including short and inline blocks. Oversized targets
are errors requiring manual review. `state::Database` stores input-keyed PASS/FLAG probabilities
and item records with file, source hash, byte/line range, kind, classification, state, disposition,
replacement edit, and errors. Non-Python classifications are DIRECT.

The default SQLite database is in the current worktree's Git metadata directory at
`commentreducr/state.sqlite`; `--database` or config `database` overrides it. SQLite uses WAL
and synchronous FULL; a file lock prevents concurrent runs on one database. Classifier keys
include the frozen model/settings, target, and context. LLM checkpoints additionally bind the
whole source snapshot and LLM settings. Pending/in-progress/errors retry; ready verdicts reuse.

Workers operate on individual flagged items, not whole files. Preflight is needed only for
uncached LLM jobs. A file with an unresolved item is not changed; its completed verdicts remain
ready. For a complete file, validate the edited parse, verify the original snapshot, persist a
prepared rewrite and both hashes, then atomically replace the source and commit applied states.
On resume, matching the before hash replays the prepared write; matching the after hash commits
its disposition without writing again. Other source edits invalidate the file checkpoint and
force rescan. Completed unchanged outputs are skipped for matching scope/language/model settings.

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
Rust: doc comments (`///` but not `////`, `//!`, `/** */` but not `/***` or `/**/`, `/*! */`:
they are `#[doc]` attributes, doctests run from them, and `missing_docs` can make removing one a
build error), `SAFETY:` anywhere in any case (clippy's `undocumented_unsafe_blocks`),
`@generated`, mdBook `ANCHOR:`/`ANCHOR_END:`, `grcov-excl-*`/`LCOV_EXCL_*`, and at the start of
the comment `region`/`endregion`, `noinspection`, `@formatter:`, `language=`, `nosemgrep`,
`TODO/FIXME/XXX/HACK/NOTE`. A shebang is its own node in the Rust grammar, never a comment.
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
license/copyright/SPDX text; the def/class carries a decorator matching `keep_decorators`
(default `tool`, `command`, `group`: Strands `@tool`, click/typer commands -- its docstring
becomes the decorated callable's prompt/help text); or the class's bases match `keep_bases`
(default `Signature`, `BaseModel`: a `dspy.Signature` subclass's docstring is the signature's
instructions, a `pydantic.BaseModel` subclass's is the structured-output schema description),
including a same-file subclass of one. A pattern `P` matches a dotted name `N` when `N == P` or
`N` ends with `"." + P` (case-sensitive), so `tool` matches `@strands.tool(...)` and
`Signature` matches `dspy.Signature`. A user config's `keep_decorators`/`keep_bases` lists
extend these defaults, same as `ignore`.

`delete`: `docstring::delete_edit` removes the whole lines the docstring occupies plus any
immediately-following blank lines, so the body never starts blank. A docstring that is the
entire body of its def/class (`only_statement`) becomes `pass` instead, whatever its line shape.
A docstring sharing a line with other code (`def g(self): """x"""; y = 1`, or anything after it
on its own line) is left untouched — too risky to edit safely.

`reduce`: flagged docstrings go to `LlmClient::rewrite_docstring`, which replies `DELETE` or
`KEEP` plus replacement text. The reply is capped per kind: 2 lines for a test
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
library education, commented-out code) should go. One flavor gets its own class (D4) because it
is a hygiene problem, not just noise: a comment that explains how *other* code uses, expects,
mirrors or must stay in sync with this code ("module X expects this shape", "same as in Y()",
"called from the scheduler", "see Z for details") leaks the caller into the callee and goes stale
the moment either side moves. Those are deleted; when one wraps a real hazard, the kept line
restates the hazard in this code's own terms and names no other module, file or caller. A generic
precondition on any caller ("call flush() before close()", "caller must hold the lock") is a
contract, not a leak. The docstring prompt applies the same rule. So the model does not
"summarize": it replies either `DELETE` or one terse line (<= max_words). `llm::Verdict` carries
that; reduce mode deletes the block on `DELETE`. The system prompt states the rubric and sixteen
few-shot demos (both languages, majority DELETE) are sent as prior user/assistant turns.
Only classifier-flagged Python targets and all non-Python targets reach the LLM. The prompts
and demos are unchanged. Inline/trailing comment replacement stays inside the comment's byte
range; own-line replacements keep their original indentation and terminators. Unsafe replies
and parse failures leave the whole file untouched. `--dry-run` applies to `delete` only.

`reduce --scope comments --eval tools/dataset/comments.jsonl` and
`reduce --scope docstrings --eval tools/dataset/docstrings.jsonl` bypass screening to measure
the runtime LLM prompt path.

## Token budget and prefix caching

oMLX prefix-caches in 512-token blocks: cached tokens per request = the constant prefix rounded
down to a multiple of 512. The comment prefix (system prompt + 16 demos + "Comment:") is
tuned to ~2096 tokens so 2048 are cached and only ~50 tokens of prefix plus the comment
(~100 tokens) are prefilled per request; the docstring prefix (system prompt + 12 demos) sits at
~2730 so 2560 are cached. Completions average ~6 tokens because a delete verdict
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
