<p align="center"><img src="https://raw.githubusercontent.com/jeffg-dev/commentreducr/main/assets/banner.jpg" alt="commentreducr" width="600"></p>

# commentreducr

Strips low-value comments from JS/TS/Python in a git repo. Keeps structural comments
(linter directives, licenses, TODOs). Design notes in [DESIGN.md](DESIGN.md).

## Install

```sh
cargo install commentreducr
```

## Usage

```sh
commentreducr <path> --delete    # remove all non-structural comments
commentreducr <path> --reduce    # summarize dense prose blocks to one line (needs an LLM)
```

`--delete --dry-run` counts without writing. `--reduce` fails if the LLM is unreachable.

## LLM for `--reduce`

Any OpenAI-compatible chat endpoint. The prompt is tuned for
[Gemma 4 E2B](https://huggingface.co/mlx-community/gemma-4-e2b-it-4bit) (MLX); other
models run but are unmeasured. [oMLX](https://github.com/jundot/omlx) is a good server on
Apple Silicon since it caches the prompt prefix.

Config in `~/.config/commentreducr/config.toml` (or `--config FILE`). Flags override.

```toml
endpoint = "http://localhost:8000/v1"   # default
model = "gemma-4-e2b-it-4bit"           # default
api_key = "sk-..."                       # optional
```

## Development

```sh
cargo test
cargo fmt --check && cargo clippy --all-targets -- -D warnings   # CI gate
cargo run -- --eval tools/dataset/comments.jsonl                 # score the prompt
```

PRs only; main requires CI. Prompt rubric in [tools/dataset](tools/dataset).

## License

Apache 2.0
