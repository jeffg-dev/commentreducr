pub mod docstring;
pub mod eval;
pub mod files;
#[cfg(feature = "hook")]
pub mod hook_git;
#[cfg(feature = "hook")]
pub mod hook_model;
pub mod llm;
pub mod parse;
pub mod progress;
pub mod prose;
#[cfg(feature = "hook")]
mod reduction;
pub mod rewrite;
#[cfg(feature = "hook")]
pub mod state;
pub mod structural;
pub mod types;

use anyhow::{Context, Result, ensure};
#[cfg(feature = "hook")]
use docstring::DocKind;
use docstring::Docstring;
use progress::Progress;
use rewrite::Edit;
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
    pub lines_deleted: usize,
    pub lines_reduced: usize,
    pub files_skipped: usize,
    pub llm_errors: usize,
    pub classifier_errors: usize,
    pub screened: usize,
    pub classifier_cached: usize,
    pub verdicts_cached: usize,
    pub files_resumed: usize,
    pub tokens: Option<llm::TokenUsage>,
    pub elapsed: std::time::Duration,
}

impl Stats {
    fn merge(&mut self, r: FileResult) {
        self.files_changed += r.changed as usize;
        self.comments_kept += r.kept;
        self.comments_deleted += r.deleted;
        self.comments_reduced += r.reduced;
        self.lines_deleted += r.lines_deleted;
        self.lines_reduced += r.lines_reduced;
        self.files_skipped += r.skipped as usize;
        self.llm_errors += r.llm_errors;
    }

    pub fn has_errors(&self) -> bool {
        self.files_skipped > 0 || self.llm_errors > 0 || self.classifier_errors > 0
    }
}

#[derive(Default)]
struct FileResult {
    changed: bool,
    kept: usize,
    deleted: usize,
    reduced: usize,
    lines_deleted: usize,
    lines_reduced: usize,
    skipped: bool,
    llm_errors: usize,
}

impl FileResult {
    fn summary_line(&self, path: &Path) -> Option<String> {
        if self.deleted == 0 && self.reduced == 0 {
            return None;
        }
        let mut parts = Vec::new();
        if self.deleted > 0 {
            parts.push(format!(
                "{} deleted ({} lines)",
                self.deleted, self.lines_deleted
            ));
        }
        if self.reduced > 0 {
            parts.push(format!(
                "{} reduced ({} lines saved)",
                self.reduced, self.lines_reduced
            ));
        }
        Some(format!("{}: {}", path.display(), parts.join(", ")))
    }
}

#[derive(Clone)]
enum Item {
    Comment(CommentBlock),
    Docstring(Docstring),
}

impl Item {
    fn range(&self) -> (usize, usize, usize, usize) {
        match self {
            Self::Comment(b) => (b.start, b.end, b.start_line + 1, b.end_line + 1),
            Self::Docstring(d) => (d.start, d.end, d.start_line + 1, d.end_line + 1),
        }
    }

    fn delete(&self, source: &str) -> Option<Edit> {
        match self {
            Self::Comment(b) => Some(rewrite::delete_edit(source, b)),
            Self::Docstring(d) => docstring::delete_edit(source, d),
        }
    }

