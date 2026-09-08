use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use commentreducr::{Config, Mode, Target, run};
use std::path::{Path, PathBuf};

const DEFAULT_ENDPOINT: &str = "http://localhost:8000/v1";
const DEFAULT_MODEL: &str = "gemma-4-e2b-it-4bit";

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
    Comments(Opts),
    /// Python docstrings (module, class, function)
    Docstrings(Opts),
}

#[derive(clap::Args, Debug)]
struct Opts {
    /// Directory to process (git-tracked files under it, recursively).
    #[arg(required_unless_present = "eval")]
    path: Option<PathBuf>,

    /// Evaluate the LLM prompt against a labeled JSONL dataset instead of processing files.
    #[arg(long, value_name = "JSONL")]
    eval: Option<PathBuf>,

    /// Parse only, no LLM, no writes: print a redacted report of every file that fails to parse
    /// (node kinds and line shapes, no paths or code) for pasting into a bug report.
    #[arg(long, conflicts_with = "eval")]
    diagnose: bool,

    /// Reduce large dense comment/docstring blocks to one line/short text (default).
    #[arg(long, conflicts_with = "delete")]
    reduce: bool,

    /// Delete all non-structural comments/docstrings.
    #[arg(long)]
    delete: bool,

    /// Config file (TOML with endpoint / model / api_key).
    #[arg(long, value_name = "FILE", default_value_os_t = default_config_path())]
    config: PathBuf,

    /// OpenAI-compatible base URL for --reduce [default: http://localhost:8000/v1].
    #[arg(long)]
    endpoint: Option<String>,

    /// Model name; the prompt is tuned for Gemma 4 E2B [default: gemma-4-e2b-it-4bit].
    #[arg(long)]
    model: Option<String>,

    /// API key, if the endpoint needs one.
    #[arg(long)]
    api_key: Option<String>,

    /// Worker threads (also the max in-flight LLM requests).
    #[arg(long, default_value_t = 8)]
    concurrency: usize,

    /// Minimum prose lines (comments) or non-blank docstring lines (docstrings) for a block to
    /// be reduced.
    #[arg(long, default_value_t = 4)]
    min_lines: usize,

    /// (comments only) Minimum average words per line for a block to be reduced.
    #[arg(long, default_value_t = 5.0)]
    min_density: f64,

    /// (comments only) Target max words in a summary.
    #[arg(long, default_value_t = 20)]
    max_words: usize,

    /// With --delete only: count what would change without writing.
    #[arg(long, requires = "delete")]
    dry_run: bool,

    #[arg(short, long)]
    verbose: bool,
}

/// Optional settings from the config file; flags override these.
#[derive(serde::Deserialize, Default)]
struct FileConfig {
    endpoint: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
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
    let file = load_file_config(&opts.config)?;
    let cfg = Config {
        target,
        mode: if opts.delete {
            Mode::Delete
        } else {
            Mode::Reduce
        },
        min_lines: opts.min_lines,
        min_density: opts.min_density,
        max_summary_words: opts.max_words,
        endpoint: opts
            .endpoint
            .clone()
            .or(file.endpoint)
            .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string()),
        model: opts
            .model
            .clone()
            .or(file.model)
            .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
        api_key: opts.api_key.clone().or(file.api_key),
        llm_concurrency: opts.concurrency,
        dry_run: opts.dry_run,
        verbose: opts.verbose,
    };
    if let Some(dataset) = &opts.eval {
        return match target {
            Target::Comments => commentreducr::eval::run(dataset, &cfg),
            Target::Docstrings => commentreducr::eval::run_docstrings(dataset, &cfg),
        };
    }
    let path = opts.path.as_deref().unwrap();
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
        "{} files scanned, {} changed, {} skipped; {noun}: {} kept, {} deleted, {} reduced, {} LLM failures",
        stats.files_scanned,
        stats.files_changed,
        stats.files_skipped,
        stats.comments_kept,
        stats.comments_deleted,
        stats.comments_reduced,
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert!(load_file_config(&path).unwrap().model.is_none());
        std::fs::write(&path, "model = \"m\"\napi_key = \"k\"\n").unwrap();
        let c = load_file_config(&path).unwrap();
        assert_eq!(
            (c.model.as_deref(), c.api_key.as_deref(), c.endpoint),
            (Some("m"), Some("k"), None)
        );
        std::fs::write(&path, "model = ").unwrap();
        assert!(load_file_config(&path).is_err());
    }
}
