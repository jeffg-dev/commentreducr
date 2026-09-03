pub mod files;
pub mod llm;
pub mod parse;
pub mod policy;
pub mod prose;
pub mod rewrite;
pub mod structural;
pub mod types;

use anyhow::Result;
use std::path::Path;
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

/// Process every tracked source file under `root`.
/// Per file: read (skip non-UTF-8 with a warning), extract_comments -> group_blocks, for each block
/// analyze + is_structural + decide; Reduce actions call the LLM (if configured) with bounded
/// concurrency, falling back to the extractive summary on error; build edits; rewrite::apply;
/// write back only if changed (unless dry_run, in which case print a per-file summary of changes).
/// Files are processed concurrently on the tokio runtime.
pub async fn run(root: &Path, cfg: &Config) -> Result<Stats> {
    let _ = (root, cfg);
    todo!("lib::run")
}