    #[cfg(feature = "hook")]
    fn kind(&self) -> &'static str {
        match self {
            Self::Comment(_) => "comment",
            Self::Docstring(d) if d.is_test => "test_docstring",
            Self::Docstring(d) => match d.kind {
                DocKind::Module => "module_docstring",
                DocKind::Class => "class_docstring",
                DocKind::Function => "function_docstring",
            },
        }
    }

    #[cfg(feature = "hook")]
    fn text(&self) -> String {
        match self {
            Self::Comment(b) => b
                .comments
                .iter()
                .map(|c| c.text.trim())
                .collect::<Vec<_>>()
                .join("\n"),
            Self::Docstring(d) => d.text.clone(),
        }
    }

    #[cfg(feature = "hook")]
    fn reduce(
        &self,
        source: &str,
        lang: Language,
        client: &llm::LlmClient,
        cfg: &Config,
    ) -> Result<(&'static str, Option<Edit>)> {
        match self {
            Self::Comment(b) => {
                let text = prose::clean_lines(b, lang).join("\n");
                let lines: Vec<&str> = source.lines().collect();
                let context = lines
                    .iter()
                    .skip(b.end_line + 1)
                    .map(|l| l.trim())
                    .find(|l| !l.is_empty())
                    .unwrap_or("");
                match client.summarize(&text, context, cfg.max_summary_words)? {
                    llm::Verdict::Delete => Ok(("delete", self.delete(source))),
                    llm::Verdict::Line(summary) => {
                        ensure!(
                            !summary.contains(['\n', '\r', '\u{2028}', '\u{2029}'])
                                && !summary.contains("*/"),
                            "unsafe comment replacement"
                        );
                        Ok((
                            "reduce",
                            Some(rewrite::reduce_edit(source, b, lang, &summary)),
                        ))
                    }
                }
            }
            Self::Docstring(d) => {
                let request = llm::DocRequest {
                    kind: match d.kind {
                        DocKind::Module => "module",
                        DocKind::Class => "class",
                        DocKind::Function => "function",
                    },
                    name: &d.name,
                    signature: &d.signature,
                    is_test: d.is_test,
                    in_test_file: d.in_test_file,
                    text: &d.text,
                    body_preview: &d.body_preview,
                    body_lines: d.body_lines,
                };
                match client.rewrite_docstring(&request)? {
                    llm::DocVerdict::Delete => Ok(("delete", self.delete(source))),
                    llm::DocVerdict::Text(text) => {
                        let edit = docstring::replace_edit(source, d, &text)
                            .context("unsafe docstring replacement")?;
                        Ok(("reduce", Some(edit)))
                    }
                }
            }
        }
    }
}

struct Plan {
    source: String,
    items: Vec<Item>,
    kept: usize,
}

fn plan_file(path: &Path, lang: Language, cfg: &Config) -> Result<Plan> {
    ensure!(
        !std::fs::symlink_metadata(path)?.file_type().is_symlink(),
        "source file is a symlink"
    );
    let source = std::fs::read_to_string(path)?;
    let mut items = Vec::new();
    let mut kept = 0;
    if cfg.target != Target::Docstrings {
        for b in parse::group_blocks(&source, parse::extract_comments(&source, lang)?) {
            if structural::is_structural(&b, lang, &source) {
                kept += 1;
            } else {
                items.push(Item::Comment(b));
            }
        }
    }
    if cfg.target != Target::Comments && lang == Language::Python {
        for d in
            docstring::extract_docstrings(&source, docstring::is_test_file(path), &cfg.keep_bases)?
        {
            if docstring::is_structural(&d, &source, &cfg.keep_decorators) {
                kept += 1;
            } else {
                items.push(Item::Docstring(d));
            }
        }
    }
    items.sort_by_key(|i| i.range().0);
    Ok(Plan {
        source,
        items,
        kept,
    })
}

pub fn run(root: &Path, cfg: &Config) -> Result<Stats> {
    ensure!(cfg.llm_concurrency > 0, "workers must be at least 1");
    let mut tracked = files::tracked_source_files(root, &cfg.ignore)?;
    tracked.retain(|(_, lang)| cfg.language.matches(*lang));
    if cfg.target == Target::Docstrings {
        tracked.retain(|(_, lang)| *lang == Language::Python);
    }
    if cfg.mode == Mode::Reduce {
        #[cfg(feature = "hook")]
        return reduction::run(root, tracked, cfg);
        #[cfg(not(feature = "hook"))]
        anyhow::bail!(
            "reduce requires the optional classifier: cargo install commentreducr --features hook"
        );
    }
    let progress = Progress::silent();
    let mut stats = Stats {
        files_scanned: tracked.len(),
        ..Stats::default()
    };
    for ((path, _), r) in parallel(
        tracked,
        cfg.llm_concurrency,
        |(path, lang)| -> Result<FileResult> {
            let plan = plan_file(path, *lang, cfg)?;
            let mut result = FileResult {
                kept: plan.kept,
                ..FileResult::default()
            };
            let mut edits = Vec::new();
            for item in plan.items {
                if let Some(edit) = item.delete(&plan.source) {
                    result.deleted += 1;
                    let (_, _, start, end) = item.range();
                    result.lines_deleted += end - start + 1;
                    edits.push(edit);
                } else {
                    result.kept += 1;
                }
            }
            let after = rewrite::apply(&plan.source, edits);
            parse::extract_comments(&after, *lang)
                .context("rewrite would introduce a parse error")?;
            result.changed = after != plan.source;
            if result.changed && !cfg.dry_run {
                atomic_write(path, &plan.source, &after)?;
            }
            if let Some(line) = result.summary_line(path) {
                progress.print(line);
            }
            Ok(result)
        },
    ) {
        match r {
            Ok(Ok(r)) => stats.merge(r),
            other => {
                eprintln!(
                    "warning: skipping {} ({})",
                    path.display(),
                    match other {
                        Ok(Err(e)) => format!("{e:#}"),
                        Err(e) => e,
                        _ => unreachable!(),
                    }
                );
                stats.files_skipped += 1;
            }
        }
    }
    Ok(stats)
}

