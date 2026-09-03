# commentreducr

Strips low-value comments from a git-tracked codebase (JS/TS/Python), keeping structural
comments (linter directives, licenses, TODOs, etc.) intact. See [DESIGN.md](DESIGN.md) for
details.

## Install

```sh
cargo install commentreducr
```

## Usage

```sh
commentreducr <path> --reduce   # summarize dense prose comments, keep the rest
commentreducr <path> --delete   # remove all non-structural comments
```

Add `--dry-run` to preview changes without writing files, or `--no-llm` to skip the
LLM-based summarization in `--reduce` mode.

## LLM setup for `--reduce`

`--reduce` sends each dense comment block to an OpenAI-compatible chat endpoint and asks a
model whether to keep, summarize, or delete it. You need to point it at a server.

The prompt is tuned against [Gemma 4 E2B](https://huggingface.co/mlx-community/gemma-4-e2b-it-4bit)
(`gemma-4-e2b-it-4bit`, MLX), and that is the default model name. We recommend
[oMLX](https://github.com/jundot/omlx) on Apple Silicon because it prefix-caches the prompt,
but any OpenAI-compatible server (vLLM, LM Studio, Ollama, llama.cpp, a hosted API) works.
Other models will run but the keep/delete accuracy is only measured on Gemma 4 E2B.

Configure with flags or environment variables:

| Flag         | Env var                 | Default                    |
|--------------|-------------------------|----------------------------|
| `--endpoint` | `COMMENTREDUCR_ENDPOINT`| `http://localhost:8000/v1` |
| `--model`    | `COMMENTREDUCR_MODEL`   | `gemma-4-e2b-it-4bit`      |
| `--api-key`  | `COMMENTREDUCR_API_KEY` | none (also reads `OPENAI_API_KEY`) |

Example against a hosted provider:

```sh
export COMMENTREDUCR_ENDPOINT=https://api.example.com/v1
export COMMENTREDUCR_MODEL=some-model
export COMMENTREDUCR_API_KEY=sk-...
commentreducr <path> --reduce
```

## Development

```sh
git clone https://github.com/jeffg-dev/commentreducr && cd commentreducr
cargo test                                            # unit + CLI tests
cargo fmt --check && cargo clippy --all-targets -- -D warnings   # what CI enforces
cargo run -- <path> --delete --dry-run                # try it on a repo
cargo run -- --eval tools/dataset/comments.jsonl      # score the LLM prompt (needs an endpoint)
```

Changes go through a PR; main requires CI to pass. See [DESIGN.md](DESIGN.md) for how the
pipeline works and [tools/dataset](tools/dataset) for the prompt-tuning rubric.

## Development

```sh
git clone https://github.com/jeffg-dev/commentreducr && cd commentreducr
cargo test                                            # unit + CLI tests
cargo fmt --check && cargo clippy --all-targets -- -D warnings   # what CI enforces
cargo run -- <path> --delete --dry-run                # try it on a repo
cargo run -- --eval tools/dataset/comments.jsonl      # score the LLM prompt (needs an endpoint)
```

Changes go through a PR; main requires CI to pass. See [DESIGN.md](DESIGN.md) for how the
pipeline works and [tools/dataset](tools/dataset) for the prompt-tuning rubric.

## License

Apache 2.0. See [LICENSE](LICENSE).
