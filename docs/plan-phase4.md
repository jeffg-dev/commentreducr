# Implementation plan: Phase 4 of the `reduce` redesign (changeset mode)

Audience: the implementer (a Sonnet-class agent working one step at a time) and the maintainer
reviewing each PR. Background: `docs/reduce-redesign.md` Proposal 3 and `docs/PLAN.md` Phase 4.
Phases 2 and 3 are in place on this branch (`docs/plan-phase2-3.md`); this file is the ordered
list of commits for Phase 4, with the files, signatures, tests and exit check for each.

Rules for every step (same as Phases 2-3):

- One step is one commit and one reviewable diff. `cargo fmt && cargo clippy --all-targets
  -- -D warnings && cargo test` is green at every commit. The Python tool tests stay green too:
  `python3 -m unittest tools/test_bakeoff_policy.py tools/bakeoff/test_render.py tools/bakeoff/test_run_judge.py`.
- No new crates. SARIF is a handful of JSON fields written with `serde_json::json!`.
- The parse-validate-atomic-write path in `reduction::run` and `lib::atomic_write` is not
  weakened. Changeset mode only decides *which* items are judged and *what* is sent to the judge;
  every edit still goes through `rewrite::apply`, the re-parse check, the source-hash check,
  `Database::prepare`, `atomic_write` and `Database::complete`. Report-only runs write nothing.
- Private data never enters the repo. Tests use synthetic temp repos (the `Repo` helpers in
  `src/hook_git.rs` tests, `tests/hook.rs` and `tests/reduce.rs`).
- The MiniLM screener, `classifier_key` and the `classifications` table do not change.
- The legacy prompt path stays selectable (`judge = "legacy"`, still the shipped default).

## What Phase 4 adds, in one paragraph

`reduce --changed <base>..<head>` and `check` compute a changeset from Git, sort every item in
each changed file into three tiers, and judge only tiers 1 and 2. Tier 1 (comment added or
modified in the range) gets the full schema and can be edited. Tier 2 (comment unchanged, but
the code it annotates or its enclosing scope changed) gets three questions (`g1_contradicts`,
`g1_unverifiable`, `c8_durability`) plus a before/after block of the annotated code, and is only
ever reported as stale or possibly stale; the one edit it can produce is a verified
`FixWrongClaim` rewrite, never a delete. Tier 3 (everything else) is not judged and not touched.
`check` runs the same pipeline on committed blobs, never writes, and prints findings as text,
JSON or SARIF. The pre-push hook keeps calling `check --warn`, so it stays warning-only.

## Order of work at a glance

| Step | Commit title | Depends on |
|---|---|---|
| 1 | `changeset.rs`: ranges, changed files, hunks and blobs for all languages | - |
| 2 | Tiers: `scope_lines` on `ItemContext`, `changeset::tier`, before-block lookup | 1 |
| 3 | `state.change` in the renderer (Python and Rust, byte-identical), `ItemState.change` | 2 |
| 4 | Agent authorship from the commits in range | 1 |
| 5 | 5a: `reduction::run` takes a changeset, tier-3 skip, cache isolation; 5b: tier-2 policy; 5c: report-only flag and findings | 3, 4 |
| 6 | `reduce --changed <base>..<head>` | 5 |
| 7 | `findings.rs`: text, JSON and SARIF | 5c |
| 8 | 8a: shared config builder, `--changed`/`--format` on the legacy path; 8b: `check` for all languages through the judge path | 6, 7 |
| 9 | `install-git-hook` upgrade; docs; decision log | 8 |

Steps 2-3 and 4 are independent of each other. Step 7 needs only the `Finding` type from
step 5c and can be written in parallel with step 6. `plan_source` moved from step 5 to step 2,
which is its first user.

---

### Step 1. `changeset.rs`: the Git side, for all languages

Why first: `hook_git.rs` already has every Git call Phase 4 needs (`pushed_ranges`,
`changed_paths`, `hunks`, `blob`, `ignored_paths`), but they are private, hard-coded to Python,
and return blocks rather than a reusable description of the change. This step moves the plumbing
into a module both `reduce --changed` and `check` call, and changes nothing the current `check`
prints.

Files: new `src/changeset.rs`; edit `src/hook_git.rs` (the moved functions become
`pub(crate)` in `changeset.rs`; `hook_git` keeps `HookBlock`, `changed_blocks`, `install` and
the tests), `src/lib.rs` (`mod changeset;`).

```rust
/// One range of commits. `base` is None on a first push (the whole tree is new).
pub(crate) struct Range { pub base: Option<String>, pub head: String }

pub(crate) struct Hunk { pub old_start: usize, pub old_count: usize, pub new_start: usize, pub new_count: usize }
// moved from hook_git; fields become pub(crate)

pub(crate) struct ChangedFile {
    pub path: PathBuf,            // path at `head`, relative to the repo root
    pub old_path: Option<PathBuf>, // path at `base`; None when added
    pub lang: Language,
    pub hunks: Vec<Hunk>,         // empty base: one hunk covering every line (as today)
    pub head_source: String,      // blob at head
    pub base_source: Option<String>, // blob at base; None when added or when base is None
}

pub(crate) struct Changeset {
    pub root: PathBuf,
    pub ranges: Vec<Range>,
    pub files: Vec<ChangedFile>,
    pub agents: Vec<String>,      // filled in step 4; empty until then
}

/// `input` is Git's pre-push stdin (Some) or None for upstream..HEAD, exactly as `changed_blocks`
/// takes today. `range` is an explicit `<base>..<head>` from the command line; when it is Some,
/// `input` and `remote` are ignored.
pub(crate) fn collect(repo: &Path, range: Option<&str>, input: Option<&str>, remote: Option<&str>,
    languages: Languages, ignore: &[String]) -> Result<Changeset>

/// Parse `a..b`; `a` and `b` are resolved with the existing `commit()` helper.
fn parse_range(repo: &Path, spec: &str) -> Result<Range>
```

Behaviour:

