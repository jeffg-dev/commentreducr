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

## License

Apache 2.0. See [LICENSE](LICENSE).
