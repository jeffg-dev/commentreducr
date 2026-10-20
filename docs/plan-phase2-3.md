# Implementation plan: Phases 2 and 3 of the `reduce` redesign

Audience: the implementer (a Sonnet-class agent working one step at a time) and the maintainer
reviewing each PR. Background is in `docs/reduce-redesign.md` (Proposals 1-3) and
`docs/PLAN.md`. This file is the ordered list of commits, with the exact types, signatures,
tests and exit checks for each.

Rules for every step:

- One step is one commit and one reviewable diff. Keep `cargo fmt && cargo clippy --all-targets
  -- -D warnings && cargo test` green at every commit. Python tooling tests stay green too:
  `python3 -m unittest tools/test_bakeoff_policy.py tools/bakeoff/test_render.py tools/bakeoff/test_run_judge.py`.
- No new crates. `minreq`, `serde_json`, `sha2`, `regex`, `rusqlite` and the five tree-sitter
  grammars already present are enough.
- The parse-validate-atomic-write path in `reduction::run` and `lib::atomic_write` is not
  touched. Every new verdict still becomes an `rewrite::Edit` that goes through `rewrite::apply`,
  the re-parse check, the source-hash check, `Database::prepare`, `atomic_write` and
  `Database::complete`, exactly as today.
- Private data never enters the repo. Tests use synthetic fixtures under `tests/fixtures/`.
  Scripts that read the gold set (`private/`, gitignored) take the path as an argument.
- The MiniLM screener (`hook_model.rs`, `classifier_key`, the `classifications` table) is not
  changed. Step 1 proves this with a snapshot test.
- The old prompt path (`LlmClient::summarize`, `rewrite_docstring`, `DEMOS`, `DOC_DEMOS`,
  `Item::reduce`) stays and stays selectable as `judge = "legacy"` until Phase 3 exit.

Definitions used below:

- "v2 row": one line of a dataset in the format of `tools/dataset/v2/SCHEMA.md`.
- "state": the text block a judge reads, produced by `tools/bakeoff/render.py::render_state`
  in Python and, after step 6, by `render::render_state` in Rust. Both must be byte-identical.
- "answers": `{question_id: {option: probability}}`, the shape every bake-off judge returns.

## Order of work at a glance

| Step | Phase | Commit title | Depends on |
|---|---|---|---|
| 1 | 2 | Move `context_for_range` to `context.rs`, generalise over languages | - |
| 2 | 2 | `context::ItemContext`: kind, placement, annotated code, scope, symbol (2a Python, 2b other languages) | 1 |
| 3 | 2 | `--dump-items` emits context; `tools/dataset/v2/parity.py` | 2 |
| 4 | 2 | `evidence.rs`: text evidence | 2 |
| 5 | 2 | `evidence.rs`: git blame ages and agent authorship | 4 |
| 6 | 2 | `render.rs`: state renderer with parity fixtures; schema moves to `src/schema/` | 2 |
| 7 | 2 | `states` table and `--dump-items` state field | 3, 5, 6 |
| 8 | 3 | `judge.rs`: `Judge` trait and `SystemOne` backend | 6 |
| 9 | 3 | `policy.rs` mirroring `policy.py`; thresholds JSON; shared case fixtures | - |
| 10 | 3 | Config keys and fingerprint | 8, 9 |
| 11 | 3 | Judge path in `reduction::run` (keep / delete / review / rename), ledger (11a); review counts in stats (11b) | 7, 10 |
| 12 | 3 | `Generative` backend (frontier checklist prompt) | 11 |
| 13 | 3 | Verified rewrite path | 11 |
| 14 | 3 | Escalation | 13 |
| 15 | 3 | Eval v2: run files (15a), Rust report (15b) | 13 |
| 16 | 3 | Docs, exit measurement, decision log | 15 |

Steps 4-5 and 6 are independent of each other; 8 and 9 are independent of each other and of 7.

---

## Phase 2: state and evidence extraction in Rust

### Step 1. Move `context_for_range` into `src/context.rs` and generalise it

Why first: the enclosing-scope text already exists for Python (`hook_git::context_for_range`)
and feeds the MiniLM classifier key. Generalising it must not change one byte of the Python
output, or every cached classification and the hook's verdicts would shift.

Files: new `src/context.rs`; edit `src/hook_git.rs`, `src/reduction.rs`, `src/lib.rs`,
`src/parse.rs`.

New code in `context.rs`:

```rust
/// Node kinds that count as an enclosing scope, and the field that holds the body, per language.
pub(crate) struct ScopeKinds { pub kinds: &'static [&'static str], pub body_field: &'static str }
pub(crate) fn scope_kinds(lang: Language) -> ScopeKinds
// Python: ["function_definition", "class_definition"], body "body"   (unchanged behaviour)
// JS/TS/TSX: ["function_declaration", "generator_function_declaration", "method_definition",
//            "arrow_function", "function_expression", "class_declaration", "class"], body "body"
// Rust: ["function_item", "impl_item", "trait_item", "struct_item", "enum_item", "mod_item"], body "body"
// YAML: [] (always module level)

/// Moved verbatim from hook_git; `lang` picks the scope kinds. Returns the same String as before.
pub(crate) fn context_for_range(source: &str, tree: &tree_sitter::Tree, lang: Language,
    start: usize, end_byte: usize, start_line: usize, end_line: usize) -> String
```

`parse::parse_tree` becomes `pub(crate)` so callers stop building their own parser (the
`tree_sitter::Parser` block in `reduction.rs` lines 65-75 is replaced by
`parse::parse_tree(&plan.source, lang)?` for every language, not only Python). `hook_git::context`
calls the moved function with `Language::Python`.

The classifier input does not change for any language: `reduction::run` still passes `""` as
the context to `classifier_key` for non-Python items, exactly as today (`unwrap_or_default()` on a
`None` tree). Feeding the new JS/TS/Rust scope text into `classifier_key` would change the
`items.classifier_key` column, which is part of the `items` UNIQUE key, and silently invalidate
every cached non-Python verdict. The generalised scope is for `ItemContext` (step 2) only.

Tests:

- `context::tests::python_context_unchanged`: for `tests/fixtures/sample.py` and
  `tests/fixtures/test_sample.py`, run the old algorithm (copy the pre-move function body into the
  test as `old_context_for_range`) and the new one on every item from `plan_file`, assert equal.
  Delete the old copy in step 2 once the snapshot JSON exists.
- Existing `tests/hook.rs` and `tests/reduce.rs` pass unchanged.

Exit check: `cargo test` green; `git diff --stat` shows no change in `hook_model.rs` or
`state.rs`; the `classifier_key(...)` call in `reduction.rs` still receives `""` for non-Python.

### Step 2. `context::ItemContext`

