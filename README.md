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
commentreducr reduce . --ai          # for AI agents: JSONL advice on stdout
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

`reduce` works in four steps:

1. Fixed rules keep structural items without asking a model (see Preserved documentation).
2. Each remaining item gets its context: the code it describes, the enclosing function or
   class, and evidence such as whether the names it mentions exist.
3. A decision model (Clef-Flash by default, through a `/v1/systemone` endpoint such as oMLX
   0.7.1 or later) answers 13 yes/no and score questions about the item, and fixed thresholds
   turn the answers into keep, delete, rewrite, fix or review. Items that are close to a
   threshold are review; `reduce` keeps them unchanged.
4. A local generative model (`generator_model`) writes the rewrites. The decision model checks
   each rewrite: it must keep every fact, add none, and not contradict the code. A rewrite
   that fails the check leaves the original.

Python items are first screened by the frozen MiniLM classifier; items it passes are kept
without reaching the judge. `--workers N` bounds concurrent model requests. `reduce` edits
files in place and never runs Git commands: review and commit the result yourself.
`judge = "legacy"` (or `--judge legacy`) restores the 1.x prompt path.

### For AI agents: `--ai`

`reduce --ai` is for running under an AI coding harness such as Claude Code. It applies only
the deletes the judge is sure of, writes nothing else, and prints one JSON object per line on
stdout for every item it left for the agent. Everything else goes to stderr. Items close to a
threshold count as uncertain: the review band is at least `ai_margin` (0.1) wide in this
mode. A rerun reprints the open advice from cached verdicts.

```json
{"comment":"// A later registered operator can send work to an earlier one. Continue\n// until a complete pass runs none.","end_line":65,"evidence":"authored_by_agent: False; comment_age_days: 0; code_newer_than_comment: False","kind":"inline","path":"d2.ts","recommendation":"Delete or reduce via rewrite","signals":[{"signal":"P(contradicts the code); fix above","threshold":0.4,"value":0.2},{"signal":"information the code does not show, 0-3; delete below","threshold":0.5,"value":1.14},{"signal":"P(a rename could replace it); rename above","threshold":0.5,"value":0.07},{"signal":"P(warns about a trap or hazard); keep at or above","threshold":0.3,"value":0.2},{"signal":"explains why, 0-3; keep at or above","threshold":1.0,"value":0.53}],"start_line":64}
```

`recommendation` is one of:

| Recommendation | Meaning |
|---|---|
| `Delete or reduce via rewrite` | Uncertain: the item is near a threshold. |
| `Reduce via rewrite` | Keep the meaning in fewer words. |
| `Fix via rewrite: it may contradict the code` | The comment may be wrong. |
| `Delete after renaming the code so the name says it` | A better name would carry the comment. |
| `Fix or delete: the code it describes changed` | `--changed` only: the comment may now be stale. |

`signals` are the judge's scores that decided the item, each with its threshold. `comment` is
the item as it appears in the file, delimiters included. Keys are in alphabetical order.

### Using your own judge model

Point either mode at any `/v1/systemone` server, for example a fine-tuned Clef-Flash served by
`tools/finetune/serve.py`, and give it the thresholds tuned for it:

```sh
commentreducr reduce . --judge-endpoint http://127.0.0.1:8102/v1/systemone \
  --judge-model my-clef-ft --judge-thresholds my-thresholds.json
```

The same settings work as `judge_endpoint`, `judge_model` and `judge_thresholds` in the config
file. The thresholds file holds only the keys to change, for example
`{"stale_g1": 0.9, "ai_margin": 0.15}`.

`delete` removes every safely editable non-structural item in scope without a classifier
or LLM. `--dry-run` applies to deletion only and prints proposed changes without writing.

### Only changed comments

`reduce --changed BASE..HEAD` judges only comments added or edited in that commit range, and
reports (never edits) unchanged comments that the change made wrong. The working
tree must match HEAD; files outside the path argument are left alone.

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

Classification values are `PASS`, `FLAG`, `DIRECT` (non-Python or oversized), or `ERROR`.
Progress is `pending`, `in_progress`, `ready`, `kept`, `applied`, or `error`; dispositions are `keep`,
`reduce`, `delete`, `review`, `rename`, `stale`, `rewrite` and `fix` (the last two in `--ai` only), or `error`. Cached classifier probabilities are in `classifications`.

## Git hook and checks

`install-git-hook` installs an executable pre-push hook running `commentreducr check --warn`.
It preserves existing hooks, their stdin/arguments, and their failure status. Installation is
repeatable and upgrades the previous commentreducr wrapper.

`check` reports changed Python comment/docstring blocks in the actual committed trees being
pushed, including multiple refs. A manual invocation compares HEAD with its upstream; new
branches use known remote history, and the first push checks the whole Python tree. Only
changed blocks are classified, with the complete block and nearby code as context. Source
files are untouched. With the default `judge = "systemone"` it checks every language through
the judge, report-only; `judge = "legacy"` checks only Python, with the classifier. `--changed BASE..HEAD` checks a commit range instead of the push.
`--format json` or `--format sarif` prints a findings document on stdout (the summary goes to
stderr), and `--output FILE` writes the findings to a file instead.

Every comment in a changed file falls into one of three tiers. Tier 1 is a comment added or
modified in the range; it gets the full judgement and can be reported as bloat or wrong.
Tier 2 is an unchanged comment whose code or enclosing function changed. It is judged only
when the change touched something the comment names: an identifier (matched across camelCase
and snake_case) on a line of the comment's code or enclosing function that the change removed or
rewrote, or a number or quoted string whose
value the change removed. That token is the finding's evidence. The judge is then asked only
whether the code after the change contradicts the comment; if it does, the comment is reported
as stale. Tier 2 is never edited, and with `judge = "legacy"` it is never reported. Tier 3 is
everything else, which is not judged. `reduce --changed BASE..HEAD` applies the same tiers and
edits only tier 1.

To show findings as pull request annotations, upload the SARIF in a GitHub workflow:

```yaml
- run: commentreducr check --changed origin/${{ github.base_ref }}..HEAD --format sarif --output results.sarif --warn
- uses: github/codeql-action/upload-sarif@v3
  with:
    sarif_file: results.sarif
```

`check --warn` prints findings and errors to stdout and exits 0. Strict `check` exits 1 for
findings or errors, otherwise 0. Findings require author review. Oversized targets that cannot
fit the classifier are reported for manual review rather than truncated. `reduce` sends
these blocks directly to the LLM.

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
flag overrides both for a run. `judge` picks the verdict path; `legacy` is the original prompt
path (flags: `--judge`, `--judge-model`, `--judge-endpoint`, `--judge-thresholds`). List values append to the defaults.

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
judge = "systemone"              # systemone | generative | legacy
judge_model = "clef-flash-4bit"
# judge_endpoint = "..."         # full URL (generative: .../chat/completions or its base); default {endpoint}/systemone (systemone) or endpoint
generator_model = "gemma-4-26b-a4b-it-4bit"   # systemone: writes the rewrites the judge verifies
# escalation_model = "..."       # no escalation when unset
# judge_thresholds = "th.json"   # JSON of policy thresholds merged over the shipped ones (--judge-thresholds)
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
