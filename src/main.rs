use anyhow::Result;
use clap::Parser;
use commentreducr::{Config, Mode, run};
use std::path::PathBuf;

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

    /// OpenAI-compatible base URL for --reduce.
    #[arg(
        long,
        env = "COMMENTREDUCR_ENDPOINT",
        default_value = "http://localhost:8000/v1"
    )]
    endpoint: String,

    /// Disable the LLM; use extractive summaries only.
    #[arg(long)]
    no_llm: bool,

    /// Model name sent to the endpoint. The prompt is tuned for Gemma 4 E2B.
    #[arg(
        long,
        env = "COMMENTREDUCR_MODEL",
        default_value = "gemma-4-e2b-it-4bit"
    )]
    model: String,

    /// API key, if the endpoint needs one (falls back to OPENAI_API_KEY).
    #[arg(long, env = "COMMENTREDUCR_API_KEY", hide_env_values = true)]
    api_key: Option<String>,

    /// Max in-flight LLM requests.
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

    /// Report what would change without writing.
    #[arg(long)]
    dry_run: bool,

    #[arg(short, long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config {
        mode: if cli.delete {
            Mode::Delete
        } else {
            Mode::Reduce
        },
        min_lines: cli.min_lines,
        min_density: cli.min_density,
        max_summary_words: cli.max_words,
        endpoint: if cli.no_llm || cli.delete {
            None
        } else {
            Some(cli.endpoint)
        },
        model: cli.model,
        api_key: cli.api_key.or_else(|| std::env::var("OPENAI_API_KEY").ok()),
        llm_concurrency: cli.concurrency,
        dry_run: cli.dry_run,
        verbose: cli.verbose,
    };
    if let Some(dataset) = &cli.eval {
        return commentreducr::eval::run(dataset, &cfg).await;
    }
    let stats = run(cli.path.as_deref().unwrap(), &cfg).await?;
    eprintln!(
        "{} files scanned, {} changed; comments: {} kept, {} deleted, {} reduced ({} extractive fallbacks)",
        stats.files_scanned,
        stats.files_changed,
        stats.comments_kept,
        stats.comments_deleted,
        stats.comments_reduced,
        stats.llm_fallbacks
    );
    Ok(())
}