Two commits: 2a adds the types, the rules and the Python fixtures (`sample.py`, `scopes.py`);
2b adds the JS/TS/TSX/Rust/YAML scope kinds' fixtures. Each fixture JSON is read by eye in review,
and seven at once is too many for one PR.

Files: `src/context.rs`, `src/lib.rs` (`Plan`), `src/docstring.rs` (one accessor), new fixtures
under `tests/fixtures/context/`.

New types (all `serde::Serialize` so step 3 and step 7 can dump them):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommentKindV2 { Inline, ApiDoc, Todo, Directive }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement { BeforeStatement, Trailing, FunctionHead, ClassHead, ModuleHead }

#[derive(Debug, Clone, Serialize)]
pub struct Symbol { pub name: Option<String>, pub public: Option<bool>, pub is_test: bool }

#[derive(Debug, Clone, Serialize)]
pub struct ItemContext {
    pub language: &'static str,          // "python" | "javascript" | "typescript" | "tsx" | "rust" | "yaml"
    pub comment_kind: CommentKindV2,
    pub placement: Placement,
    pub comment: String,                 // raw text with delimiters; see rules below
    pub annotated_code: String,
    pub enclosing_scope: String,
    pub symbol: Symbol,
}

pub(crate) fn item_context(source: &str, tree: &tree_sitter::Tree, lang: Language,
    path: &Path, item: &Item) -> ItemContext
