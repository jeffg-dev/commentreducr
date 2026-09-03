pub mod files;
pub mod llm;
pub mod parse;
pub mod policy;
pub mod prose;
pub mod rewrite;
pub mod structural;
pub mod types;

use anyhow::Result;
use llm::LlmClient;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::task::JoinSet;
pub use types::*;

#[derive(Debug, Default)]
pub struct Stats {
    pub files_scanned: usize,
    pub files_changed: usize,
    pub comments_kept: usize,
    pub comments_deleted: usize,
    pub comments_reduced: usize,
    pub llm_fallbacks: usize,
}

impl Stats {
    fn merge(&mut self, r: FileResult) {
        if r.changed {
            self.files_changed += 1;
        }
        self.comments_kept += r.kept;
        self.comments_deleted += r.deleted;
        self.comments_reduced += r.reduced;
        self.llm_fallbacks += r.llm_fallbacks;
    }
}

/// Per-file outcome, folded into `Stats` once every task has finished.
#[derive(Default)]
struct FileResult {
    changed: bool,
    kept: usize,
    deleted: usize,
    reduced: usize,
    llm_fallbacks: usize,
}

/// Process every tracked source file under `root`.
/// Per file: read (skip non-UTF-8 with a warning), extract_comments -> group_blocks, for each block
/// analyze + is_structural + decide; Reduce actions call the LLM (if configured) with bounded
/// concurrency, falling back to the extractive summary on error; build edits; rewrite::apply;
/// write back only if changed (unless dry_run, in which case print a per-file summary of changes).
/// Files are processed concurrently on the tokio runtime.
pub async fn run(root: &Path, cfg: &Config) -> Result<Stats> {
    let tracked = files::tracked_source_files(root)?;
    let llm = Arc::new(cfg.endpoint.as_ref().map(|_| LlmClient::new(cfg)));
    let cfg = Arc::new(cfg.clone());

    let mut set = JoinSet::new();
    for (path, lang) in tracked.iter().cloned() {
        set.spawn(process_file(path, lang, llm.clone(), cfg.clone()));
    }

    let mut stats = Stats {
        files_scanned: tracked.len(),
        ..Stats::default()
    };
    while let Some(res) = set.join_next().await {
        stats.merge(res?);
    }
    Ok(stats)
}

/// Read, analyze and (if anything changed) rewrite one file. Recoverable failures (unreadable /
/// non-UTF-8 file, a parser error) are reported with a warning and the file is skipped.
async fn process_file(
    path: PathBuf,
    lang: Language,
    llm: Arc<Option<LlmClient>>,
    cfg: Arc<Config>,
) -> FileResult {
    let mut result = FileResult::default();

    let src = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("warning: skipping {} (unreadable: {err})", path.display());
            return result;
        }
    };

    let comments = match parse::extract_comments(&src, lang) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("skipping {path}: {err}", path = path.display());
            return result;
        }
    };
    let blocks = parse::group_blocks(&src, comments);
    let lines: Vec<&str> = src.lines().collect();

    let mut edits = Vec::new();
    for block in &blocks {
        let analysis = prose::analyze(block, lang, cfg.max_summary_words);
        let structural = structural::is_structural(block, lang, &src);
        let action = policy::decide(block, &analysis, structural, &cfg);

        match action {
            Action::Keep => result.kept += 1,
            Action::Delete => {
                result.deleted += 1;
                if cfg.dry_run || cfg.verbose {
                    println!(
                        "{}:{}: delete {} lines",
                        path.display(),
                        block.start_line + 1,
                        block.line_count()
                    );
                }
                edits.push(rewrite::delete_edit(&src, block));
            }
            Action::Reduce { prose, fallback } => {
                result.reduced += 1;
                let context = first_nonblank_after(&lines, block.end_line);
                let summary = match llm.as_ref() {
                    Some(client) => match client
                        .summarize(&prose, &context, cfg.max_summary_words)
                        .await
                    {
                        Ok(s) => s,
                        Err(_) => {
                            result.llm_fallbacks += 1;
                            fallback
                        }
                    },
                    None => fallback,
                };
                if cfg.dry_run || cfg.verbose {
                    println!(
                        "{}:{}: reduce -> {}",
                        path.display(),
                        block.start_line + 1,
                        summary
                    );
                }
                edits.push(rewrite::reduce_edit(&src, block, lang, &summary));
            }
        }
    }

    if edits.is_empty() {
        return result;
    }

    let new_src = rewrite::apply(&src, edits);
    if new_src != src {
        result.changed = true;
        if !cfg.dry_run {
            if let Err(err) = std::fs::write(&path, &new_src) {
                eprintln!("warning: failed to write {}: {err}", path.display());
                result.changed = false;
            }
        }
    }

    result
}

/// First non-blank line strictly after `end_line` (0-based), trimmed and capped at 120 chars.
fn first_nonblank_after(lines: &[&str], end_line: usize) -> String {
    lines
        .iter()
        .skip(end_line + 1)
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .map(|l| l.chars().take(120).collect())
        .unwrap_or_default()
}
