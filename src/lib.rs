pub mod eval;
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
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::Mutex;
pub use types::*;

#[derive(Debug, Default)]
pub struct Stats {
    pub files_scanned: usize,
    pub files_changed: usize,
    pub comments_kept: usize,
    pub comments_deleted: usize,
    pub comments_reduced: usize,
    /// Files skipped because they could not be read, parsed, or processed (bug caught).
    pub files_skipped: usize,
    /// Blocks left unchanged because the LLM call failed.
    pub llm_errors: usize,
}

impl Stats {
    fn merge(&mut self, r: FileResult) {
        if r.changed {
            self.files_changed += 1;
        }
        self.comments_kept += r.kept;
        self.comments_deleted += r.deleted;
        self.comments_reduced += r.reduced;
        self.files_skipped += r.skipped as usize;
        self.llm_errors += r.llm_errors;
    }

    pub fn has_errors(&self) -> bool {
        self.files_skipped > 0 || self.llm_errors > 0
    }
}

/// Per-file outcome, folded into `Stats` once every task has finished.
#[derive(Default)]
struct FileResult {
    changed: bool,
    kept: usize,
    deleted: usize,
    reduced: usize,
    skipped: bool,
    llm_errors: usize,
}

/// Process every tracked source file under `root`.
/// Per file: read (skip non-UTF-8 with a warning), extract_comments -> group_blocks, for each block
/// analyze + is_structural + decide; Reduce actions call the LLM with bounded concurrency; build
/// edits; rewrite::apply; write back only if changed (unless dry_run, in which case print a
/// per-file summary of changes).
/// In reduce mode the endpoint is checked before any file is touched. After that the run is
/// best-effort: a failed LLM call leaves that block unchanged, and a panic while processing a file
/// (a bug) skips that file. Both are warned about and counted in `Stats`; nothing is written for a
/// file unless its whole pass succeeded.
/// Files are processed on `cfg.llm_concurrency` worker threads.
pub fn run(root: &Path, cfg: &Config) -> Result<Stats> {
    let tracked = files::tracked_source_files(root)?;
    let llm = (cfg.mode == Mode::Reduce).then(|| LlmClient::new(cfg));
    if let Some(client) = &llm {
        client.check()?;
    }

    let mut stats = Stats {
        files_scanned: tracked.len(),
        ..Stats::default()
    };
    let results = parallel(tracked, cfg.llm_concurrency, |(path, lang)| {
        process_file(path, *lang, llm.as_ref(), cfg)
    });
    for ((path, _), res) in results {
        match res {
            Ok(r) => stats.merge(r),
            Err(msg) => {
                eprintln!(
                    "warning: skipping {} (internal error: {msg})",
                    path.display()
                );
                stats.files_skipped += 1;
            }
        }
    }
    Ok(stats)
}

/// Run `f` over `items` on `workers` threads. A panic inside `f` is caught and returned as its
/// message, so one bad item cannot take the run down.
pub(crate) fn parallel<T: Send, R: Send>(
    items: Vec<T>,
    workers: usize,
    f: impl Fn(&T) -> R + Sync,
) -> Vec<(T, Result<R, String>)> {
    let queue = Mutex::new(items.into_iter());
    let out = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..workers.max(1) {
            s.spawn(|| {
                loop {
                    let Some(item) = queue.lock().unwrap().next() else {
                        break;
                    };
                    let r = std::panic::catch_unwind(AssertUnwindSafe(|| f(&item)))
                        .map_err(panic_message);
                    out.lock().unwrap().push((item, r));
                }
            });
        }
    });
    out.into_inner().unwrap()
}

/// Read, analyze and (if anything changed) rewrite one file. Recoverable failures (unreadable /
/// non-UTF-8 file, a parser error) are reported with a warning and the file is skipped; a failed
/// LLM call leaves that block unchanged.
fn process_file(path: &Path, lang: Language, llm: Option<&LlmClient>, cfg: &Config) -> FileResult {
    let mut result = FileResult {
        skipped: true,
        ..FileResult::default()
    };

    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("warning: skipping {} (unreadable: {err})", path.display());
            return result;
        }
    };

    let comments = match parse::extract_comments(&src, lang) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("warning: skipping {} ({err})", path.display());
            return result;
        }
    };
    result.skipped = false;
    let blocks = parse::group_blocks(&src, comments);
    let lines: Vec<&str> = src.lines().collect();

    let mut edits = Vec::new();
    for block in &blocks {
        let analysis = prose::analyze(block, lang);
        let structural = structural::is_structural(block, lang, &src);
        let action = policy::decide(block, &analysis, structural, cfg);

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
            Action::Reduce { prose } => {
                let context = first_nonblank_after(&lines, block.end_line);
                let line = block.start_line + 1;
                let client = llm.expect("reduce mode has an LLM");
                let verdict = match client.summarize(&prose, &context, cfg.max_summary_words) {
                    Ok(v) => v,
                    Err(err) => {
                        eprintln!(
                            "warning: {}:{line}: left unchanged, LLM failed ({err:#})",
                            path.display()
                        );
                        result.llm_errors += 1;
                        result.kept += 1;
                        continue;
                    }
                };
                match verdict {
                    llm::Verdict::Delete => {
                        result.deleted += 1;
                        if cfg.dry_run || cfg.verbose {
                            println!(
                                "{}:{line}: delete {} lines (llm)",
                                path.display(),
                                block.line_count()
                            );
                        }
                        edits.push(rewrite::delete_edit(&src, block));
                    }
                    llm::Verdict::Line(summary) => {
                        result.reduced += 1;
                        if cfg.dry_run || cfg.verbose {
                            println!("{}:{line}: reduce -> {summary}", path.display());
                        }
                        edits.push(rewrite::reduce_edit(&src, block, lang, &summary));
                    }
                }
            }
        }
    }

    if edits.is_empty() {
        return result;
    }

    let new_src = rewrite::apply(&src, edits);
    if new_src != src {
        result.changed = true;
        if !cfg.dry_run
            && let Err(err) = std::fs::write(path, &new_src)
        {
            eprintln!("warning: failed to write {}: {err}", path.display());
            result.changed = false;
        }
    }

    result
}

/// Best-effort text of a panic payload.
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic".to_string()
    }
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
