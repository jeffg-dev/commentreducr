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
commentreducr comments [path] --delete      # remove all non-structural comments (path defaults to .)
commentreducr comments [path] --reduce -n 8 # summarize dense prose blocks to one line (needs an LLM)
commentreducr docstrings [path] --delete    # remove Python docstrings
commentreducr docstrings [path] --reduce    # rewrite bloated docstrings to the caller's contract (needs an LLM)
```

`-h` shows the common options; `--help` shows everything, including tuning flags.

`docstrings` always keeps doctests, a module docstring in a file that reads `__doc__`
(argparse/click render it as help text), any click/typer command's docstring, and license text
— in both `--delete` and `--reduce`. `--reduce` aims for a contract, not a story: a test
function's docstring becomes one or two lines saying what it guards; a test module's becomes
zero to one short paragraph; everything else gets the contract a caller needs, not a narration
of the implementation. A docstring shorter than `--min-lines` is left alone.

`--delete --dry-run` counts without writing. Either mode prints a per-file summary line to
stdout for every file with a delete or reduce; add `-v`/`--verbose` for a line per comment/
docstring block too. `--reduce` fails if the LLM is unreachable; otherwise it scans first, then
shows progress (percent, blocks, files, time left, token counts and throughput) on stderr and
prints token totals at the end. The final summary reports both block counts and the source
lines they cover, e.g. `120 deleted (2340 lines), 5 reduced (60 lines saved)`.

Files that fail to parse are skipped with a warning. To report one, run
`commentreducr comments --diagnose <path>` (the repo, a subdirectory or a single file): it
parses only, touches nothing, and prints a redacted report to stdout (node kinds, positions and
line shapes with letters and digits masked, no paths or code) that is safe to paste into an
issue. The path behind each numbered file goes to stderr so you can review it first.

## LLM for `--reduce`

Any OpenAI-compatible chat endpoint. `comments` defaults to
[Gemma 4 E2B](https://huggingface.co/mlx-community/gemma-4-e2b-it-4bit) (MLX), which the
comment prompt is tuned for. `docstrings` defaults to
[Gemma 4 26B A4B](https://huggingface.co/mlx-community/gemma-4-26b-a4b-it-4bit): about 4x
slower per request, but E2B tends to drop the very gotcha a docstring exists to state. Other
models run but are unmeasured. [oMLX](https://github.com/jundot/omlx) is a good server on
Apple Silicon since it caches the prompt prefix.

The default build only talks to a plain `http://` endpoint, which covers a local server like
the ones above. For an `https://` endpoint (e.g. a cloud API), install with
`cargo install commentreducr --features tls`.

Config in `~/.config/commentreducr/config.toml` (or `--config FILE`). Flags override.

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
`tools/corpus_check.py` runs `--delete` over any tree and asserts the Python AST (modulo
docstrings) and every YAML document are unchanged; the stdlib and 1266 real YAML files pass.

## License

Apache 2.0
