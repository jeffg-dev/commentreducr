use anyhow::{Context, Result};
use clap::Parser;
use commentreducr::{Config, Mode, run};
use std::path::{Path, PathBuf};

const DEFAULT_ENDPOINT: &str = "http://localhost:8000/v1";
const DEFAULT_MODEL: &str = "gemma-4-e2b-it-4bit";

/// Delete or reduce comments in git-tracked Python and JS/TS source files.
#[derive(Parser, Debug)]
#[command(name = "commentreducr", version)]
struct Cli {
    /// Directory to process (git-tracked files under it, recursively).
    #[arg(required_unless_present = "eval")]
    path: Option<PathBuf>,

    /// Evaluate the LLM prompt against a labeled JSONL dataset instead of processing files.
    #[arg(long, value_name = "JSONL")]
    eval: Option<PathBuf>,

    /// Reduce large dense comment blocks to one line (default).
    #[arg(long, conflicts_with = "delete")]
    reduce: bool,

    /// Delete all non-structural comments.
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

    /// Minimum prose lines for a block to be reduced.
    #[arg(long, default_value_t = 4)]
    min_lines: usize,

    /// Minimum average words per line for a block to be reduced.
    #[arg(long, default_value_t = 5.0)]
    min_density: f64,

    /// Target max words in a summary.
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
    let file = load_file_config(&cli.config)?;
    let cfg = Config {
        mode: if cli.delete {
            Mode::Delete
        } else {
            Mode::Reduce
        },
        min_lines: cli.min_lines,
        min_density: cli.min_density,
        max_summary_words: cli.max_words,
        endpoint: cli
            .endpoint
            .or(file.endpoint)
            .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string()),
        model: cli
            .model
            .or(file.model)
            .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
        api_key: cli.api_key.or(file.api_key),
        llm_concurrency: cli.concurrency,
        dry_run: cli.dry_run,
        verbose: cli.verbose,
    };
    if let Some(dataset) = &cli.eval {
        return commentreducr::eval::run(dataset, &cfg);
    }
    let stats = run(cli.path.as_deref().unwrap(), &cfg)?;
    eprintln!(
        "{} files scanned, {} changed, {} skipped; comments: {} kept, {} deleted, {} reduced, {} LLM failures",
        stats.files_scanned,
        stats.files_changed,
        stats.files_skipped,
        stats.comments_kept,
        stats.comments_deleted,
        stats.comments_reduced,
        stats.llm_errors,
    );
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
