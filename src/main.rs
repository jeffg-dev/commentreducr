use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use commentreducr::{Config, Mode, Target, run};
use std::path::{Path, PathBuf};

const DEFAULT_ENDPOINT: &str = "http://localhost:8000/v1";
const DEFAULT_MODEL: &str = "gemma-4-e2b-it-4bit";
/// Docstring rewrites need the bigger model: E2B drops the gotcha a docstring exists to state.
const DEFAULT_DOC_MODEL: &str = "gemma-4-26b-a4b-it-4bit";
const DEFAULT_WORKERS: usize = 8;
const DEFAULT_MIN_LINES: usize = 4;
const DEFAULT_MIN_DENSITY: f64 = 5.0;
const DEFAULT_MAX_WORDS: usize = 20;

const AFTER_HELP: &str =
    "Advanced options: --help. Settings file: ~/.config/commentreducr/config.toml";
const AFTER_LONG_HELP: &str = "endpoint, model, docstrings_model, api_key, workers, min_lines, \
                               min_density and max_words can also be set in \
                               ~/.config/commentreducr/config.toml; flags win.";

/// Delete or reduce comments (Python, JS/TS, YAML) or Python docstrings in git-tracked files.
#[derive(Parser, Debug)]
#[command(name = "commentreducr", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Comments in Python, JS/TS and YAML files
    #[command(after_help = AFTER_HELP, after_long_help = AFTER_LONG_HELP)]
    Comments(Opts),
    /// Python docstrings (module, class, function)
    #[command(after_help = AFTER_HELP, after_long_help = AFTER_LONG_HELP)]
    Docstrings(Opts),
}

/// Field order is help order: common flags, then the "LLM" heading, then "Advanced" (long
/// help only). Numeric flags are `Option` so the config file can supply them when absent.
#[derive(clap::Args, Debug)]
struct Opts {
    /// Directory or file to process [default: .]
    path: Option<PathBuf>,

    /// Delete all non-structural comments/docstrings.
    #[arg(long)]
    delete: bool,

    /// Reduce large dense blocks to one line/short text via the LLM (default).
    #[arg(long, conflicts_with = "delete")]
    reduce: bool,

    /// With --delete only: report what would change without writing.
    #[arg(long, requires = "delete")]
    dry_run: bool,

    /// Worker threads (and max in-flight LLM requests) [default: 8]
    #[arg(short = 'n', long, alias = "concurrency", value_name = "N")]
    workers: Option<usize>,

    /// Print every changed block, not just per-file summaries.
    #[arg(short, long)]
    verbose: bool,

    /// Config file [default: ~/.config/commentreducr/config.toml]
    #[arg(
        long,
        value_name = "FILE",
        default_value_os_t = default_config_path(),
        hide_default_value = true
    )]
    config: PathBuf,

    /// OpenAI-compatible base URL [default: http://localhost:8000/v1]
    #[arg(long, help_heading = "LLM")]
    endpoint: Option<String>,

    /// Model name
    #[arg(
        long,
        help_heading = "LLM",
        long_help = "Model name [default: gemma-4-e2b-it-4bit for comments, \
                      gemma-4-26b-a4b-it-4bit for docstrings]"
    )]
    model: Option<String>,

    /// API key, if the endpoint needs one
    #[arg(long, help_heading = "LLM")]
    api_key: Option<String>,

    /// Minimum lines in a block before it is reduced [default: 4]
    #[arg(
        long,
        value_name = "N",
        help_heading = "Advanced",
        hide_short_help = true
    )]
    min_lines: Option<usize>,

    /// (comments only) Minimum average words per line [default: 5]
    #[arg(
        long,
        value_name = "N",
        help_heading = "Advanced",
        hide_short_help = true
    )]
    min_density: Option<f64>,

    /// (comments only) Target max words in a summary [default: 20]
    #[arg(
        long,
        value_name = "N",
        help_heading = "Advanced",
        hide_short_help = true
    )]
    max_words: Option<usize>,

    /// Parse only, no LLM, no writes: report files that fail to parse
    #[arg(
        long,
        conflicts_with = "eval",
        help_heading = "Advanced",
        hide_short_help = true,
        long_help = "Parse only, no LLM, no writes: print a redacted report of every file that \
                      fails to parse (node kinds and line shapes, no paths or code) for pasting \
                      into a bug report"
    )]
    diagnose: bool,

    /// Score the LLM prompt against a labeled JSONL dataset instead of processing files
    #[arg(
        long,
        value_name = "JSONL",
        help_heading = "Advanced",
        hide_short_help = true
    )]
    eval: Option<PathBuf>,
}

