# Dataset format v2

One JSON object per line (JSONL). This is the format for every row the project holds: gold,
mined, generated or donated.

**Private data stays outside the repo.** The maintainer's gold set lives in `private/` at the
repo root, which is gitignored. Never commit it, copy it into a tracked path, or send it to an
external service. Tools take data paths as arguments; tests use small synthetic fixtures.

## Example

```json
{
  "id": "own-0001",
  "source": {"repo": "github.com/org/repo", "commit": "<sha>", "path": "src/cache.py",
             "line": 120, "license": "MIT", "pr": null},
  "language": "python",
  "comment_kind": "inline",
  "placement": "before_statement",
  "comment": "# The lock must be acquired before touching _cache; ...",
  "annotated_code": "with self._cache_lock:\n    self._cache[key] = value",
  "enclosing_scope": "def set(self, key, value):\n    ...",
  "symbol": {"name": "Cache.set", "public": false, "is_test": false},
  "change": {"authored_by_agent": true, "agent": "claude-code",
             "comment_age_days": 12, "code_newer_than_comment": false},
  "labels": {
    "kind": "warning",
    "g1_contradicts": false,
    "g1_unverifiable": false,
    "hazard": true,
    "c2_info_gain": 2,
    "c3_rationale": 2,
    "c4_level": "lower_precision",
    "c8_durability": 2,
    "c9_replaceable": "no",
    "leak": false,
    "c5_contract": null,
    "c10_actionable": null,
    "verdict": "rewrite_shorter",
    "rewrite": "# _cache is written by the refresher thread; hold _cache_lock",
    "rename_suggestion": null
  },
  "claims": [
    {"text": "the refresher thread writes _cache concurrently", "status": "confirmed"}
  ],
  "labeler": {"type": "human", "model": null, "confidence": "high", "blind": true},
  "why": "thread-safety hazard not visible at the read site"
}
```

## Context fields

| Field | Rule |
|---|---|
| `id` | Unique. Prefix names the source (`own-`, `aidev-`, a donor name). |
| `source` | `repo`, `commit`, `path`, `line`, `license`, `pr` (null unless mined from a PR). `repo` is `"synthetic"` for invented items. |
| `language` | `python`, `javascript`, `typescript`, `tsx`, `rust`, `yaml`. |
| `comment_kind` | `inline`; `api_doc` (docstring, JSDoc, rustdoc, Go-style doc); `todo` (TODO/FIXME/HACK/XXX); `directive` (linter and tool pragmas; kept structural, present only for taxonomy coverage). |
| `placement` | `before_statement`, `trailing`, `function_head`, `class_head`, `module_head`. |
| `comment` | Raw text with delimiters and line breaks. A run of own-line comments at one indentation is one item. |
| `annotated_code` | The statement, definition or block the comment attaches to, comment removed. Doc comment: signature plus first 10 body lines. Module head: first 20 lines after it. |
| `enclosing_scope` | Innermost function or class containing the item, comment removed, at most 80 lines centred on the item; module level gets 40 lines either side. Contains `annotated_code`. |
| `symbol` | `name` of the nearest enclosing definition; `public` by language convention; `is_test` by path or name. |
| `change` | `authored_by_agent`, `agent` (a tool name, never a person), `comment_age_days`, `code_newer_than_comment`, and in changeset mode `comment_new`, `comment_modified`, `scope_changed` (bool) and `annotated_code_before` (string, only for a comment whose code changed). `null` or absent where unknown, and always in whole-tree rows. |

## Labels

Values follow the question schema in `docs/reduce-redesign.md` (Proposal 1), transcribed in
`src/schema/strict-v1.json`.

| Label | Type | Values |
|---|---|---|
| `kind` | enum | `rationale`, `summary`, `contract`, `invariant`, `warning`, `alternatives_rejected`, `todo`, `directive`, `restating`, `commented_out_code`, `journal_or_history`, `banner`, `cross_reference_leak`, `change_narration` |
| `g1_contradicts` | bool | true if any claim is `contradicted` |
| `g1_unverifiable` | bool | true if any claim is `unverifiable` and none is `contradicted` |
| `hazard` | bool | see guidance |
| `c2_info_gain` | 0-3 | 0 restates; 1 paraphrases; 2 adds one fact the code cannot show; 3 adds facts unrecoverable from the whole scope |
| `c3_rationale` | 0-3 | 0 no why where needed; 1 vague; 2 specific; 3 specific plus rejected alternative or consequence. Use 2 when no why is needed. |
| `c4_level` | enum | `same_level_narration`, `higher_summary`, `lower_precision`, `mental_model` |
| `c8_durability` | 0-3 | 0 volatile facts; 1 tied to often-changing detail; 2 stable intent; 3 stable and machine-checked or linked to a durable record |
| `c9_replaceable` | enum | `fully`, `partly`, `no` |
| `leak` | bool | explains this code by another module, file, caller or sibling |
| `c5_contract` | 0-3 or null | null unless `comment_kind` is `api_doc` |
| `c10_actionable` | 0-3 or null | null unless `comment_kind` is `todo` |
| `verdict` | enum | `keep`, `rewrite_shorter`, `delete`, `rename_instead`, `fix_wrong_claim`, under the strict preset |
| `rewrite` | string or null | required when verdict is `rewrite_shorter` or `fix_wrong_claim`, else null |
| `rename_suggestion` | string or null | required when `c9_replaceable` is `fully` |

Other fields:

- `claims`: every factual claim the comment makes, each `{"text", "status"}` with status
  `confirmed`, `contradicted` or `unverifiable` against the provided code. Empty for pure
  narration.
- `labeler`: `type` (`human` or `agent`), `model` (null for humans), `confidence`
  (`high`, `medium`, `low`), `blind` (true when the labeller saw no model suggestion).
- `why`: one clause a human can scan.

## Labelling guidance

- Label the comment as found, against the code in front of you, not as you would have written
  it. Do not consult the commit message, ticket or author.
- Write the claims before scoring anything; the gates follow from them.
- `hazard` means a competent first-time reader of `annotated_code` would be surprised or make a
  mistake without the comment: ordering, units, thread or reentrancy constraints, security or
  data-loss consequences, an external bug worked around, a caller precondition. Useful
  background is not a hazard.
- A generic precondition on any caller ("hold the lock before calling") is not a `leak`.
- `c9_replaceable = fully` means you can name the identifier, type or assertion that would
  replace it; write it in `rename_suggestion`.
- Strict verdict: an inline comment survives only if it states a hazard, a genuine contract,
  or a specific rationale the code cannot carry; a doc comment survives only if it states a
  contract a caller needs; nothing survives if it contradicts the code.
  - `keep` only when the comment is already terse. A kept hazard in a long block is
    `rewrite_shorter` with the rewrite written out.
  - `rewrite` text: one line for inline, under 15 words, no "Note:"/"This"/"We", naming the
    library or identifier and never another file or caller. Doc comments: a summary line plus
    the contract.
  - `fix_wrong_claim` requires at least one `contradicted` claim; use `delete` if nothing true
    remains.
  - Doc comments are never deleted only for being long; a signature echo (c5 = 0) is.

## Split

`tools/dataset/v2/split.py` is the only implementation; import `split_of(id)`.

```
b = int(sha1(id).hexdigest(), 16) % 10
b < 5 -> train;  b in {5, 6} -> dev;  b >= 7 -> test
```
