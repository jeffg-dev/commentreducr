# AGENTS.md

Rust CLI that strips low-value comments from JS/TS/Python/YAML, and low-value Python
docstrings. Read DESIGN.md first.

- Before committing: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`. CI enforces all three.
- main is protected. Work on a branch and open a PR.
- Two LLM prompts live in `src/llm.rs`: SYSTEM_PROMPT + DEMOS for comments (tuned for Gemma 4
  E2B), and DOC_SYSTEM_PROMPT + DOC_DEMOS for docstrings. After changing either, re-run its eval
  and report accuracy before and after: `cargo run -- comments --eval tools/dataset/comments.jsonl`
  or `cargo run -- docstrings --eval tools/dataset/docstrings.jsonl`. See `tools/dataset/README.md`
  for the rubrics.
- Keep it lean: small diffs, no new dependencies without a reason, minimal tests.
