<p align="center"><img src="https://raw.githubusercontent.com/jeffg-dev/commentreducr/main/assets/banner.jpg" alt="commentreducr" width="600"></p>

# commentreducr

[![crates.io](https://img.shields.io/crates/v/commentreducr.svg)](https://crates.io/crates/commentreducr)
[![CI](https://github.com/jeffg-dev/commentreducr/actions/workflows/ci.yml/badge.svg)](https://github.com/jeffg-dev/commentreducr/actions/workflows/ci.yml)

Strips low-value comments from JS/TS/Python/YAML and low-value Python docstrings in a git
repo, keeping only the ones that say something the code cannot: a hazard, a caution, a
workaround, a caller contract. Never touches code, strings or structural comments.

## Before / after

Real output, unedited, from `commentreducr comments --reduce` and
`commentreducr docstrings --reduce` on a small sample repo, default local models.

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

## Quick start

```sh
commentreducr comments . --delete --dry-run   # count what --delete would remove: no LLM, no writes
commentreducr comments . --delete             # remove every non-structural comment
commentreducr comments .                      # --reduce (default): an LLM keeps, rewrites or deletes each dense block
commentreducr docstrings .                    # same for Python docstrings; --delete works here too
```

Both subcommands only process files `git ls-files` reports as tracked, so untracked scratch
files and anything `.gitignore`d are left alone. `--delete` removes every non-structural
comment/docstring outright, no LLM. `--reduce` (the default) is pickier: short, sparse or
code-like blocks are left alone, and dense blocks go to a local LLM that replies `DELETE` or a
terse replacement line. `-h` shows common flags; `--help` shows everything, including tuning
flags.

## What is kept

Structural comments are never touched by either mode: linter/type-checker directives
(`# noqa`, `@ts-ignore`, `eslint-*`, ...), `TODO`/`FIXME`/`HACK`/`NOTE`, licenses and SPDX
headers, shebangs, editor modelines, JSDoc blocks, and language-specific pragmas. The full list
is in [DESIGN.md](DESIGN.md).

For docstrings, `--delete` and `--reduce` both always keep: doctests, a module docstring in a
file that reads `__doc__` (argparse/click render it as help text), any click/typer command's
docstring, and license text. `--reduce` additionally leaves a docstring alone if it has fewer
than `--min-lines` text lines, no LLM call needed.

## What gets cut

Everything else is a candidate. The rule the LLM applies is the same for both comments and
docstrings: a comment or docstring earns its place only when it says something the code
cannot, and it loses that place even when phrased as a warning if it is really narration,
history, a tutorial on a library, or rationale the identifier already states.

One flavor gets its own rule because it's a hygiene problem, not just noise: a comment or
docstring that explains how *other* code uses, expects, mirrors, or must stay in sync with this
one (see the `router.js` and `payments/validate.py` lines above) leaks a caller's concern into
the callee and goes stale the moment either side moves, so it's deleted. When it wraps a real
hazard, the kept line restates the hazard in this code's own terms and names no other module or
caller. A generic precondition on any caller ("call flush() before close()", "hold the lock")
is a contract, not a leak, and stays.

For docstrings specifically, `--reduce` aims for the contract a caller needs rather than a
story of the implementation: a test function's docstring becomes one or two lines saying what
it guards, a test module's becomes zero to one short paragraph, everything else gets a one-line
summary plus at most a short paragraph or Args/Returns list.

## Safety

None of this touches anything that is not a comment or docstring: string and template
literals, regex literals, and JSX text are byte-for-byte untouched. `tools/corpus_check.py`
runs `--delete` over a tree and asserts the Python AST (modulo docstrings) and every YAML
document are unchanged; it passes on the Python stdlib and 1266 real-world YAML files.

Files that fail to parse are skipped with a warning and never written. To report one, run
`commentreducr comments --diagnose <path>`: it parses only, touches nothing, and prints a
redacted report (node kinds and line shapes, no paths or code) safe to paste into an issue.

Output: a per-file summary line for every file with a change (`-v`/`--verbose` adds a line per
block). The run total reports both block counts and the source lines they cover, e.g.
`120 deleted (2340 lines), 5 reduced (60 lines saved)`. A file that fails to parse, or an LLM
call that fails mid-run, is counted as a skip/warning and makes the exit status 1; `--reduce`
leaves that one block unchanged rather than guessing.

## LLM setup

`--reduce` needs a reachable OpenAI-compatible chat endpoint (`/v1/chat/completions`) and
checks it before touching any file. `comments` defaults to
[Gemma 4 E2B](https://huggingface.co/mlx-community/gemma-4-e2b-it-4bit) (MLX); `docstrings`
defaults to [Gemma 4 26B A4B](https://huggingface.co/mlx-community/gemma-4-26b-a4b-it-4bit),
about 4x slower per request but less likely to drop the one gotcha a docstring exists to
state. Other models run but are unmeasured. Measured 2026-09-08 on
[oMLX](https://github.com/jundot/omlx) (Apple Silicon, which caches the prompt prefix): the
comment prompt decides correctly 90.0% of the time on a 130-row labeled set (Gemma 4 E2B), the
docstring prompt 92.6% on a 68-row set (Gemma 4 26B A4B). For the comment model that's about
0.4s per request, and the default 8 workers give roughly 3.7x the throughput of one at a time.
Because the prefix is cached, each request only has to prefill roughly 50-150 tokens plus the
comment itself.

Config file at `~/.config/commentreducr/config.toml` (or `--config FILE`); flags override.

```toml
endpoint = "http://localhost:8000/v1"        # default
model = "gemma-4-e2b-it-4bit"                # default for comments (and docstrings if docstrings_model is unset)
docstrings_model = "gemma-4-26b-a4b-it-4bit" # default for docstrings
api_key = "sk-..."                            # optional
workers = 8                                   # worker threads / max in-flight LLM requests, default 8
min_lines = 4                                 # minimum lines in a block before it's reduced, default 4
min_density = 5.0                             # comments only: minimum average words per line, default 5
max_words = 20                                # comments only: target max words in a summary, default 20
```

`--delete --dry-run` counts without writing so you can preview a `--delete` run; `--dry-run`
only applies to `--delete`. `--reduce` scans first, then shows progress on stderr (percent,
blocks, files, time left, token counts and throughput) and prints token totals at the end. It
has no offline fallback: if a call fails mid-run, that one block is left unchanged with a
warning.

## Development

```sh
cargo test
cargo fmt --check && cargo clippy --all-targets -- -D warnings       # CI gate
cargo run -- comments --eval tools/dataset/comments.jsonl            # score the comment prompt
cargo run -- docstrings --eval tools/dataset/docstrings.jsonl        # score the docstring prompt
cargo build --release && tools/corpus_check.py /usr/lib/python3.12 ~/some/repo   # never-corrupt check
```

PRs only; main requires CI. Both prompts have a labeled dataset and a rubric in
[tools/dataset](tools/dataset) (the docstring one is
[docstring_rubric.md](tools/dataset/docstring_rubric.md)); `--eval` prints decision accuracy,
DELETE precision/recall and token counts so a prompt change can be scored before and after.

Design notes, the full structural-comment lists, the LLM verdict protocol and the token budget
behind the prefix caching above are in [DESIGN.md](DESIGN.md).

## License

Apache 2.0
