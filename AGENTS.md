# AGENTS.md

Rust CLI that strips low-value comments from JS/TS/Python. Read DESIGN.md first.

- Before committing: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`. CI enforces all three.
- main is protected. Work on a branch and open a PR.
- The LLM prompt lives in `src/llm.rs` (SYSTEM_PROMPT + DEMOS) and is tuned for Gemma 4 E2B. After any prompt change, re-run `cargo run -- --eval tools/dataset/comments.jsonl` and report accuracy before and after. See `tools/dataset/README.md` for the rubric.
- Keep it lean: small diffs, no new dependencies without a reason, minimal tests.