/// Optional settings from the config file; flags override these.
#[derive(serde::Deserialize, Default)]
struct FileConfig {
    endpoint: Option<String>,
    model: Option<String>,
    /// Model for the docstrings subcommand; falls back to `model`, then the built-in default.
    docstrings_model: Option<String>,
    api_key: Option<String>,
    workers: Option<usize>,
    min_lines: Option<usize>,
    min_density: Option<f64>,
    max_words: Option<usize>,
}

fn default_config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_default();
    base.join("commentreducr").join("config.toml")
}

/// A missing file is fine (all defaults); a present but malformed one is an error.
fn load_file_config(path: &Path) -> Result<FileConfig> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("bad config {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FileConfig::default()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // Panics inside a worker are caught and reported as warnings; keep the default hook quiet.
    std::panic::set_hook(Box::new(|_| {}));
    let (target, opts) = match &cli.command {
        Command::Comments(o) => (Target::Comments, o),
        Command::Docstrings(o) => (Target::Docstrings, o),
    };
    // clap's `requires = "delete"` on --dry-run only fires against the arguments actually typed,
    // so `--reduce --dry-run` (an explicit --reduce, rather than --reduce's absence) sails past
    // it and would otherwise run the full reduce pipeline -- including live LLM calls -- with
    // only the final write suppressed. Reduce mode does not support dry-run at all; check the
    // resolved flag instead of trusting clap to have rejected every shape of this combination.
    if opts.dry_run && !opts.delete {
        anyhow::bail!("--dry-run only applies to --delete");
    }
    let file = load_file_config(&opts.config)?;
    let cfg = Config {
        target,
        mode: if opts.delete {
            Mode::Delete
        } else {
            Mode::Reduce
        },
        min_lines: opts
            .min_lines
            .or(file.min_lines)
            .unwrap_or(DEFAULT_MIN_LINES),
        min_density: opts
            .min_density
            .or(file.min_density)
            .unwrap_or(DEFAULT_MIN_DENSITY),
        max_summary_words: opts
            .max_words
            .or(file.max_words)
            .unwrap_or(DEFAULT_MAX_WORDS),
        endpoint: opts
            .endpoint
            .clone()
            .or(file.endpoint)
            .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string()),
        model: opts
            .model
            .clone()
            .or(match target {
                Target::Comments => file.model,
                Target::Docstrings => file.docstrings_model.or(file.model),
            })
            .unwrap_or_else(|| {
                match target {
                    Target::Comments => DEFAULT_MODEL,
                    Target::Docstrings => DEFAULT_DOC_MODEL,
                }
                .to_string()
            }),
        api_key: opts.api_key.clone().or(file.api_key),
        llm_concurrency: opts.workers.or(file.workers).unwrap_or(DEFAULT_WORKERS),
        dry_run: opts.dry_run,
        verbose: opts.verbose,
    };
    if let Some(dataset) = &opts.eval {
        return match target {
            Target::Comments => commentreducr::eval::run(dataset, &cfg),
            Target::Docstrings => commentreducr::eval::run_docstrings(dataset, &cfg),
        };
    }
    let default_path = PathBuf::from(".");
    let path = opts.path.as_deref().unwrap_or(&default_path);
    if opts.diagnose {
        println!("commentreducr {}", env!("CARGO_PKG_VERSION"));
        let bad = commentreducr::diagnose(path, target)?;
        eprintln!("{bad} files with parse errors");
        if bad > 0 {
            std::process::exit(1);
        }
        return Ok(());
    }
    let stats = run(path, &cfg)?;
    let noun = match target {
        Target::Comments => "comments",
        Target::Docstrings => "docstrings",
    };
    eprintln!(
        "{} files scanned, {} changed, {} skipped; {noun}: {} kept, {} deleted ({} lines), {} reduced ({} lines saved), {} LLM failures",
        stats.files_scanned,
        stats.files_changed,
        stats.files_skipped,
        stats.comments_kept,
        stats.comments_deleted,
        stats.lines_deleted,
        stats.comments_reduced,
        stats.lines_reduced,
        stats.llm_errors,
    );
    if let Some(t) = stats.tokens {
        eprintln!(
            "tokens: {t}; {:.1}s, {}",
            stats.elapsed.as_secs_f64(),
            commentreducr::progress::token_rates(t, stats.elapsed)
        );
    }
    if stats.has_errors() {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_config_is_default_and_bad_config_errors() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "commentreducr-maintest-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        assert!(load_file_config(&path).unwrap().model.is_none());
        std::fs::write(&path, "model = \"m\"\napi_key = \"k\"\nmin_lines = 2\n").unwrap();
        let c = load_file_config(&path).unwrap();
        assert_eq!(
            (
                c.model.as_deref(),
                c.api_key.as_deref(),
                c.endpoint,
                c.docstrings_model,
                c.min_lines
            ),
            (Some("m"), Some("k"), None, None, Some(2))
        );
        std::fs::write(&path, "model = ").unwrap();
        assert!(load_file_config(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