// Rename a complete, synced sibling file only while the original still matches the plan.
fn atomic_write(path: &Path, before: &str, after: &str) -> Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let meta = std::fs::symlink_metadata(path)?;
    ensure!(!meta.file_type().is_symlink(), "source file is a symlink");
    let dir = path.parent().context("source file has no parent")?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = dir.join(format!(
        ".commentreducr-{}-{stamp}-{serial}.tmp",
        std::process::id()
    ));
    let mut created = false;
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        created = true;
        file.write_all(after.as_bytes())?;
        file.set_permissions(meta.permissions())?;
        file.sync_all()?;
        ensure!(
            std::fs::read_to_string(path)? == before,
            "file changed during analysis; rerun to rescan"
        );
        std::fs::rename(&temp, path)?;
        Ok(())
    })();
    if created {
        let _ = std::fs::remove_file(temp);
    }
    result
}

pub fn diagnose(
    root: &Path,
    target: Target,
    language: Languages,
    ignore: &[String],
) -> Result<usize> {
    let mut bad = 0;
    let mut tracked = files::tracked_source_files(root, ignore)?;
    tracked.retain(|(_, lang)| language.matches(*lang));
    if target == Target::Docstrings {
        tracked.retain(|(_, lang)| *lang == Language::Python);
    }
    for (path, lang) in tracked {
        let src = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("warning: skipping {} (unreadable: {e})", path.display());
                continue;
            }
        };
        if let Some(report) = parse::diagnose(&src, lang)? {
            bad += 1;
            eprintln!("file {bad}: {}", path.display());
            println!("### file {bad}: {report}");
        }
    }
    Ok(bad)
}

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
                    let r = std::panic::catch_unwind(AssertUnwindSafe(|| f(&item))).map_err(|p| {
                        p.downcast_ref::<String>()
                            .cloned()
                            .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                            .unwrap_or_else(|| "panic".to_owned())
                    });
                    out.lock().unwrap().push((item, r));
                }
            });
        }
    });
    out.into_inner().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_result_summary_line_omits_zero_parts() {
        let path = Path::new("src/foo.py");

        let nothing = FileResult::default();
        assert_eq!(nothing.summary_line(path), None);

        let deleted_only = FileResult {
            deleted: 3,
            lines_deleted: 45,
            ..Default::default()
        };
        assert_eq!(
            deleted_only.summary_line(path).as_deref(),
            Some("src/foo.py: 3 deleted (45 lines)")
        );

        let both = FileResult {
            deleted: 3,
            lines_deleted: 45,
            reduced: 1,
            lines_reduced: 12,
            ..Default::default()
        };
        assert_eq!(
            both.summary_line(path).as_deref(),
            Some("src/foo.py: 3 deleted (45 lines), 1 reduced (12 lines saved)")
        );

        let reduced_only = FileResult {
            reduced: 1,
            lines_reduced: 12,
            ..Default::default()
        };
        assert_eq!(
            reduced_only.summary_line(path).as_deref(),
            Some("src/foo.py: 1 reduced (12 lines saved)")
        );
    }
}
