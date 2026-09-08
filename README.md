<p align="center"><img src="https://raw.githubusercontent.com/jeffg-dev/commentreducr/main/assets/banner.jpg" alt="commentreducr" width="600"></p>

# commentreducr

Strips low-value comments from JS/TS/Python/YAML in a git repo, and low-value Python
docstrings. Keeps structural comments (linter directives, licenses, TODOs) and docstrings
(doctests, module docstrings a file reads via `__doc__`, click/typer command help, license
text). Design notes in [DESIGN.md](DESIGN.md).

## Install

```sh
cargo install commentreducr
```

## Usage

```sh
commentreducr comments <path> --delete      # remove all non-structural comments
commentreducr comments <path> --reduce      # summarize dense prose blocks to one line (needs an LLM)
commentreducr docstrings <path> --delete    # remove Python docstrings
commentreducr docstrings <path> --reduce    # rewrite bloated docstrings to the caller's contract (needs an LLM)
```

`docstrings` always keeps doctests, a module docstring in a file that reads `__doc__`
(argparse/click render it as help text), any click/typer command's docstring, and license text
— in both `--delete` and `--reduce`. `--reduce` aims for a contract, not a story: a test
function's docstring becomes one or two lines saying what it guards; a test module's becomes
zero to one short paragraph; everything else gets the contract a caller needs, not a narration
of the implementation. A docstring shorter than `--min-lines` is left alone.

`--delete --dry-run` counts without writing. `--reduce` fails if the LLM is unreachable;
otherwise it scans first, then shows progress (percent, blocks, files, time left, token
counts and throughput) on stderr and prints token totals at the end.

Files that fail to parse are skipped with a warning. To report one, run
`commentreducr comments --diagnose <path>` (the repo, a subdirectory or a single file): it
parses only, touches nothing, and prints a redacted report to stdout (node kinds, positions and
line shapes with letters and digits masked, no paths or code) that is safe to paste into an
issue. The path behind each numbered file goes to stderr so you can review it first.

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
cargo fmt --check && cargo clippy --all-targets -- -D warnings       # CI gate
cargo run -- comments --eval tools/dataset/comments.jsonl            # score the comment prompt
cargo run -- docstrings --eval tools/dataset/docstrings.jsonl        # score the docstring prompt
```

PRs only; main requires CI. Prompt rubric in [tools/dataset](tools/dataset).

## License

Apache 2.0