- `changed_paths` drops its `Language::Python` filter and keeps any path where
  `Language::from_path(&p.new)` is `Some` and `languages.matches(lang)`.
- `ignored_paths` is applied as today.
- `collect` does not parse anything. A blob that cannot be read or is not UTF-8 is an error,
  exactly as `blob` is today, so the legacy `check` output and exit code do not change. A head
  blob that does not parse is left to `reduction::run`, which already skips a file it cannot
  plan with a warning. A base blob that does not parse is not an error (today's
  `unwrap_or_default` for old blocks).
- `ChangedFile.path` is relative to `Changeset.root`, which is `git rev-parse --show-toplevel`
  (as `changed_blocks` computes today), never to the directory the user passed.
- Files are not deduplicated across ranges: each range's hunks are relative to its own base. `changed_blocks` keeps its per-block `seen` set, so its output is unchanged.
- `hook_git::changed_blocks` is rewritten on top of `collect` (filter `files` to Python, then
  run today's `source_blocks` / `changes_block` loop over `head_source` and `base_source`). Its
  output is unchanged.

Tests: the three existing tests in `hook_git::tests` pass unchanged (they cover ranges,
renames, deletions and the dirty working tree). Add one test in `changeset.rs` with the same
`Repo` helper (move it to `changeset.rs` and re-export for `hook_git`'s tests): a commit that
touches `a.py`, `b.ts` and `c.yaml` yields three `ChangedFile`s with the right languages and
hunks, and `languages = Languages::Python` yields one.

Exit check: `cargo test` green; `tests/hook.rs` output unchanged (the `cues.py:2-` and
`possible class docstring bloat` assertions); `git diff --stat` shows no change in
`hook_model.rs` or `state.rs`.

### Step 2. Tiers

Files: `src/context.rs`, `src/changeset.rs`, `src/lib.rs` (`plan_source`), new fixture
`tests/fixtures/changeset/` (two versions of one Python file and one TypeScript file, plus
`expected.json`).

`plan_source` lands here because the before block below needs a `Plan` built from a string:

```rust
// lib.rs
/// `plan_file` minus the symlink check and the read: plans `source` as if it were `path`.
pub(crate) fn plan_source(path: &Path, source: String, lang: Language, cfg: &Config) -> Result<Plan>
// plan_file keeps its symlink check and read, then calls plan_source. No behaviour change.
```

`ItemContext` gains one field, serialised and excluded from the state (like `annotated_lines`):

```rust
/// 0-based inclusive line range of the innermost enclosing function-like scope; None at
/// module level and when the innermost scope is a class, impl, trait, struct, enum or mod.
pub scope_lines: Option<(usize, usize)>,
```

`item_context` already finds the scope node to render `enclosing_scope`; record its
`start_position().row..=end_position().row` there, but only when its kind is one of the
function kinds in `scope_kinds` (`function_definition`, `function_item`, `method_definition`,
`function_declaration`, `generator_function_declaration`, `arrow_function`,
`function_expression`). The innermost scope is often a class, `impl` or `mod tests` block that
spans most of the file; using it would make every comment in it tier 2 on any edit, and tier 2
is paid for with a judge call in the pre-push hook. Those comments still reach tier 2 through
`annotated_lines`. This changes the `context` JSON column and `--dump-items` output by one key,
and nothing else (the state text is unchanged).

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Tier { Added, Modified, ScopeChanged, Untouched }
// Added and Modified are both "tier 1"; ScopeChanged is tier 2; Untouched is tier 3.

/// For tier 2: the annotated code of the same comment before the change.
pub(crate) struct Before { pub annotated_code: String }

/// `item_lines` is 1-based inclusive (as `Item::range` returns). `old_items` are the
/// (text, 1-based lines) of the base version's items of the same kind, used for the two rules
/// below that need the old side.
pub(crate) fn tier(item_lines: (usize, usize), kind: &str, text: &str, ctx: &ItemContext,
    hunks: &[Hunk], old_items: &[(String, (usize, usize))]) -> Tier
```

Rules, in order:

1. Tier 1 when today's `changes_block` says a hunk changes the block, minus today's exception
   (every relevant hunk also intersects an old block with the same kind and text: a changed code
   line carrying an unchanged trailing comment). Both functions move from `hook_git` to
   `changeset` unchanged. `Added` when no old item has the same text, else `Modified`.
   `changes_block` takes `SourceBlock`s, whose `kind` and `text` today come from
   `source_blocks` (Python-only kind strings). `tier` builds its `SourceBlock`s from `Item`s on
   both sides with `Item::kind()` and `Item::text()`, so old and new compare like with like;
   `source_blocks` stays for the legacy hook path only.
2. Tier 2 when not tier 1 and a hunk's new side intersects `ctx.annotated_lines` (converted to
   1-based) or `ctx.scope_lines`, or a pure deletion hunk sits inside either range (same
   `intersects` arithmetic as `changes_block`'s deletion case, applied to the range).
3. Tier 3 otherwise.

Before block for tier 2: plan the base source (`plan_source(old_path, base_source, ..)`,
`parse::parse_tree`, `plan_contexts`, with the same `cfg`; a base that does not parse gives no
before blocks, never an error), find the old item with the same kind and text, take its
`annotated_code`. Several old items with the same text: take the one whose `start_line` is
closest to the new item's. None found (comment unchanged but moved from another file, or a
rename): `Before` is None and the state carries only the after block.

```rust
pub(crate) fn before_block(old: &[(ItemContext, (usize, usize))], kind: &str, text: &str,
    new_lines: (usize, usize)) -> Option<Before>
```

Tests: `changeset::tests::tiers_from_fixture`: for each fixture pair, run `tier` on every item
of the new version and compare with `expected.json` (`[{"line": 8, "tier": "modified"}, ...]`).
The Python fixture covers: a new comment (added), an edited comment (modified), an untouched
comment whose statement changed (scope_changed), an untouched comment whose function body
changed elsewhere (scope_changed), an untouched comment in an untouched function (untouched), a
trailing comment on a changed line (scope_changed: rule 1's exception keeps it out of tier 1,
and rule 2 then catches it because its annotated line changed; this is the textbook stale
comment), a comment directly in a class body when only one method of the class changed
(untouched), and a module-level comment in a file where only another function changed
(untouched). The TypeScript fixture covers one of each of the three
tiers to prove the scope kinds work outside Python.

Exit check: fixture test green; `tests/fixtures/context/*.json` regenerated for the new
`scope_lines` key and nothing else in them differs.

### Step 3. `state.change` in the renderer

Files: `tools/bakeoff/render.py`, `tools/bakeoff/test_render.py`, `src/render.rs`,
`tests/fixtures/render/` (one new pair), `tools/dataset/v2/SCHEMA.md`, `src/lib.rs`
(`ItemState`).

The renderer must stay byte-identical between Python and Rust, and byte-identical to today for
rows that have no change record, because the Phase 1 thresholds were tuned on those bytes.

`render.py`:

```python
EVIDENCE_KEYS = ("authored_by_agent", "agent", "comment_age_days", "code_newer_than_comment",
                 "comment_new", "comment_modified", "scope_changed")
```

and, after the `evidence:` block and before `_fit`, when `change.get("annotated_code_before")`
is not None:

```python
head.append(f"annotated_code_before:\n{change['annotated_code_before']}")
```

(`annotated_code` already in the head is the after block; the label on the existing block does
not change, so whole-tree states are unchanged.)

`render.rs`: `StateRow` gains `comment_new: Option<bool>`, `comment_modified: Option<bool>`,
`scope_changed: Option<bool>`, `annotated_code_before: Option<&'a str>`; `from_v2_row` reads
them from `change`; `render_state` prints them in the same places and the same order. The
Rust fixture test and `test_write_rust_fixtures` gain one row with all four set and one with
only `scope_changed` set.

`SCHEMA.md`: the `change` row gains the four keys, each `null`/absent in whole-tree rows.

`ItemState` gains `pub tier: Option<Tier>` and `pub change: Option<ChangeRecord>` where

```rust
#[derive(Serialize)]
pub(crate) struct ChangeRecord {
    pub comment_new: bool, pub comment_modified: bool, pub scope_changed: bool,
    pub annotated_code_before: Option<String>,
}
```

`plan_states` gains a parameter `change: Option<(&ChangedFile, &[(ItemContext, (usize, usize))])>`
(the file's hunks and the base version's contexts); it calls `tier` and `before_block` per
item, builds the `ChangeRecord` for tiers 1 and 2 (None for tier 3), and passes the values into
`StateRow`. `Database::store_state` stores the record inside the existing `evidence` JSON column
as a `"change"` key (no schema migration; `user_version` stays 2).

Tests: `tools/bakeoff/test_render.py` and `render::tests` fixture parity; one assertion that a
row without `change` renders the same bytes as before (keep one existing fixture file untouched
and assert it still matches).

Exit check: both test suites green; `git diff tests/fixtures/render/` shows only additions.

### Step 4. Agent authorship from the commits in range

Files: `src/changeset.rs`, `src/evidence.rs`.

```rust
/// Lower-case tool names of every commit in `base..head` (or every commit reachable from head
/// when base is None, capped at 200) that `evidence::agent_of` recognises, deduplicated, in
/// first-seen order.
pub(crate) fn agents_in_range(repo: &Path, range: &Range,
    cache: &Mutex<HashMap<String, Option<String>>>) -> Vec<String>
```

`git log --format=%H base..head` then `agent_of` per sha (already cached per run). Any Git
failure gives an empty list, never an error.

Blame at the head revision. `plan_states` today calls `evidence::blame(repo, path)`, which
blames the working-tree file. In changeset mode the items come from the head blob, so on a
dirty tree, or when the pushed branch is not the one checked out, blame line numbers point at
other lines and the ages and agents are silently wrong (and `code_newer_than_comment` feeds the
tier-2 judgement). `evidence::blame` gains `rev: Option<&str>`; with `Some(head)` it runs
`git blame --line-porcelain <head> -- <path relative to the repo root>`. `plan_states` passes
`Some(&range.head)` through the `change` parameter and `None` otherwise, so whole-tree runs are
unchanged.

Use in `plan_states` (via the `change` parameter): for tier-1 items only, when
`ev.authored_by_agent` is still None after `git_evidence` (no blame, e.g. `check` on a blob or a
shallow clone), set `authored_by_agent = Some(!agents.is_empty())` and `agent = agents.first()`.
A blame answer, when there is one, wins because it names the comment's own commit.

`shortcut: a mixed human/agent range marks every unblamed tier-1 comment as agent-authored;
upgrade to per-commit blame at head (git blame <head> -- path) if the 30-PR validation shows it
matters.`

Tests: in `changeset::tests`, a temp repo with two commits, the second carrying a
`Co-Authored-By: Claude <noreply@anthropic.com>` trailer: `agents_in_range` over `first..second`
is `["claude-code"]`, over `first..first` is empty. And `blame` with `Some(head)` on a repo
whose working tree has two extra lines inserted at the top returns the head blob's line
count, not the working tree's.

Exit check: `cargo test` green; `reduce` on a non-Git directory still runs (empty agents).

### Step 5. Changeset-aware `reduction::run`, tier-2 policy, report-only findings

This is the core step. Three commits: 5a is the plumbing, the tier-3 skip and the cache
isolation that keeps a tier-2 item from ever reusing a delete; 5b is the tier-2 question set
and policy; 5c is the report-only flag and findings. Until 5b lands, tier-2 items are treated
like tier 3 (not judged, not touched), so 5a is safe on its own.

Files: `src/lib.rs`, `src/reduction.rs`, `src/render.rs`, `src/policy.rs`, `src/state.rs`
(5c, one read helper), `src/schema/stale-v1.json` (new), `tests/reduce.rs`.

5a:

```rust
// reduction.rs
pub(crate) struct RunMode<'a> { pub changes: Option<&'a Changeset>, pub report_only: bool }
pub(crate) fn run(root: &Path, tracked: Vec<(PathBuf, Language)>, cfg: &Config, mode: RunMode) -> Result<Stats>
// 5a passes report_only: false everywhere; the flag is wired in 5c.
```

Inside `reduction::run`:

- With `changes`, the planning loop iterates `changes.files` instead of `tracked`: `path` is
  `changes.root.join(&f.path)` (not the `root` argument, which can be a subdirectory or a single
  file; the caller filters `files` to the target first), `name` is that path canonicalised, as
  a string (the same key a whole-tree run uses), the source is `f.head_source` through
  `plan_source`.
- Write mode keeps today's guards in today's order: the symlink check from `plan_file`, then
  `database.resume` with the *working-tree* source (so a rerun after a successful
  `reduce --changed` finds the completed write and skips the file, and an interrupted write is
  recovered as today), then the working tree must equal `head_source`; otherwise the file is
  skipped with the warning `working tree differs from <head>; commit or stash, then rerun`.
- States are computed for every judge, including legacy, when `changes` is Some: the tier needs
  `ItemContext`. Whole-tree legacy runs still skip them, as today.
- The base contexts for the file are computed once (`plan_source(base_source)` →
  `plan_contexts`), and `plan_states` gets the `change` parameter.
- After `plan_states`, items whose tier is `Untouched` are moved from `plan.items` to
  `plan.structural`, and their entries are removed from `states` in the same pass, because
  `states[index]` must stay aligned with `plan.items[index]` (the worker and `store_state` index
  both by the same position). They are never screened, judged, stored or edited.
- Tier-2 items are not screened by the MiniLM classifier: its question is "is this bloat", not
  "is this still true", so a PASS would mark a stale comment `keep` without a judge call. They
  take the `DIRECT` path, as non-Python items do. The classifier itself and its cache are
  untouched.
- Cache isolation. `items` rows are unique on `(file, source_hash, bytes, kind,
  classifier_key, llm_key)`, and the tier is not in that key, while the tier of the same bytes
  depends on the range. Without a change here, a full-schema `delete` stored by a whole-tree
  run, an earlier `check` of another range, or a failed run, is reused as-is when the same item
  is tier 2 in this range, and the write loop deletes it. Tier-2 items therefore use
  `llm_key = fingerprint(&[&llm_key, "stale", render::SCHEMA_STALE])` (5b adds the schema; in
  5a the literal `"stale"` alone), so they never share a row with full-schema verdicts.
- Defence in depth in the write loop: a tier-2 item whose record is `delete`, or `reduce`
  without an edit produced by the tier-2 verified rewrite path, is an error for that file
  (`ensure!`), which skips the file as any other inconsistency does.
- Legacy judge with `changes`: tier 1 goes through `item.reduce` as today. Tier 2 never becomes
  a job: in 5a it is skipped like tier 3; in 5b its verdict is decided at plan time from
  evidence alone and stored with `Database::verdict`, so it can never reach `item.reduce`.

Tests (5a):

- (a) a tier-1 comment with `c2 = 0` is deleted and the file is rewritten through the normal
  path (file parses, `items.result_type = 'delete'`).
- (b) a tier-3 comment in the same file is untouched, has no `items` row, and the mock saw no
  request whose state contains its text.
- (d) with the working tree edited after `head`, the file is skipped with the
  `working tree differs` warning and not written; rerunning `reduce --changed` right after a
  successful run reports the file as resumed, not as differing.
- (f) cache isolation: run a whole-tree `reduce` with a mock that answers `c2 = 0` for a
  comment but fails on another item of the same file (so nothing is written and the `delete`
  row stays `ready`); then commit a change to the code under that comment and run
  `reduce --changed`. The comment is tier 2 and is not deleted.

5b, tier 2:

`src/schema/stale-v1.json`: `g1_contradicts`, `g1_unverifiable` and `c8_durability` copied
verbatim from `strict-v1.json` (same instruction and criteria text, so the Phase 1 per-question
accuracies apply), plus one instruction line at the top telling the judge that
`annotated_code_before` is the code before the change and `annotated_code` the code after.
`render::SCHEMA_STALE` embeds it. It joins the non-legacy branch of `llm_key` only (the legacy
key must stay byte-identical) and the tier-2 key above.

```rust
// policy.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stale { Stale, PossiblyStale, Fresh }
/// Tier 2. `Stale` when P(g1_contradicts) > th.g1 and not within `th.margin` of it (a value in
/// the margin is `PossiblyStale`, as `decide` sends it to review); `PossiblyStale` when
/// P(g1_unverifiable) > th.stale_unverifiable, or E[c8_durability] < th.stale_durability, or
/// `ev.literal_mismatch` is non-empty; else `Fresh`. With no answers (legacy judge) only the
/// evidence rule applies. Missing questions in the answers give `PossiblyStale`.
pub fn stale_verdict(answers: Option<&Answers>, ev: &Evidence, th: &Thresholds) -> (Stale, Vec<(String, f64, f64)>)
```

The 0.6 and 1.0 live in `thresholds-strict-v1.json` as `stale_unverifiable` and
`stale_durability` (two new keys; `Thresholds` gains two fields; `policy.py`'s
`DEFAULT_THRESHOLDS` gains the same two so the shared cases file still loads in both). One
shared case per rule in `tests/fixtures/policy/`, including a g1 value inside the margin.

In `judge_item`, a tier-2 item asks `questions_for(render::SCHEMA_STALE, None)` and maps:

| Stale verdict | disposition | edit |
|---|---|---|
| `Fresh` | `"keep"` | none |
| `PossiblyStale` | `"stale?"` | none |
| `Stale`, judge is not System One | `"stale"` | none |
| `Stale`, System One | `FixWrongClaim` path from step 13 of Phase 3: generator, `verify_rewrite`, `"reduce"` on success, `"stale"` on any rejection (including the generator saying delete) | the verified edit |

No escalation for tier 2 in this phase (`stale?` is already a note, not an edit).

Tier 2 can therefore never produce a `delete` edit; the only edit is a verified rewrite. The
worker closure's downgrade rule (`review`/`rename` pass through without an edit) adds
`"stale"` and `"stale?"`, and a tier-2 `"reduce"` whose edit is a no-op becomes `"stale"`, not
`"keep"` (today's rule would turn it into `keep` and hide the finding). `FileResult`/`Stats`
count both under a new `stale: usize`; `summary_line` prints `N possibly stale`.

Tests (5b):

- (c) a tier-2 comment gets a request whose question set is exactly the three stale ids and
  whose state contains `annotated_code_before:`; with `g1 = 0.9` and the chat mock returning
  a candidate that fails verification, the file keeps the original text and
  `items.result_type = 'stale'`.
- (e) `--judge legacy` with the same repo: tier 1 reaches `/chat/completions` (use a comment
  long enough to go `DIRECT`, or a non-Python file, so the test does not depend on the
  classifier), tier 2 makes no request and ends `keep` (no literal mismatch) or `stale?`
  (fixture with a literal mismatch).

5c, report-only and findings:

```rust
// lib.rs
pub(crate) struct Finding {
    pub path: PathBuf, pub start_line: usize, pub end_line: usize,
    pub tier: Tier, pub kind: String,            // comment_kind as_str
    pub verdict: String,                          // the item's disposition (items.result_type)
    pub checks: Vec<(String, f64, f64)>,          // (check name, value, threshold)
    pub evidence: String,                         // one line: the `evidence:` block joined with "; "
    pub message: String,                          // the text line `check` prints
}
// Stats gains `pub findings: Vec<Finding>` (empty for whole-tree runs).
```

- `report_only` skips `database.resume` entirely. This is required, not an optimisation:
  `resume` calls `atomic_write` when it finds a `prepared` write whose before-hash matches the
  source, so a `check` after an interrupted `reduce` would otherwise write to the working tree.
  It also never reads the working tree, and the write loop skips the working-tree comparison,
  `prepare`, `atomic_write` and `complete`. It still runs `rewrite::apply` and the re-parse
  check, so a finding set that would not parse is reported as a skipped file.
- Item verdicts are still stored (the hook pays for the judge once, `reduce --changed` reuses
  the verdict). Because `complete` is not called, a later write-mode run plans the file again
  and finds every verdict `ready`, which is the existing resume behaviour for an interrupted run.
- Findings, in both modes, for every record whose `result_type` is not `keep`; in write mode
  that leaves the notes (`review`, `rename`, `stale`, `stale?`) and the applied edits.
- Checks for cached verdicts: a cached record has no policy numbers in memory. `judge_item`
  writes the named checks into the ledger it already stores (`ledger["checks"]`), and a new
  `Database::answers(id) -> Option<serde_json::Value>` reads the `states.answers` column back
  when the finding is built. Legacy (`item.reduce`) records carry no checks.
- `Checks` in `policy.rs` becomes `pub(crate)` with a `pub(crate) fn decide_with_checks(...)
  -> (Verdict, Vec<(String, f64, f64)>)`. Each check gets a name at the call site (`"g1"`,
  `"api_quality"`, `"c5"`, `"c2"`, `"rename"`, `"hazard"`, `"c3"`, `"leak"`), nothing else in
  `decide` changes, and `policy.py` is not touched (the names are presentation only).

Tests (5c):

- (g) report-only on the 5a repo: findings for the tier-1 delete and the tier-2 stale item, the
  file bytes and mtime are unchanged, and a following write-mode `reduce --changed` makes no
  judge request and applies the delete.
- (h) report-only with a `prepared` `file_writes` row left by a killed `reduce` (reuse the
  setup of `killed_run_resumes_ready_verdicts_and_does_not_apply_stale_offsets`): the file is
  not written.

`tests/changed.rs` is fine if `reduce.rs` gets long; the `Mock` and `Repo` helpers move to
`tests/common/mod.rs`. The mock routes on the question ids in the request body.

Exit check for all three commits: `cargo test` green; the whole-tree path (`mode.changes =
None`) produces the same `items` rows and file bytes as before on `tests/fixtures/` (run the
existing `systemone_judge_deletes_reviews_near_thresholds_and_caches_verdicts` unchanged).

### Step 6. `reduce --changed <base>..<head>`

Files: `src/main.rs`, `src/lib.rs` (`run` signature), `README.md` (one paragraph).

```rust
// main.rs, in Opts, under the common flags
/// Judge only comments added or changed in this commit range, and report comments whose code changed
#[arg(long, value_name = "BASE..HEAD", conflicts_with_all = ["eval", "diagnose", "dump_items"])]
changed: Option<String>,
```

`lib::run(root, cfg, changed: Option<&str>)`: with `Some`, `changeset::collect(root,
Some(spec), None, None, cfg.language, &cfg.ignore)`, keep only the files under `root` (the
target may be a subdirectory or one file, and `collect` returns paths relative to the repo
top level), apply the same `Target::Docstrings` Python-only filter `run` applies to `tracked`,
and `reduction::run(..., RunMode { changes: Some(&cs), report_only: false })`. `--changed` with `delete` is an error
(`--changed only applies to reduce`), like `--dry-run` with `reduce`. The final summary line
gains `{} possibly stale`. After the summary, every `Finding` (step 5c) is printed one per line
in the `check` text format (step 7 moves this into `findings::text`).

Tests: `tests/cli.rs` gets one case: `reduce --changed a..b` where `a` does not exist exits
non-zero with `unknown revision` in the message; `delete --changed` is refused;
`reduce --changed` run on a subdirectory leaves a changed file outside it untouched.

Exit check: `reduce --changed HEAD~1..HEAD --judge systemone` on this repo with the mock
System One server from `tools/bakeoff/mock_systemone.py` runs end to end and prints the
tier-2 findings.

### Step 7. `findings.rs`: text, JSON and SARIF

Files: new `src/findings.rs`, `src/lib.rs`, one unit test.

```rust
pub(crate) fn text(f: &Finding) -> String
// "{path}:{start}-{end}: {verdict} ({kind}, {tier}) {message}; {check}={value:.2}/{threshold:.2}, ...; {evidence}"
pub(crate) fn json(findings: &[Finding]) -> serde_json::Value
// an array of objects with the Finding fields; tier and verdict as strings; checks as
// [{"name","value","threshold"}]
pub(crate) fn sarif(findings: &[Finding], version: &str) -> serde_json::Value
```

SARIF 2.1.0, the minimum GitHub code scanning accepts:

```json
{"version": "2.1.0",
 "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
 "runs": [{"tool": {"driver": {"name": "commentreducr", "version": "...",
            "rules": [{"id": "comment/delete", "shortDescription": {"text": "..."}}, ...]}},
           "results": [{"ruleId": "comment/delete", "level": "warning",
                        "message": {"text": "<the text line without the path prefix>"},
                        "locations": [{"physicalLocation": {
                            "artifactLocation": {"uri": "src/a.py"},
                            "region": {"startLine": 8, "endLine": 10}}}],
                        "properties": {"tier": "added", "kind": "inline",
                                       "checks": [...], "evidence": "..."}}]}]}
```

Rule ids are `comment/<verdict>` with the disposition string `Finding.verdict` carries
(`items.result_type`: `delete`, `reduce`, `review`, `rename`, `stale`, `stale?` becomes
`possibly-stale`, and `bloat` for the legacy classifier finding). These are what a cached
record holds; the policy verdict names (`rewrite_shorter`, `fix_wrong_claim`) are not
available for cached or legacy records. `level` is `warning` for `delete`, `reduce` and
`stale`, and `note` for the rest.
`rules` lists only the ids that appear. `uri` is the path relative to the repo root with `/`
separators.

Test: `findings::tests::sarif_shape`: two findings → the JSON has `runs[0].results.len() == 2`,
each result has `ruleId`, `level`, `message.text`, `locations[0].physicalLocation.region.startLine`,
and `rules` has exactly the two ids used. No SARIF validator is added. That these fields are
enough for GitHub's upload is inferred from GitHub's documented required properties, not
measured; the step 9 README example is the first real upload, and a rejection there is fixed
in this file.

Exit check: `cargo test` green.

### Step 8. `check` through the judge path, for all languages

Two commits. 8a: extract the `Config`-building block of `main()` (defaults → file → flags) into
one function that both `reduce` and `check` call, and add `--changed` and `--format` to the
legacy path; `changed_blocks` gains a `range: Option<&str>` parameter passed to `collect`. 8b:
the judge path below. `prepush_check` builds no `Config` today (it reads only `ignore`,
`keep_decorators`, `keep_bases` and `database` from the file), so 8a is a real refactor of
`main.rs` and gets its own reviewable diff.

Files: `src/main.rs` (`HookOpts`, `prepush_check`, the config builder), `src/hook_git.rs`
(`changed_blocks` is kept for the legacy path only), `tests/hook.rs`, `README.md`.

```rust
// HookOpts gains
/// Check this commit range instead of the push (or upstream..HEAD)
#[arg(long, value_name = "BASE..HEAD", conflicts_with = "remote_name")]
changed: Option<String>,
/// Output format [default: text]
#[arg(long, value_enum, default_value_t = Format::Text)]
format: Format,                // Text | Json | Sarif
```

No `--output` flag: `--format sarif > results.sarif` does the same, and the summary goes to
stderr whenever the format is not text. No `--judge` flag on `check` either: the judge comes
from the config file, so the hook and a manual `check` always agree.

`prepush_check` reads the full config (same layering as `reduce`: defaults → file → the
`--config`/`--database` flags it already has) and branches on `cfg.judge`:

- `Legacy` (shipped default): today's classifier path, unchanged, Python only. Its findings are
  converted to `Finding { verdict: "bloat", tier: Added|Modified, checks: [("classifier", p,
  threshold)], .. }` so `--format json|sarif` work here too. The summary line adds
  `N non-Python blocks not checked (set judge = systemone to check them)` when the changeset
  had any.
- `SystemOne` / `Generative`: `changeset::collect` with the same `input`/`remote`/`--changed`
  handling, then `reduction::run(.., RunMode { changes: Some(&cs), report_only: true })`.
  Findings come from `stats.findings`. The judge's `check()` preflight failing is a check
  error (`could not complete pre-push check: ...`), which keeps the hook's exit 0 under
  `--warn` and exit 1 without.

Output: text findings one per line on stdout as today, then the summary line
`commentreducr: {} changed blocks checked; {findings} findings; {errors} check errors.` with
two new counts, `{tier2} possibly stale` and `{skipped} untouched`. `--format json|sarif`
writes the document to stdout and the summary to stderr, so the stream stays valid JSON. Exit code policy is unchanged: non-zero on findings or errors unless
`--warn`.

The database: `check` already opens the state database; with the judge path it now also stores
`items`/`states` rows for the blob's source hash. Nothing is written to source files
(`report_only`, which also skips `resume`; see 5c), and `tests/hook.rs`'s dirty-working-tree
assertion proves it. Python tier-1 items are still screened, so the judge path needs the MiniLM
model; `install-git-hook` already installs it, and otherwise `reduction::run` downloads it on
first use as it does for `reduce`.

Stdout must carry only the document under `--format json|sarif`. Three things on the run path
print to stdout today: `install_model`'s `Downloading MiniLM-L12` line, the per-file
`summary_line` through `Progress::print`, and the `--verbose` per-item lines. 8b moves the
download line to stderr (one-word change), and `report_only` prints no per-file summary line
(the findings replace it). The new hook test parses stdout as JSON to prove it.

Tests in `tests/hook.rs`: the two existing tests unchanged (legacy default). One new test:
a repo with a Python and a TypeScript change and a `--config` file setting `judge =
"systemone"` and `judge_endpoint` to the mock, `check --changed a..b --format sarif` against
the `Mock` server (moved to `tests/common/mod.rs` in step 5) exits 1 with a
SARIF document on stdout containing one result per language, and `--warn` exits 0 with the
same document; the working tree is unchanged.

Exit check: `cargo test` green; `check --warn` on this repo with `judge = legacy` prints the
same lines as before this phase.

### Step 9. `install-git-hook` upgrade, docs, decision log

Files: `src/hook_git.rs` (`install`), `src/main.rs` (`install_hook`), `README.md`,
`DESIGN.md`, `docs/PLAN.md`, `src/default_config.toml` (comments only).

The hook script does not change: it already runs `commentreducr check --warn "$@"`, and
`--warn` is what keeps it warning-only, so every user who installed v2 gets changeset checks
by upgrading the binary. What changes in `install`:

- Managed-hook detection matches the prefix `#!/bin/sh\n# commentreducr pre-push hook v` and
  any version number, instead of listing `v1` and `v2`, so a future header bump reinstalls
  without another edit here. The header stays `v2`, because reinstalling an identical script
  is the existing no-op.
- `install_hook` prints, after the installed path, which judge the hook will use
  (`judge = legacy: Python only, classifier` or `judge = systemone: all languages`) and, for
  legacy, the one config line that switches it. It still calls `install_model()` so the
  legacy path works offline.

Docs:

- `README.md` "Git hook and checks": `check` covers all languages when a judge is configured;
  `--changed`, `--format`, `--output`; the three tiers in three sentences; an example of
  uploading SARIF in a GitHub workflow (`github/codeql-action/upload-sarif`).
- `DESIGN.md`: a "Changeset mode" section describing what exists (tiers, `state.change`,
  report-only path, findings formats), replacing "This checker remains Python-only".
- `docs/PLAN.md`: Phase 4 row's deliverable column lists what landed; decision log entry with
  the date and the exit measurement status (below).
- `AGENTS.md`: no change (the eval instruction is unaffected).

Tests: `hook_git::tests::install_recognises_any_managed_version` writes a `v1` header file,
calls `install`, asserts the backup is not created and the header is current.

Exit check: `cargo test` green; `install-git-hook` on a repo with a v1 hook upgrades it;
`git diff --stat` shows no change under `src/model/`, `src/hook_model.rs` or `src/state.rs`
across the whole phase.

---

## Exit measurement (not an implementation step)

Phase 4's exit criterion in `docs/PLAN.md` is: report-only runs on 30 real agent-authored PRs
with no false deletes in tier 2. By construction (step 5b) tier 2 cannot produce a delete, so
the measurement is about false *stale* findings, which would have been deletes under a policy
that trusted G1 alone:

- Pick 30 PRs from the Phase 1 AIDev sample, from repositories not used in the gold set, with
  at least one tier-2 item each. Clone each at the PR head with full history.
- Run `check --changed <merge-base>..<head> --judge systemone --format json --output
  <pr>.json` with the Phase 3 judge, and `--judge generative` for the escalation comparison if
  configured.
- Check every tier-2 finding (`stale` and `possibly-stale`) by hand: is the comment actually
  contradicted by the after-code? Record per PR: tier-1 items, tier-2 items, tier-2 findings,
  true stale, false stale, and the comment-lines-added per code-line-added ratio.
- Blocker: any `stale` finding (the kind that would reach `FixWrongClaim`) on a comment that
  is still correct. Target for `possibly-stale`: precision ≥ 0.5, since those are notes, not
  edits.
- Record the table and the decision in `docs/PLAN.md`'s log and, if the numbers are good,
  this is also when `judge` in `default_config.toml` can change (a Phase 3 exit decision that
  Phase 4's data informs).

The PR list and the per-item notes contain other people's code and stay under `private/`
(gitignored); only the counts go in the repo.

## Out of scope or deferred

- Per-commit agent attribution inside a mixed range (step 4 shortcut).
- Editing in `check`: never; `check` is report-only by definition.
- A `--changed` form that compares against the working tree (`--changed main`): the write
  path needs the head blob to equal the file on disk, so uncommitted changes are skipped with
  a warning. Add a working-tree head only if the skip is a real nuisance.
- Deleting tier-2 comments on a confirmed contradiction in strict mode: `docs/PLAN.md` says
  "edit only on a confirmed contradiction" and the verified rewrite is that edit; a delete
  would need the 30-PR numbers first.
- The per-PR "comment lines added per code lines added" summary line from Proposal 3: cheap,
  but a report format question; add it to the summary line once the findings formats have
  settled.
- `rename_instead` suggestions with a proposed identifier in the finding message: needs the
  generator to propose a name, which is the open rename question from Phase 3.
- Posting review comments to GitHub directly: SARIF upload covers inline display; anything
  else is a separate tool.
- SARIF `fixes` (the proposed rewrite as a code-scanning autofix): the verified candidate is
  available for tier 1 in `items.edit`; wire it into SARIF only if someone asks for it.
- `check --output FILE`: shell redirection does the same (step 8).

## Review notes

Review of the first draft of this plan against the code on `redesign-p23` and `docs/PLAN.md`
Phase 4. Each item below was fixed in the step named.

Paths that could write or delete when they must not:

1. `Database::resume` calls `atomic_write` when it finds a `prepared` write whose before-hash
   matches the source. The draft's report-only path still went through it, so a `check` after
   an interrupted `reduce` could write to the working tree. Report-only now skips `resume`
   (5c, test h).
2. `items` rows are keyed without the tier, and the tier of the same bytes depends on the
   range. A full-schema `delete` cached by a whole-tree run, a failed run or another range's
   `check` would be reused for a tier-2 item and applied. Tier-2 items now use their own
   `llm_key`, and the write loop refuses a tier-2 `delete` (5a, test f).
3. The MiniLM screener runs on every Python item, whatever the judge. A tier-2 comment it
   passes would be stored `keep` without a stale check. Tier 2 now bypasses it (5a).
4. Legacy runs compute no states, so the draft's tiers did not exist under `judge = legacy`,
   and a flagged tier-2 item would have reached `item.reduce`, which can delete. States are now
   computed whenever a changeset is given, and legacy tier 2 never becomes a job (5a, 5b).
5. `stale_verdict` had no margin: a g1 probability just over the threshold went straight to the
   rewrite path. It now uses `th.margin` as `decide` does (5b).

Wrong assumptions about existing code:

6. `plan_states` blames the working-tree file. In changeset mode the items come from the head
   blob, so blame lines, ages and agents were wrong on a dirty tree or a non-checked-out branch.
   Blame now runs at the head revision (step 4).
7. Moving tier-3 items out of `plan.items` without removing their `states` entries would
   misalign `states[index]` with `plan.items[index]` (5a).
8. `root.join(f.path)` is wrong when `reduce` is given a subdirectory or a file; changeset paths
   are relative to the repo top level (step 1, 5a, step 6).
9. A cached record has no policy numbers, so findings for cached verdicts had no checks. The
   checks now go into the stored ledger and are read back (5c). `Finding.verdict` is the
   disposition, because that is what a cached or legacy record holds (5c, step 7).
10. The draft said findings were pushed only in report-only mode, but step 6 printed them after
    a write run. They are now pushed in both modes (5c).
11. The draft skipped non-UTF-8 head blobs with a warning while also promising the legacy
    `check` output is unchanged; today it is an error. It stays an error (step 1).
12. The fixture expected a trailing comment on a changed line to be untouched, but rule 2 makes
    it tier 2, which is the right answer (step 2).
13. `prepush_check` builds no `Config`, `check` has no `--judge` flag, and `changed_blocks` takes
    no range. Step 8 is split so the config refactor is its own commit, and its test uses a
    config file.
14. Under `--format json|sarif`, the model download line, per-file summary lines and verbose
    lines would land on stdout and corrupt the document (step 8).
15. A tier-2 rewrite that turns out to be a no-op was downgraded to `keep` by the existing rule,
    hiding the finding; it is now `stale` (5b).

Steps too large or out of order:

16. Step 5 was one commit in practice (plumbing, report-only, findings, policy). It is now 5a,
    5b and 5c, and 5a alone is safe because tier 2 is skipped until 5b.
17. Step 2 needed `plan_source`, which the draft defined in step 5; it moved to step 2.
18. Step 2 used the innermost scope, which for Rust `impl`/`mod tests` and Python classes spans
    most of a file and would make most comments tier 2 on any edit. `scope_lines` is now
    function-like scopes only (step 2).

Scope cut: `--output` (redirection does the same). Kept: everything `docs/PLAN.md` Phase 4
lists. Not checked: SARIF acceptance by GitHub (inferred from GitHub's documentation, first
measured by the step 9 example); hook latency with tier-2 judge calls and the generator in
`check`, which the 30-PR run will measure.

## Review notes after the 30-PR validation

The exit check failed: 79 of 83 tier-2 findings flagged accurate comments
(`docs/decisions/changeset-validation.md`). The cause was treating "code near the comment
changed" as "the comment may be stale". Tier 2 was redesigned:

1. A deterministic gate runs first (`evidence::changed_mentions`). An item is a candidate only
   when the change touched something the comment names: an identifier-shaped mention
   (backticked, dotted, snake or camel case, `f()`) that is an identifier on a line of the
   before-version's annotated code plus enclosing scope that the new file no longer has
   (whitespace ignored, case and `_` folded), or a number or quoted string that the before
   text had and the after-version's annotated code plus scope lacks. Kept lines are looked up
   in the whole new file because the scope is cut to 80 rows: an insertion pushes lines out of
   the window without removing them. The scope is included because tier 2 fires on scope
   changes and the judge sees the scope: a comment naming a constant used two lines below
   its statement was otherwise never a candidate.
   It is stored in `state.change.changed_mentions`. With no match the item is kept without a
   judge call.
2. Gated items get one question, `g1_contradicts`, now about the after-code only
   (`schema/stale-v2.json`). `g1_unverifiable` and `c8_durability` are gone: both are high
   for most good comments.
3. Tier 2 never edits, in any mode: the verified-rewrite path for tier 2 is removed and the
   write loop refuses any tier-2 verdict other than `keep` or `stale` with no edit. `stale`
   needs P(g1_contradicts) > `stale_g1` (0.5). The `stale?` / `possibly-stale` band is
   removed, since it had precision 0.03 against a target of 0.5. The legacy judge has no stale
   question, so legacy tier 2 is always kept.
4. A `stale` finding's evidence is `changed: <tokens>`; SARIF rule ids are `comment/<verdict>`.

Known gap: plain-word claims never pass the gate ("returns the sum" above `a + b` changed to
`a - b`). Add a claim-level check only if validation shows missed stale comments.

Re-score of the 83 findings with the gate, using the before and after blocks stored by the
validation run: 1 is still a candidate (`fold_constants.rs:359`, which names `tryFoldAdd`,
whose signature changed; its old g1 was 0.04, under the threshold). None of the 4 findings
that were not false survive; none of them was made stale by its PR.

Gate measured offline (no judge), after widening it to the enclosing scope:

| Set | Gate passes, annotated code only | Gate passes, with scope |
|---|---|---|
| 30 mutation stale cases (27 reach tier 2) | 12 | 15 |
| 30 mutation fine cases (24 reach tier 2) | 0 | 1 |
| 83 real tier-2 findings (none made stale by its PR) | 1 | 1 |

The 12 stale misses are 11 inverted conditions and one rename that left the old name in the
scope, all described in plain words; that is the known gap above. The 3 that never reach tier 2
are structural (`TODO`, `SAFETY`) and are never judged. The one fine candidate is a `uv` local
rename on a line whose string literal holds `main()`; it still needs the judge to score it
above `stale_g1` (not measured, the judge server was busy). The real candidate is still only
`fold_constants.rs:359`.

