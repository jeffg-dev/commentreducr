<p align="center"><img src="https://raw.githubusercontent.com/jeffg-dev/commentreducr/main/assets/banner.jpg" alt="commentreducr" width="600"></p>

# commentreducr

[![crates.io](https://img.shields.io/crates/v/commentreducr.svg)](https://crates.io/crates/commentreducr)
[![CI](https://github.com/jeffg-dev/commentreducr/actions/workflows/ci.yml/badge.svg)](https://github.com/jeffg-dev/commentreducr/actions/workflows/ci.yml)

Strips low-value comments from JS/TS/Python/YAML/Rust and low-value Python docstrings in a git
repo, keeping only the ones that say something the code cannot: a hazard, a caution, a
workaround, a caller contract. Never touches code, strings or structural comments.

## Before / after

Examples produced by the LLM reducer on a small sample repo. Classifier screening
now selects which Python blocks reach this stage.

```diff
--- a/orders.py
+++ b/orders.py
@@ -1,22 +1,10 @@
-"""Order export helpers.
-
-This module was split out of billing/export.py during the Q3 refactor (PLAT-1180)
-so that the nightly report job in reports/nightly.py could import it without
-pulling in the whole billing package. The scheduler calls export_orders once per
-tenant; see scheduler.py for how the callbacks are fired.
-"""
 import csv
 import hmac
 import time
 
 
 def normalize_currency(code: str) -> str:
-    """Uppercases and validates a 3-letter ISO 4217 currency code.
-
-    This is called from the checkout flow, the invoice serializer and about a
-    dozen other places, so changing the validation here affects all of them. It
-    mirrors the check in payments/validate.py -- keep the two in sync.
-    """
+    """Uppercases and validates a 3-letter ISO 4217 currency code."""
     code = code.strip().upper()
     if code not in SUPPORTED:
         raise ValueError(code)
@@ -24,41 +12,24 @@ def normalize_currency(code: str) -> str:
 
 
 def release_hold(order) -> None:
-    """Releases the payment hold on an order.
-
-    Call this before charge_order, never after: charge_order re-reads the
-    order's held_amount to compute the refundable difference, and once the
-    hold is released that field reads zero, so the refund silently comes out
-    as nothing. We found this the hard way in staging last month when a retry
-    path called these in the wrong order.
-
-    Internally this just flips a boolean and appends a ledger row.
+    """Must be called before charge_order, never after: charge_order reads
+    held_amount to compute the refund, and it reads zero once the hold is
+    released.
     """
     order.hold_released = True
     order.ledger.append(("release", order.held_amount))
 
 
 def export_orders(orders, out):
-    # Loop over every order in the list, format each one as a CSV row and
-    # write it out to the stream. We skip cancelled orders because the nightly
-    # report job in reports/nightly.py does not want them in the export, and
-    # the finance team asked for that back in March (see PLAT-1203).
     writer = csv.writer(out)
     for order in orders:
         if order.cancelled:
             continue
         writer.writerow(order.as_row())
 
-    # The timeout here is in seconds, not milliseconds like every other call
-    # in this package, because the upstream flush API multiplies it by 1000
-    # on its side before handing it to the socket. Passing 5000 here, as we
-    # did once, means the flush will happily wait over an hour.
+    # timeout is seconds, not ms; upstream multiplies by 1000
     out.flush(timeout=5)
 
 
 def verify(token: str, expected: str) -> bool:
-    # We use compare_digest here instead of == because == short-circuits on
-    # the first differing byte, so an attacker could measure the response time
-    # and learn the token one byte at a time. This is the classic timing
-    # attack; see the OWASP page on constant-time comparison for details.
     return hmac.compare_digest(token, expected)
```

```diff
--- a/search.ts
+++ b/search.ts
@@ -1,20 +1,9 @@
 import { debounce } from "./util";
 
-// The router module handles matching the URL against the route table. It
-// lives in ./router.js and exports a single `match` function that returns
-// the handler or null. We call it here so the search page can deep-link
-// straight to a result; the header component does the same thing on load.
 const route = match(location.pathname);
 
-// Wait 300ms after the last keystroke before firing the search so that we
-// don't hammer the API with a request per character. 300 felt about right
-// in testing; 500 felt laggy and 100 still produced a burst of requests
-// on fast typists. Tracked in FE-2210 if we ever want to tune it again.
 export const search = debounce(runSearch, 300);
 
-// The config module reads env vars at import time, so importing it before
-// dotenv has run means every value is undefined and nothing warns you. The
-// ordering of these two imports is therefore important and has bitten us
-// twice already; please do not let the formatter or an import sorter move it.
+// import order matters; ensure dotenv is loaded before config
 import "dotenv/config";
 import { config } from "./config";
```

The module docstring (history, "the scheduler calls this, see scheduler.py") is gone. The
currency docstring keeps its one-line contract and drops "mirrors payments/validate.py, keep
in sync." `release_hold` is the case worth pausing on: there's a real hazard underneath (call
order matters, and a refund can silently come out as zero) so the docstring survives, but the
staging war story and "internally this just flips a boolean" narration of the implementation
are cut. That's the contract, not the story. The CSV-export narration and the ticket
reference are gone; the units gotcha survives as one line. The router cross-reference ("lives
in ./router.js", "the header component does the same") is gone because it names other files,
not a hazard in this code. The debounce tuning anecdote is gone. The import-order hazard
survives, restated without the "has bitten us twice" history.

## Install

```sh
cargo install commentreducr
```

Every build bundles a CPU inference runtime and SQLite. It requires no Python or
inference server for classification. The first reduction or hook installation downloads and
verifies the frozen 34 MB MiniLM-L12 model; subsequent classification runs offline.
Reduction of flagged items still uses your configured OpenAI-compatible LLM endpoint.

## Usage

```sh
commentreducr reduce . --scope all --language all --workers 8
commentreducr reduce src --scope comments --language rust --workers 4
commentreducr reduce . --scope docstrings --language python
commentreducr delete . --scope all --language python --dry-run
commentreducr delete . --scope comments --language typescript
commentreducr install-git-hook
commentreducr check --warn
```

Both `--scope` and `--language` default to `all`. Scopes are `comments`, `docstrings`,
and `all`; docstrings means Python module/class/function docstrings. Languages are
`python`, `typescript`, `rust`, and `all`. `typescript` includes TSX; `all` also includes
JavaScript and YAML. A path limits the run to a tracked file or directory; omitted means
`.`. Untracked files, configured ignores, and Git ignore rules are respected. Rust UI
fixtures with a tracked `.stderr` sibling are skipped because their snapshots pin line numbers.

This is a breaking CLI change: the `comments` and `docstrings` subcommands and the
`--reduce`/`--delete` mode flags are removed. Use `reduce` or `delete` with `--scope`.
`install-hook` is now `install-git-hook`; `prepush-check` is now `check`. Run
`install-git-hook` again to upgrade a previously installed hook and preserve its original chain.

`reduce` examines every non-structural item, including single-line, trailing, inline,
and code-like comments. Python items are screened by the frozen classifier; only FLAG
items reach the LLM, which can retain, reduce, or delete them. Other languages go directly
to the LLM because the classifier was trained for Python. `--workers N` bounds concurrent
LLM requests, including requests for different items in one file. The line-count and density
gates are gone; old `min_lines`/`min_density` config keys are ignored.

`delete` removes every safely editable non-structural item in scope without a classifier
or LLM. `--dry-run` applies to deletion only and prints proposed changes without writing.

## Resume and local state

Repeat the same `reduce` command after interruption. SQLite stores classifier decisions
and each item's file, source snapshot, one-based line range, kind, progress, and disposition.
Completed LLM verdicts are reused; failed or interrupted requests are retried. An interrupted
request may be sent again if its response had not yet been committed locally.

The default database is `commentreducr/state.sqlite` in the current worktree's Git metadata
directory, normally `.git/commentreducr/state.sqlite`. It stays out of the source tree.
Use `--database FILE` or the `database` config key to choose a location. `check` reuses the
same classifier cache. Concurrent operations on a database fail promptly rather than mix work.

Classifier cache keys include the frozen model/settings and the complete target plus its
code context. LLM checkpoints also include the source snapshot, models, endpoint, and output
settings. A source edit invalidates obsolete offsets. A completed file is skipped only while
its output and settings still match the checkpoint.

A file with unresolved items is left untouched. Successful item verdicts remain cached for
its next attempt; other files can finish. Before applying a completed file, the tool records
its planned rewrite, verifies the current source, validates parsing, and atomically replaces
it. Resume handles interruption before or after replacement without applying an edit twice.

For example, inspect the ledger with SQLite:

```sql
SELECT file, start_line, end_line, kind, classification, state, result_type, error
FROM items
ORDER BY file, start_line;
```

Classification values are `PASS`, `FLAG`, `DIRECT` (non-Python), or `ERROR`. Progress is
`pending`, `in_progress`, `ready`, `kept`, `applied`, or `error`; dispositions are `keep`,
`reduce`, `delete`, or `error`. Cached classifier probabilities are in `classifications`.

## Git hook and checks

`install-git-hook` installs an executable pre-push hook running `commentreducr check --warn`.
It preserves existing hooks, their stdin/arguments, and their failure status. Installation is
repeatable and upgrades the previous commentreducr wrapper.

`check` reports changed Python comment/docstring blocks in the actual committed trees being
pushed, including multiple refs. A manual invocation compares HEAD with its upstream; new
branches use known remote history, and the first push checks the whole Python tree. Only
changed blocks are classified, with the complete block and nearby code as context. Source
files are untouched. This checker remains Python-only.

`check --warn` prints findings and errors to stdout and exits 0. Strict `check` exits 1 for
findings or errors, otherwise 0. Findings require author review. Oversized targets that cannot
fit the classifier are reported for manual review rather than truncated. Reduction similarly
leaves their file untouched and records the error.

The model cache defaults to `~/.cache/commentreducr` (`XDG_CACHE_HOME` or `LOCALAPPDATA` when
set). `COMMENTREDUCR_MODEL_PATH` selects a local copy for offline setup. Its checksum is
verified when loaded. The model weights and threshold are unchanged in this CLI migration.

## Preserved documentation

Structural comments survive both modes: license/SPDX text, shebangs, modelines, linter and
formatter directives, TODO/FIXME/XXX/HACK/NOTE, JSDoc, Rust doc comments, and Rust `SAFETY:`
comments. Structural Python docstrings also survive: doctests, module help consumed through
`__doc__`, protected decorators, and protected base classes. Config lists extend the defaults.

The rubric is strict: comments need an unexpected reason, a non-obvious trap, or a shortcut
through complex code. Docstrings explain the consumer contract. Narration, implementation
restatements, history, caller cross-references, and unnecessary passages count as bloat even
when the block also includes useful material. The classifier flags the whole block; the LLM
stage decides what can survive.

## Configuration

Settings use `~/.config/commentreducr/config.toml` (`XDG_CONFIG_HOME` when set), or
`--config FILE`. Flags override the file, which overrides these defaults. `model` supplies
comments and is the fallback for docstrings; `docstrings_model` can override it. The `--model`
flag overrides both for a run. List values append to the defaults.

```toml
scope = "all"
language = "all"
endpoint = "http://localhost:8000/v1"
model = "gemma-4-e2b-it-4bit"
docstrings_model = "gemma-4-26b-a4b-it-4bit"
workers = 8
max_words = 20
ignore = ["migrations/"]
keep_decorators = ["tool", "command", "group"]
keep_bases = ["Signature", "BaseModel"]
# database = "/path/to/local/state.sqlite"
# api_key = "..."
```

Malformed config is an error; a missing file uses defaults. Invalid/unreadable source files
are skipped with a warning. Errors produce exit 1 while preserving unfinished files.
`reduce --diagnose` parses without classification, LLM calls, or writes and emits a redacted
parse report. Prompt evaluation bypasses classifier screening:

```sh
cargo run -- reduce --scope comments --eval tools/dataset/comments.jsonl
cargo run -- reduce --scope docstrings --eval tools/dataset/docstrings.jsonl
```