pub(crate) fn language_name(lang: Language) -> &'static str
```

Rules (they are the SCHEMA.md rules made precise for this parser):

- `comment`: for `Item::Comment`, each `Comment.text` trimmed and joined with `"\n"` (what
  `Item::text()` already does); for `Item::Docstring`, `source[d.start..d.end]` (prefix and
  quotes included, not the cleaned text).
- `comment_kind`: `ApiDoc` for a docstring, a Rust `Comment.doc` block, or a JS/TS block whose
  text starts with `/**`; `Todo` when any line matches `structural`'s TODO/FIXME/XXX/HACK/NOTE
  rule; `Directive` when `structural::is_structural` is true for any other reason; else
  `Inline`. In practice only `Inline` and Python `ApiDoc` reach the judge today because
  `plan_file` drops structural blocks; the other values exist for `--dump-items` and the dataset.
- `placement`: `Trailing` when `!own_line`; for docstrings `ModuleHead` / `ClassHead` /
  `FunctionHead` by `DocKind`; otherwise `BeforeStatement`.
- `annotated_code`:
  - `BeforeStatement`: text of the next named sibling of the last comment node that is not
    itself a comment (`node.next_named_sibling()` loop). If there is none (comment ends a
    block), the previous non-comment named sibling. If neither, `""`.
  - `Trailing`: the smallest ancestor of the comment node that starts on the comment's line and
    is not the comment (the statement it trails); fallback the whole line without the comment.
  - Docstring: `d.signature` plus the first 10 body lines after the docstring (reuse
    `docstring::body_preview_and_lines` with a limit parameter of 10 and no 80-char cut; add
    `pub(crate) fn body_lines_after(src, doc, block_end_line, n) -> Vec<String>`). `Docstring`
    does not store the definition's end line, so `item_context` takes it from the tree (the
    docstring node's enclosing `function_definition`/`class_definition`). Keep the lines'
    indentation (the existing helper trims them; add a flag rather than a second loop).
    `ModuleHead`: the first 20 lines after the docstring.
  - In every case the comment's own bytes are not included.
- `enclosing_scope`: `context_for_range(...)` from step 1 (comment removed, header kept, 80-line
  cap with 40 either side at module level). This is the same text the classifier sees for
  Python, which is deliberate.
- `symbol.name`: dotted names of enclosing definitions from outermost to innermost
  (`Cache.set`); `None` at module level. Node kinds: the `scope_kinds` list plus the
  `name` field (Rust `impl_item` uses its `type` field; an impl of `Foo` contributes `Foo`).
- `symbol.public`: Python `Some(!name.starts_with('_'))` on the innermost segment; JS/TS
  `Some(true)` when an `export_statement` ancestor exists, else `Some(false)`; Rust `Some(true)`
  when the innermost definition has a `visibility_modifier` child; YAML `None`.
- `symbol.is_test`: `docstring::is_test_file(path)` or the innermost name matches
  `docstring`'s test-name rule (`test`/`Test` prefix). Reuse the existing helpers.

`Plan` gains nothing; instead add `pub(crate) fn plan_contexts(plan: &Plan, tree:
&tree_sitter::Tree, lang: Language, path: &Path) -> Vec<ItemContext>` in `lib.rs` that maps every
item. It takes the tree rather than parsing, so `reduction::run` parses each file once (step 1)
and reuses that tree for both the classifier context and this call; `dump_items` parses once
itself. The result is kept in a new `FilePlan.contexts` field.

Tests: `tests/fixtures/context/{sample.py,sample.ts,sample.tsx,sample.js,sample.rs,sample.yaml}`
copied from the existing fixtures, plus one new `scopes.py` with a class containing a method
containing a trailing comment, a comment ending a block, a module docstring and an `_private`
function. Expected output `tests/fixtures/context/<name>.json`: the array of `ItemContext` as
JSON. The test `context::tests::fixtures_match` serialises and compares strings exactly. The
implementer generates the JSON once, reads every entry by eye against the rules above, and
commits it; the maintainer reviews the JSON in the PR.

Exit check: fixtures test green; `python_context_unchanged` from step 1 still green.

### Step 3. `--dump-items` emits context; `tools/dataset/v2/parity.py`

Files: `src/lib.rs` (`dump_items`), `src/main.rs` (help text only), new
`tools/dataset/v2/parity.py`, `tests/cli.rs`.

`dump_items` rows gain every `ItemContext` field (serialise the struct flattened next to the
existing `start`/`end`/`start_line`/`end_line`/`kind`/`text`). Existing keys keep their names so
the Phase 1 tooling still reads them.

`parity.py GOLD.jsonl CHECKOUTS_DIR [--bin commentreducr]`: for each gold row, find
`CHECKOUTS_DIR/<repo basename>` checked out at `source.commit` (the script does not fetch; it
prints the rows it cannot find), run `--dump-items` on `source.path`, pick the item whose
`start_line <= source.line <= end_line`, and compare `comment_kind`, `placement`,
`annotated_code`, `enclosing_scope` and `symbol` with the gold row after trimming every line and
stripping trailing blank lines. Print one line per mismatch (`id field`) and a summary
`N rows, M found, K mismatches by field`. No data is written anywhere.

Tests: `tests/cli.rs::dump_items_includes_context` asserts the new keys exist for one Python
and one Rust fixture item. `parity.py` gets no committed test beyond `python3 -m py_compile`
(it needs private data).

Exit check: the maintainer runs `parity.py` on the gold set locally and reports the mismatch
count in the PR. The Phase 2 exit criterion in PLAN.md ("reproduces the Python mining tool's
fields on all 300 gold items") is measured here; mismatches that come from the private
extractor's own choices are listed in the PR and resolved either by changing the Rust rule or by
re-extracting the gold row, decided per field by the maintainer. Note: the Python extractor is
not in this repository, so this script is the only parity check that can be committed.

### Step 4. `evidence.rs`: text evidence

Files: new `src/evidence.rs`, `src/lib.rs`.

```rust
#[derive(Debug, Clone, Default, Serialize)]
pub struct Evidence {
    pub mentions: Vec<Mention>,            // identifier-shaped tokens in the comment
    pub unresolved: Vec<String>,           // mentions found nowhere in scope, file or imports
    pub literal_mismatch: Vec<String>,     // numbers/quoted strings in the comment absent from annotated_code
    pub overlap: Option<f64>,              // token overlap with annotated_code after identifier splitting; None if either side has no tokens
    pub signature_echo: bool,              // api_doc only: content tokens are a subset of the signature's tokens
    pub todo_tracker: Option<bool>,        // todo only: Some(has owner or tracker id)
    // filled by step 5, null until then
    pub comment_age_days: Option<u64>,
    pub code_age_days: Option<u64>,
    pub code_newer_than_comment: Option<bool>,
    pub authored_by_agent: Option<bool>,
    pub agent: Option<String>,
}
#[derive(Debug, Clone, Serialize)]
pub struct Mention { pub token: String, pub resolved: bool }

pub(crate) fn text_evidence(ctx: &ItemContext, source: &str, item_bytes: (usize, usize)) -> Evidence
// item_bytes = Item::range().0..1, needed for "outside the comment's own bytes" below
```

Rules, each one small:

- Identifier-shaped tokens: backticked spans, `snake_case` with an underscore, `camelCase` with
  an interior capital, dotted names, and `name()` calls; words shorter than 3 chars and common
  English words are not identifiers (reuse nothing fancy; a fixed list of 30 stop words is
  enough). Resolved when the token (or its last dotted segment) occurs in `enclosing_scope` or
  in `source` outside the comment's own bytes.
- Literals: integers, decimals with units glued (`300ms`, `5s`), and quoted strings; mismatch
  when the bare number or string does not occur in `annotated_code`.
- Overlap: split identifiers on `_` and case boundaries, lowercase, drop stop words; Jaccard
  overlap `|comment ∩ code| / |comment|` over the comment's tokens (Steidl's coherence, same
  direction as the proposal: "overlap ≥ 0.8 on a one-statement comment → restating").
- Signature echo: for `ApiDoc` only, the comment's tokens after the same split are a subset of
  the tokens of the first line of `annotated_code`.
- TODO: `Some(true)` when the comment matches `@\w+|\b[A-Z]{2,}-\d+\b|#\d+|https?://`.

Nothing here acts on a verdict yet. The evidence is stored (step 7) and shown by `--dump-items`.
Acting on it (deterministic deletes, "policy step 1") is deferred; see the deferred list at the
end.

Tests: unit tests in `evidence.rs` with five literal cases (one per field), no fixtures.

### Step 5. `evidence.rs`: git blame ages and agent authorship

Files: `src/evidence.rs`, `src/hook_git.rs` (reuse `git_text`, make it `pub(crate)`).

```rust
/// Per-file blame: 0-based line -> (commit sha, author time in unix seconds). None when the
/// file is untracked, the repo is missing, or git fails for any reason.
pub(crate) struct Blame { lines: Vec<Option<(String, i64)>> }
pub(crate) fn blame(repo: &Path, path: &Path) -> Option<Blame>      // one `git blame --line-porcelain` per file
pub(crate) fn agent_of(repo: &Path, sha: &str, cache: &Mutex<HashMap<String, Option<String>>>) -> Option<String>
pub(crate) fn git_evidence(ev: &mut Evidence, blame: &Blame, ctx: &ItemContext, item_lines: (usize, usize),
    annotated_lines: Option<(usize, usize)>, now: i64, repo: &Path, cache: &…)
```

- `comment_age_days`: now minus the newest author time among the comment's lines.
  `code_age_days`: the same over the annotated code's lines (step 2 must also record the
  annotated node's line range on `ItemContext` as `annotated_lines: Option<(usize, usize)>`,
  serialised but excluded from the state).
  `code_newer_than_comment = Some(code_age < comment_age)`.
- `agent`: from `git log -1 --format=%B%n%ae%n%ce` of the comment's newest commit: a
  `Co-Authored-By:` trailer (matched case-insensitively; GitHub writes `Co-authored-by`) naming
  Claude, Cursor, Copilot, Devin, Codex or OpenAI, or an author/committer email at
  `anthropic.com`, `cursor.sh`, `cursor.com`, `devin.ai`, `openai.com`, or a GitHub noreply
  address whose local part contains `copilot`. Never match the bare `github.com` domain:
  `noreply@github.com` is the committer of every merge and squash made in GitHub's web UI, so it
  would mark most human commits as Copilot's. Value is the lower-case tool name (`claude-code`,
  `cursor`, `copilot`, `devin`, `codex`); `authored_by_agent = Some(agent.is_some())` when the
  commit is known, else `None`. Use the same rule as the Phase 1 mining tool if it differs, so
  the gold rows and the Rust output agree.
- The repo for `blame`/`agent_of` is `git rev-parse --show-toplevel` run from the file's parent
  directory (the `root` argument may be a file or a subdirectory), resolved once per run.
- Any git failure (no `.git`, shallow clone, untracked file, non-UTF-8 output) leaves the
  fields `None`. Never an error.

Blame is computed once per file in `reduction::run`. Until step 10 it always runs (the state
is inspection only); step 10 gates it, with the rest of the context/evidence/state work, on
`cfg.judge != Legacy`.

Tests: one test in `tests/reduce.rs`-style temp repo (the `Repo` helper there) with two commits,
one carrying a `Co-Authored-By: Claude <noreply@anthropic.com>` trailer; assert `agent ==
Some("claude-code")`, ages are `Some`, and a file written but not committed gives all `None`.

Exit check: `reduce` on a non-git directory (`--database` pointed elsewhere) still works with
nulls.

### Step 6. `render.rs`: the state renderer, byte-identical to `render.py`

Files: new `src/render.rs`; move `tools/bakeoff/schema/strict-v0.json` to
`src/schema/strict-v1.json` (content unchanged except `"version": "strict-v1"`); edit
`tools/bakeoff/render.py` (`DEFAULT_SCHEMA` points at `../../src/schema/strict-v1.json`),
`tools/bakeoff/judges/systemone.py` (same), `tools/bakeoff/run_judge.py` (`--schema` help
text), `tools/bakeoff/test_render.py`; new fixtures under `tests/fixtures/render/`.

The rendered state is the bake-off input. Thresholds tuned on it only transfer if the Rust
output is the same bytes. Port `render.py` line by line, including its Python-isms:

```rust
pub const MAX_STATE_TOKENS: usize = 3000;
pub const SCHEMA_STRICT: &str = include_str!("schema/strict-v1.json");

/// Input row for the renderer: the fields render.py reads, as Options so missing keys print like
/// Python's None. Built from ItemContext + Evidence (step 7) or from a v2 dataset row (step 15).
pub struct StateRow<'a> {
    pub language: Option<&'a str>, pub comment_kind: Option<&'a str>, pub placement: Option<&'a str>,
    pub comment: &'a str, pub annotated_code: &'a str, pub enclosing_scope: &'a str,
    pub symbol_name: Option<&'a str>, pub public: Option<bool>, pub is_test: Option<bool>,
    pub authored_by_agent: Option<bool>, pub agent: Option<&'a str>,
    pub comment_age_days: Option<i64>, pub code_newer_than_comment: Option<bool>,
}
pub fn render_state(row: &StateRow, max_tokens: usize) -> String
pub type Questions = Vec<(String, serde_json::Value)>;      // schema file order, see detail 7
pub fn questions_for(schema: &str, comment_kind: Option<&str>) -> Questions
pub(crate) fn py_str_bool(b: Option<bool>) -> &'static str   // "True" | "False" | "None"
pub(crate) fn py_splitlines(s: &str) -> Vec<&str>            // Python str.splitlines() boundaries
pub(crate) fn estimate_tokens(s: &str) -> usize               // s.chars().count() / 4, not bytes
```

Details that matter for byte parity (each one is a fixture case):

1. Header line: `language: {l}  comment_kind: {k}  placement: {p}` with two spaces; a missing
   value prints `None`.
2. Symbol line: `symbol: {name}  public: {True|False|None}  is_test: {True|False|None}`.
3. Evidence block only when at least one of the four keys is non-null, in the fixed order
   `authored_by_agent, agent, comment_age_days, code_newer_than_comment`, one `k: v` per line,
   bools as `True`/`False`.
4. Sections joined by `"\n\n"`; the scope section is `enclosing_scope:\n{scope}` and comes last.
5. Token estimate is `len(text) // 4` where `len` counts Unicode scalar values.
6. Scope trimming: `_scope_window` uses `splitlines()` (Python boundaries, including `\r`,
   `\x0b`, `\x0c`, `\x1c`-`\x1e`, `\x85`, ` `, ` `), finds the first non-blank
   annotated line by substring (`first in ln`), falls back to `len(lines) // 2`, centres with
   `len(annotated.splitlines()) // 2`, adds `...` markers, and binary-searches `half` in
   `[0, len(scope.splitlines())]` with `best = build("...")` as the floor. Port the loop
   exactly; do not "improve" it.
7. `questions_for`: keep only `type`, `instructions`, `criteria` per question; drop a question
   whose `applies_to` value is not the row's `comment_kind`. Keep the schema file's question
   order. Python sends questions in file order (dicts keep insertion order); this crate's
   `serde_json` is built without `preserve_order`, so a `serde_json::Map` would sort them
   alphabetically and the server would see a different request from the one the thresholds
   were tuned on. Parse `"questions"` with a small `serde::de::Visitor` into `Questions` (no new
   crate, no new feature) and write the request body's `questions` object from that `Vec` by
   hand. The parity test compares key order too, not only the parsed values.

Fixtures: `tests/fixtures/render/cases.json`, an array of `{name, row, max_tokens}` with at
least: a plain row; a row with every optional field null; a row with evidence; a row whose
scope needs trimming (the 400-line scope from `test_render.py`); a row with a ` ` and a
`\r\n` in the scope; a row with multi-byte characters so the token estimate differs from a byte
count; an `api_doc` row and a `todo` row for `questions_for`. `tools/bakeoff/test_render.py`
gains `test_write_rust_fixtures` that renders every case with the Python code and writes
`tests/fixtures/render/<name>.state.txt` and `<name>.questions.json`; those files are committed.
`render::tests::parity` reads the same cases, renders in Rust and asserts `==` on the state
string and on the parsed questions JSON.

Exit check: both test suites green; `git diff tools/bakeoff/schema/` shows only the move; the
paraphrase schema stays under `tools/bakeoff/schema/` (it is a test input, not a product file).

### Step 7. `states` table and the state in `--dump-items`

Files: `src/state.rs`, `src/reduction.rs`, `src/lib.rs`.

Schema change, created with `CREATE TABLE IF NOT EXISTS` so no migration code is needed; bump
`PRAGMA user_version` to 2 and accept `version <= 2`:

```sql
CREATE TABLE IF NOT EXISTS states (
    item_id INTEGER PRIMARY KEY,   -- items.id
    state TEXT NOT NULL,           -- rendered state string
    context TEXT NOT NULL,         -- ItemContext as JSON
    evidence TEXT NOT NULL,        -- Evidence as JSON
    answers TEXT,                  -- step 11: {qid: {option: p}} as JSON
    verdict TEXT,                  -- step 11: policy verdict incl. "review"
    judge TEXT                     -- step 11: backend and model that answered
);
```

```rust
impl Database {
    pub(crate) fn store_state(&self, item_id: i64, state: &str, context: &str, evidence: &str) -> Result<()>  // INSERT OR REPLACE
    pub(crate) fn store_answers(&self, item_id: i64, answers: &str, verdict: &str, judge: &str) -> Result<()> // step 11
}
```

`reduction::run` builds, per item, `ItemContext` (step 2) and `Evidence` (steps 4-5), renders
the state (step 6) and calls `store_state` right after `database.item(...)`. `dump_items` prints
`evidence` and `state` fields too (it has no database; it computes them in memory). `--dump-items`
is now the complete inspection tool for one file; `--diagnose` keeps its current meaning
(redacted parse-error report), which is a deliberate deviation from PLAN.md: a redacted state
is not useful to read, and dump-items already exists.

Tests: extend `state::tests` to store and read back one state; `tests/cli.rs` asserts the
`state` key starts with `language: python`.

Exit check: Phase 2 done. Record in `docs/PLAN.md`'s decision log the parity result from step 3
and the date.

---

## Phase 3: judge backend, policy, verified rewrite

### Step 8. `judge.rs`: the `Judge` trait and the `SystemOne` backend

Files: new `src/judge.rs`, `src/lib.rs`.

```rust
pub type Answers = BTreeMap<String, BTreeMap<String, f64>>;   // qid -> option -> probability

pub struct Judgement {
    pub answers: Answers,
    pub holistic: Option<String>,     // argmax of the `verdict` question, if asked
    pub input_tokens: Option<u64>,
    pub latency_ms: u64,
}

pub trait Judge: Sync + Send {
    fn judge(&self, state: &str, questions: &render::Questions) -> Result<Judgement>;
    /// True when answers are one-hot and the policy must be bypassed (policy.py GENERATIVE_JUDGES).
    fn one_hot(&self) -> bool { false }
    fn name(&self) -> String;         // "systemone:clef-flash-4bit", for the ledger and progress
}

pub struct SystemOne { url: String, model: String, api_key: Option<String>, pub tokens: TokenTotals,
                       truncation_warned: AtomicBool }
impl SystemOne {
    pub fn new(url: &str, model: &str, api_key: Option<&str>) -> Self
    pub fn check(&self) -> Result<()>  // one request with a one-question schema and state "hi"
}
```

Request body: `{"model", "state", "questions"}` to `url` (the full `/v1/systemone` URL), with
`questions` written in `Questions` order (step 6, detail 7).
Response conversion is `judges/systemone.py::convert` exactly: `noul` → `{"true": p, "false":
1-p}`; `choice`/`score` → `probabilities` copied as-is (score keys are `"0".."3"`). Read
`usage.input_tokens`. Transport: `minreq` with a 120 s timeout, one retry on transport error,
bearer header when a key is set, same shape as `LlmClient::post`. When
`input_tokens < estimate_tokens(state) / 2` warn once per process: "state truncated by the
server; raise its max_length or lower MAX_STATE_TOKENS".

Tests: `judge::tests::systemone_round_trip` starts a `TcpListener` thread in the test (copy the
tiny HTTP helper from `tests/reduce.rs` into a `#[cfg(test)]` module, or move that helper to
`src/testing.rs` behind `#[cfg(test)]`), replies with the `mock_systemone.py` shape, and asserts
the converted `Answers`, `holistic`, and the truncation warning path (reply `input_tokens: 1`
for a long state).

### Step 9. `policy.rs` mirroring `policy.py`, with thresholds in an embedded JSON

Files: new `src/policy.rs`, new `src/schema/thresholds-strict-v1.json`, new
`tests/fixtures/policy/cases.json`, edit `tools/bakeoff/policy.py` (load defaults from that JSON)
and `tools/test_bakeoff_policy.py`.

`thresholds-strict-v1.json` holds exactly `policy.py::DEFAULT_THRESHOLDS` (same keys, same
values, including `cap_lines`, `margin`, `use_holistic`) plus three keys for step 13:
`verify_preserves: 0.8`, `verify_adds: 0.2`, `verify_g1: 0.1`. `policy.py` loads
`DEFAULT_THRESHOLDS` from that file (`json.load(open(SRC_SCHEMA / "thresholds-strict-v1.json"))`)
so Python and Rust cannot drift. `tune.py --out` keeps writing the same key set; when the
bake-off picks tuned values, they are copied into this JSON in one commit and the fingerprint
changes with it.

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct Thresholds { pub g1: f64, pub api_delete: f64, pub api_rewrite: f64, pub c5_min: f64,
    pub c2_min: f64, pub rename: f64, pub hazard: f64, pub c3_min: f64, pub leak: f64,
    pub cap_lines: usize, pub margin: f64, pub use_holistic: bool,
    pub verify_preserves: f64, pub verify_adds: f64, pub verify_g1: f64 }
pub const THRESHOLDS_STRICT: &str = include_str!("schema/thresholds-strict-v1.json");
impl Thresholds { pub fn strict() -> Thresholds }   // serde_json::from_str(THRESHOLDS_STRICT).expect(...)

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict { Keep, RewriteShorter, Delete, RenameInstead, FixWrongClaim, Review }
impl Verdict { pub fn as_str(self) -> &'static str; pub fn parse(s: &str) -> Option<Verdict> }

/// policy.py::decide. `comment_kind` and `comment` are the row fields it reads.
pub fn decide(answers: &Answers, comment_kind: &str, comment: &str, th: &Thresholds) -> Verdict
/// policy.py::judge_verdict: one-hot judges bypass the policy.
pub fn judge_verdict(j: &Judgement, one_hot: bool, comment_kind: &str, comment: &str, th: &Thresholds) -> Verdict
```

Port `_decide` and `decide` line for line: the `checks` list of `(value, threshold, scale)`,
`KeyError → Review` (a missing question or a missing `"true"` option is `0.0` for `_p`, but a
missing question is `Review`), the margin test `abs(x - t) < margin * scale`, the holistic
disagreement test, `todo`/`directive` → `Keep`, the api_doc quality formula with the
`_REPLACEABLE` weights, and `comment.count('\n') + 1 > cap_lines`. Floating-point: Python and
Rust both use IEEE doubles and the same operation order, so results are identical if the
expression order is kept. One known gap: Python's `max(a, key=a.get)` breaks ties by insertion
order, but `Answers` is a `BTreeMap`; treat an exact tie on the holistic `verdict` as `Review`
in both languages (change `policy.py` too, with a shared case) rather than guess an order.

Shared cases: `tests/fixtures/policy/cases.json` is an array of `{name, answers, comment_kind,
comment, thresholds_override, expected}` covering every branch in `tools/test_bakeoff_policy.py`
plus the api_doc paths (`delete` via `q < api_delete`, `delete` via `c5 < c5_min`,
`rewrite_shorter`, `keep`) and three margin cases on the scale-3 expectations. The Python test
gains `test_shared_cases` that runs the file through `policy.decide`; the Rust test
`policy::tests::shared_cases` runs the same file through `policy::decide`. Both must pass; the
file is the contract.

### Step 10. Config keys and fingerprint

Files: `src/types.rs`, `src/main.rs`, `src/default_config.toml`, `src/reduction.rs`, README's
config list.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JudgeKind { Legacy, SystemOne, Generative }
// Config gains:
pub judge: JudgeKind,                 // config `judge = "legacy" | "systemone" | "generative"`
pub judge_endpoint: Option<String>,   // full URL; default derived: `{endpoint}/systemone` for systemone, `{endpoint}` for generative
pub judge_model: String,              // default "clef-flash-4bit"
pub escalation_model: Option<String>, // step 14; None = no escalation
```

`default_config.toml` ships `judge = "legacy"` until the bake-off decision flips it (one-line
change later; `docs/decisions/judge-bakeoff.md` says which). CLI flags `--judge`,
`--judge-model`, `--judge-endpoint` under the `LLM` heading; `parse_config` learns the four
keys. No `policy = "rubric"` key yet (deferred: there is only one preset file).

Fingerprint in `reduction::run`: when `cfg.judge != Legacy`, `llm_key` gains
`include_str!("judge.rs")`, `include_str!("policy.rs")`, `include_str!("context.rs")`,
`include_str!("evidence.rs")`, `include_str!("render.rs")`, `render::SCHEMA_STRICT`,
`policy::THRESHOLDS_STRICT`, `format!("{:?}", cfg.judge)`, `cfg.judge_endpoint`,
`cfg.judge_model`, `cfg.escalation_model`. With `Legacy` the key is built exactly as today, so
existing cached verdicts stay valid; a test asserts the legacy `llm_key` is unchanged by this
step. A schema or threshold edit from then on invalidates judge verdicts, which is the point.
Steps 12 and 13 append their embedded files (`frontier.md`, `verify-v1.json`) to the same list.

Also in this step: compute blame, `ItemContext`, `Evidence` and the state (steps 2-7) in
`reduction::run` only when `cfg.judge != Legacy`. Legacy runs then do no extra git or render
work. `--dump-items` still computes everything.

Tests: `main::tests` parses the new keys and rejects `judge = "bogus"`; `cli_defaults` asserts
the default is `Legacy`.

### Step 11. The judge path inside `reduction::run`

Two commits: 11a is the judge path, the ledger and the `tests/reduce.rs` cases (review and
rename are counted as kept in the summary); 11b adds the `review`/`rename` counters to
`FileResult`, `Stats`, `summary_line` and `main.rs`.

Files: `src/reduction.rs`, `src/lib.rs` (`Stats`, `FileResult`), `src/progress.rs`,
`src/main.rs` (summary line), `tests/reduce.rs`.

Where it plugs in: the worker closure passed to `parallel` in `reduction::run` (today lines
189-226). It currently calls `item.reduce(...)`. After this step:

```rust
let outcome = match cfg.judge {
    JudgeKind::Legacy => item.reduce(&plan.plan.source, plan.lang, client, cfg),
    _ => judge_item(item, &plan.contexts[index], &plan.states[index], plan.lang, &plan.plan.source,
                    judge.as_ref(), &th, cfg, &db, id),
};
```

```rust
/// Returns the same (disposition, edit) pair as Item::reduce, so everything downstream is unchanged.
fn judge_item(item: &Item, ctx: &ItemContext, state: &str, lang: Language, source: &str,
    judge: &dyn Judge, th: &Thresholds, cfg: &Config, db: &Mutex<Database>, id: i64)
    -> Result<(&'static str, Option<Edit>)>
```

Inside: `questions = render::questions_for(render::SCHEMA_STRICT, Some(ctx.comment_kind.as_str()))`,
`j = judge.judge(state, &questions)?`, `v = policy::judge_verdict(&j, judge.one_hot(), ...)`,
`db.store_answers(id, &json(j.answers), v.as_str(), &judge.name())`, then map:

| Verdict | disposition | edit |
|---|---|---|
| `Keep` | `"keep"` | none |
| `Delete` | `"delete"` | `item.delete(source)` (same as today) |
| `Review` | `"review"` | none; the file is still completed, the item untouched |
| `RenameInstead` | `"rename"` | none (report only; editing is unsafe and PLAN leaves it open) |
| `RewriteShorter`, `FixWrongClaim` | until step 13: `"review"` | none |

Dispositions are already free-form strings in `items.result_type`; `complete()` marks anything
that is not `keep` as `applied`, which is right for `review`/`rename` too (nothing was written,
the verdict is final for this source hash).

The worker closure must change in one place: today it rewrites any outcome without a
source-changing edit to `("keep", None)` (`_ => ("keep", None)` after `match edit`). Left as
is, every `review` and `rename` would be stored as `keep` and test (b) below could not pass.
Restrict that downgrade to `delete` and `reduce` dispositions; `review`, `rename` and `keep`
pass through with no edit.

11b: `FileResult` gains `review: usize` and `rename: usize`; `Stats` gains the same;
`summary_line` prints `N to review` and `N rename suggested` when non-zero (and returns a line
when only these are non-zero); `main.rs`'s final line adds `{} to review`. `--verbose` already
prints each item's `result_type`, which now includes `review`; the per-check explanation is
deferred (see the end).

Preflight: `comments.check()` is called only for `Legacy`; `SystemOne::check()` is called when
any job exists and `cfg.judge == SystemOne`. Workers: unchanged, `parallel` with
`cfg.llm_concurrency`.

Tests in `tests/reduce.rs`: a mock `/v1/systemone` server (extend the `Mock` helper to route on
path) returning fixed probabilities; assert (a) a row with `c2 = 0` is deleted and the file is
rewritten through the normal path, (b) a row near a threshold ends `review`, the file content is
unchanged, `items.result_type = 'review'` and `states.answers` is populated, (c) a second run
makes no HTTP calls (verdict cached), (d) `judge = "legacy"` still uses `/chat/completions` and
the old prompt (existing tests cover this; add one assertion that no `/systemone` call happens).

Exit check: `reduce --judge systemone` against `tools/bakeoff/mock_systemone.py` on the
`tests/fixtures/` tree runs end to end and prints review counts.

### Step 12. The `Generative` backend

Files: `src/judge.rs`, move `tools/bakeoff/prompts/frontier.md` to `src/schema/frontier.md`,
edit `tools/bakeoff/judges/generative.py` (`PROMPT` points at the new path).

```rust
pub struct Generative { client: LlmClient }      // reuses endpoint, model = judge_model, api_key
impl Judge for Generative { fn one_hot(&self) -> bool { true } ... }
pub(crate) fn parse_labels(text: &str) -> Option<serde_json::Map<String, Value>>  // generative.py::parse_labels: last JSON object holding "verdict", bare or under "labels", <think> stripped
pub(crate) fn one_hot(labels: &Map) -> Answers                                      // judges/__init__.py::one_hot
```

`LlmClient` gets one generic method `pub fn chat(&self, system: &str, user: &str, max_tokens:
u32) -> Result<String>` (the body of `summarize` minus prompt assembly) used here; `summarize`
and `rewrite_docstring` are left as they are. The judgement's `holistic` is `labels.verdict`;
`policy::judge_verdict` with `one_hot = true` returns it directly, as `policy.py` does.
`include_str!("schema/frontier.md")` joins the step 10 fingerprint list.

The generative judge cannot answer the verification questions of step 13 (its prompt only
produces the strict-schema labels), so in this plan its `RewriteShorter` and `FixWrongClaim`
verdicts end as `review` with no edit. Its `rewrite` label is not used. A verify prompt for the
generative backend is deferred until the bake-off says that backend ships.

Tests: unit tests for `parse_labels` (three inputs: clean JSON, JSON under `labels`, text with a
`<think>` block and two objects) and `one_hot` (bool, int, string, null skipped).

### Step 13. Verified rewrite path

Files: `src/judge.rs`, `src/render.rs`, `src/reduction.rs`, new `src/schema/verify-v1.json`,
`tools/bakeoff/render.py` (`render_verify`), `tools/bakeoff/test_render.py`, fixtures.

`verify-v1.json`: three `noul` questions `preserves`, `adds`, `g1_candidate` with the wording
from Proposal 1 ("Does `candidate` keep every hazard, contract or precondition stated in
`original`?", "Does `candidate` assert anything that is in neither `original` nor the code?",
"Does `candidate` contradict `annotated_code` or `enclosing_scope`?").

`render_verify(original, candidate, annotated_code, enclosing_scope, max_tokens)` in both
languages, sections `original:`, `candidate:`, `annotated_code:`, `enclosing_scope:` joined by
`"\n\n"`, with the same scope trimming as `render_state`. Two parity fixtures as in step 6.

Generator: the existing clients. For `Item::Comment`, `LlmClient::summarize(prose, context,
max_words)` exactly as `Item::reduce` builds it; a `Verdict::Delete` reply from the generator
counts as a failed rewrite (the judge said rewrite, not delete). For `Item::Docstring`,
`rewrite_docstring`. This path runs only for `judge = "systemone"` (see step 12). The comment and
docstring clients' `check()` preflight runs when any judge job exists, as today for legacy.
`include_str!("schema/verify-v1.json")` joins the step 10 fingerprint list.

```rust
pub struct VerifyOutcome { pub accepted: bool, pub answers: Answers }
pub(crate) fn verify_rewrite(judge: &dyn Judge, original: &str, candidate: &str, ctx: &ItemContext,
    th: &Thresholds) -> Result<VerifyOutcome>
// accepted = P(preserves) > th.verify_preserves && P(adds) < th.verify_adds && P(g1_candidate) < th.verify_g1
// systemone only; the generative backend does not verify (step 12)
```

Shorter is deterministic and applies to `RewriteShorter` only: the candidate must have fewer
lines than the original, or the same number of lines and fewer characters, else rejected. (A
lines-only rule would reject every rewrite of a one-line comment, which is what the `leak`
and api_doc paths produce.) `FixWrongClaim` candidates need not be shorter.

Mapping in `judge_item`:

| Verdict | accepted | rejected (unsafe reply, generator said delete, not shorter, verification) |
|---|---|---|
| `RewriteShorter` | `"reduce"`, `rewrite::reduce_edit` / `docstring::replace_edit` (same safety checks as `Item::reduce`) | `"keep"`, warning line `rewrite rejected (preserves 0.61)` |
| `FixWrongClaim` | `"reduce"`, same edits | `"review"`, no edit |

A generator or verifier transport error is not a rejection: it returns `Err` like any LLM
failure today, so the item is marked `error`, the file is left untouched and the item is
retried next run. Caching `keep` or `review` because a server was down would make that wrong
verdict permanent for this source hash.

Never delete from a failed rewrite. This follows `docs/PLAN.md` ("Failure keeps the original")
and Proposal 1's verified-rewrite section ("never fall back to delete from a failed rewrite").
Proposal 1 step 2 says the opposite for strict mode ("delete"); if the maintainer wants that,
it is a one-row change here, decided with the Phase 3 exit numbers. The verify answers are
appended to the ledger row (`states.answers` gets a `"verify"` sub-object) so eval v2 can
report errors-introduced from the same data.

Tests: in `judge.rs`, a `MockJudge` (`#[cfg(test)]`) returning scripted answers; unit test for
`verify_rewrite` accept and each reject reason. In `tests/reduce.rs`, a mock that answers
"rewrite_shorter"-shaped probabilities, then verify probabilities, with `/chat/completions`
returning `K1 hold the lock`; assert the comment is replaced by one line and the file parses;
a second case where verify fails and the file is unchanged.

### Step 14. Escalation

Files: `src/reduction.rs`, `src/judge.rs`.

When `cfg.escalation_model` is set, `reduction::run` builds a second judge of the same backend
with that model. In `judge_item`, when the first verdict is `Review`, judge once more with the
escalation judge and take its verdict if it is not `Review`; if it is, or if the two
non-review verdicts disagree, the item stays `Review`. Store both answer vectors
(`states.answers` gets `"escalation"`). Stats gain `escalated: usize`.

Test: `tests/reduce.rs` with a mock that answers near-threshold for model A and decisive for
model B (route on the `model` field in the request body); assert one item resolves and the
request count is two.

### Step 15. Eval v2

Two commits: 15a runs the judge over a v2 file and writes run files (`--eval-runs`), checked
against `score.py`; 15b ports `score.py`'s report into Rust and prints it.

Files: `src/eval.rs`, `src/main.rs` (`--eval-runs`), `src/render.rs`
(`StateRow::from_v2_row`), `tools/dataset/v2/split.py` (a `__main__` filter),
`tools/bakeoff/score.py` (no change needed; used as the oracle).

`reduce --eval FILE` dispatches on the first row: a v2 row has `labels` and `comment_kind`;
otherwise the v1 paths run as today. v2 needs `cfg.judge != Legacy`; with legacy it says so and
exits 2.

```rust
pub fn run_v2(dataset: &Path, cfg: &Config, runs_dir: Option<&Path>) -> Result<()>
```

No split logic in Rust. `tools/dataset/v2/SCHEMA.md` says `split.py` is the only
implementation, and porting SHA-1 by hand to avoid a crate is more code than the feature needs.
`split.py` gains a 5-line `__main__`: `python3 tools/dataset/v2/split.py test < GOLD > OUT`
keeps the rows whose `split_of(id)` matches. The Rust eval scores whatever file it is given.

Per row: `StateRow::from_v2_row(&Value)`, `render_state`, `questions_for`, `judge.judge`,
`policy::judge_verdict`; for `RewriteShorter`/`FixWrongClaim` also run the generator and
`verify_rewrite` and keep `rewrite_scores = {adds, g1_candidate, preserves}`. Workers via
`parallel`. Then compute exactly `score.py`'s report: `n`, `macro_f1` (over the five classes,
classes with no support skipped), the confusion matrix including a `review` column,
`hazards_lost` with pool and rate, `wrong_delete`, `churn`, `wrong_keep`, `error_introduced`
(`adds > 0.5 || g1_candidate > 0.5`), `review_rate`, `rest_accuracy`, `cost` with the same
weights (`0.5 review, 10, 3, 1, 1, 0.3`), per-question `n/acc/brier` (gold bools as
`"true"`/`"false"`, ints as `"2"`; the `+1` penalty when the gold option is absent from the
distribution), latency median and p95 (`sorted[min(n-1, int(0.95*n))]`), mean input tokens;
printed overall, per `language` and per `comment_kind` in `print_report`'s layout. Gold rows
whose `comment_kind` is `todo`/`directive` go through the policy like any other (it returns
`keep`), same as `score.py`.

`--eval-runs DIR` writes one `DIR/<id>.json` per row in the `run_judge.py::save` shape
(`id, judge, answers, verdict, rewrite, latency_ms, input_tokens, raw, rewrite_scores`). This
is the cross-check: `python3 tools/bakeoff/score.py --gold GOLD --judge <name> --runs
<parent of DIR> --split test` must print the same numbers the Rust eval printed.

Tests: `eval::tests::metrics_match_score_py` (15b) on a
committed synthetic `tests/fixtures/eval/gold-synthetic.jsonl` (8 rows, `source.repo =
"synthetic"`) and `tests/fixtures/eval/runs/*.json`, with the expected report written once by
`score.py --json` and committed as `expected.json`; the Rust test computes the metrics from the
same run files (no network) and compares the numbers to 1e-9.

Exit check: on the private gold test split, `python3 tools/dataset/v2/split.py test <
private/gold.jsonl > private/gold-test.jsonl`, `reduce --eval private/gold-test.jsonl --eval-runs
private/runs/rust-flash`, then `score.py` on that directory agree; the numbers go into the step
16 PR.

### Step 16. Docs, exit measurement, decision log

Files: `DESIGN.md`, `AGENTS.md`, `docs/PLAN.md`, `src/default_config.toml`,
`docs/decisions/judge-bakeoff.md` (append).

- `DESIGN.md`: replace "LLM verdict protocol" with a section describing what now exists: state,
  evidence, judge backends, policy, verified rewrite, ledger `states` table, config keys; keep
  the legacy section under a "Legacy judge" heading with "selectable until Phase 3 exit".
- `AGENTS.md`: the eval instruction becomes "after changing a schema, thresholds, render or
  policy file, filter the gold set to the test split with `tools/dataset/v2/split.py`, run
  `reduce --eval <that file>` with the configured judge and report
  the table before and after; the old `--eval` on `tools/dataset/*.jsonl` still applies to the
  legacy prompts".
- Phase 3 exit measurement, done by the maintainer, recorded in `docs/PLAN.md`'s decision log:
  hazards-lost ≤ the `gemma-asis` bake-off number, errors-introduced = 0 on a 50-item spot
  check of accepted rewrites (list the 50 ids and the result), verdict macro-F1 above
  `gemma-asis`. If all three hold, flip `judge` in `default_config.toml` to the winning
  backend in the same PR. Deleting the legacy path, `DEMOS`, `DOC_DEMOS` and the v1 `--eval` is
  a separate PR after that, not part of this plan.

---

## Out of scope or deferred

- Acting on evidence without a judge (Proposal 2's "act directly" column: overlap ≥ 0.8 delete,
  signature-echo delete, unresolved-identifier "stale" report). Evidence is computed and stored
  from step 4 but only the four `change` keys reach the state, because that is what the bake-off
  thresholds were tuned on. Adding evidence lines to the state means a new schema version and a
  re-run of the bake-off; decide after Phase 3 exit.
- `policy = "rubric"` preset and `rubric-v1.json`: only the strict preset exists. Add the key when
  a second thresholds file exists.
- `rename_instead` edits: report only. The open question in the proposal stands.
- Changeset mode, `check` for non-Python languages, SARIF, hook upgrade: Phase 4. Nothing in
  this plan blocks it; `ItemContext`, `Evidence` and the ledger are what Phase 4 consumes.
- Python docstrings for JS/TS JSDoc and Rust doc comments as `api_doc` items: they stay
  structural (always kept). Relaxing that is a product decision for the maintainer, not a Phase
  3 step.
- Deleting the legacy prompt path, `DEMOS`, `DOC_DEMOS` and v1 `--eval`: after the Phase 3 exit
  numbers are recorded.
- Prefix caching for the schema, batching several items in one System One request, Workers AI
  as a backend: not needed for correctness; measure first.
- Density proportionality (file-level comment density prior): report-only in the proposal and
  used by nothing in Phases 2-3.
- `policy::explain` (printing the closest checks for a `review` item under `--verbose`): useful
  but not in PLAN.md; add it when review lists are being worked through.
- A verification prompt for the `generative` backend, so its rewrites can be applied: only if
  the bake-off picks that backend.
- Re-running the Phase 1 extractor in Rust for the bake-off itself: the bake-off runs on the
  gold rows' stored fields, so Rust extraction parity (step 3) is a Phase 2 exit check, not a
  bake-off input.

---

## Review notes

Changes made after checking this plan against the source (`reduction.rs`, `lib.rs`,
`state.rs`, `hook_git.rs`, `parse.rs`, `docstring.rs`, `tools/bakeoff/*.py`, `Cargo.toml`):

1. Step 1: the classifier input stays `""` for non-Python items. The original text would have
   fed new scope text into `classifier_key`, which is part of the `items` UNIQUE key, and
   invalidated every cached JS/TS/Rust/YAML verdict.
2. Step 2: `plan_contexts` takes the parsed tree instead of re-parsing. `body_lines_after` needs
   the definition's end line, which `Docstring` does not store; it now comes from the tree.
   Split into 2a and 2b.
3. Step 4: `text_evidence` takes the item's byte range, which the "outside the comment's own
   bytes" rule needs and `ItemContext` does not hold.
4. Step 5: removed the bare `github.com` committer-domain match. `noreply@github.com` commits
   every web-UI merge and squash, so it would have labelled most human commits as Copilot's.
   Trailer matching is now case-insensitive. The repo root is found with `git rev-parse`.
5. Steps 6 and 8: questions keep the schema file's order. This crate's `serde_json` has no
   `preserve_order` feature, so a `serde_json::Map` sorts keys, and the Rust request would differ
   from the one the bake-off thresholds were tuned on.
6. Step 9: an exact tie on the holistic verdict is `review` in both languages, because
   `BTreeMap` cannot reproduce Python's insertion-order tie-break.
7. Step 10: judge settings join the fingerprint only when `judge != legacy`, so existing cached
   verdicts survive the upgrade. Added `evidence.rs`, and later `frontier.md` and
   `verify-v1.json`, to the fingerprint. Context, evidence and state work is gated off for legacy
   runs.
8. Step 11: the worker closure turns any outcome without an edit into `keep`. Without changing
   that, `review` and `rename` would have been stored as `keep`. Split into 11a and 11b.
   `policy::explain` moved to the deferred list (it is not in PLAN.md).
9. Step 12: the generative backend's rewrites end as `review`. Its prompt cannot answer the
   verification questions, so nothing could have verified them.
10. Step 13: a failed `FixWrongClaim` now ends as `review`, not `delete`. That matches PLAN.md
    ("Failure keeps the original") and Proposal 1's "never fall back to delete from a failed
    rewrite". Generator and verifier transport errors are now item errors and are retried; before,
    they were cached as a permanent `keep` or `delete`. The "shorter" rule also accepts the same
    line count with fewer characters, so one-line comments can be rewritten.
11. Step 15: dropped the hand-written SHA-1 and `--eval-split`. SCHEMA.md names `split.py` as
    the only split implementation, so the gold file is filtered with it first. Split into 15a
    and 15b.
