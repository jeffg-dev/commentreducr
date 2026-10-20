mod changeset;
pub(crate) mod context;
pub mod docstring;
pub mod eval;
#[allow(dead_code)] // first caller arrives with the states table
pub(crate) mod evidence;
pub mod files;
pub mod findings;
pub mod hook_git;
pub mod hook_model;
#[allow(dead_code)] // first caller arrives with the judge path in reduction::run
pub(crate) mod judge;
pub mod llm;
pub mod parse;
#[allow(dead_code)] // first caller arrives with the judge path in reduction::run
pub(crate) mod policy;
pub mod progress;
pub mod prose;
mod reduction;
pub mod render;
pub mod rewrite;
pub mod state;
pub mod structural;
pub mod types;

use anyhow::{Context, Result, bail, ensure};
pub use changeset::Tier;
use docstring::{DocKind, Docstring};
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
    pub comments_review: usize,
    pub comments_rename: usize,
    /// Review items sent to the escalation model.
    pub escalated: usize,
    pub lines_deleted: usize,
    pub lines_reduced: usize,
    pub files_skipped: usize,
    pub llm_errors: usize,
    pub classifier_errors: usize,
    pub screened: usize,
    pub classifier_cached: usize,
    pub verdicts_cached: usize,
    pub files_resumed: usize,
    /// Tier-2 comments reported stale.
    pub comments_stale: usize,
    /// Changeset runs only: tier 1 and 2 items planned, and tier-3 items left alone.
    pub changed_items: usize,
    pub untouched: usize,
    /// Changeset runs only: every item whose disposition is not `keep`.
    pub findings: Vec<Finding>,
    pub tokens: Option<llm::TokenUsage>,
    pub elapsed: std::time::Duration,
}

impl Stats {
    fn merge(&mut self, r: FileResult) {
        self.files_changed += r.changed as usize;
        self.comments_kept += r.kept;
        self.comments_deleted += r.deleted;
        self.comments_reduced += r.reduced;
        self.comments_review += r.review;
        self.comments_rename += r.rename;
        self.comments_stale += r.stale;
        self.findings.extend(r.findings);
        self.lines_deleted += r.lines_deleted;
        self.lines_reduced += r.lines_reduced;
        self.files_skipped += r.skipped as usize;
        self.llm_errors += r.llm_errors;
    }

    /// One text line per finding, for the CLI to print after the summary.
    pub fn finding_lines(&self) -> impl Iterator<Item = String> + '_ {
        self.findings.iter().map(findings::text)
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
    review: usize,
    rename: usize,
    stale: usize,
    lines_deleted: usize,
    lines_reduced: usize,
    skipped: bool,
    llm_errors: usize,
    findings: Vec<Finding>,
}

/// One reported item of a changeset run.
#[derive(Debug)]
pub struct Finding {
    /// Relative to the repo root.
    pub path: std::path::PathBuf,
    pub start_line: usize,
    pub end_line: usize,
    pub tier: Tier,
    /// The context's comment_kind.
    pub kind: String,
    /// The item's disposition (`items.result_type`).
    pub verdict: String,
    /// (check name, value, threshold)
    pub checks: Vec<(String, f64, f64)>,
    /// The state's `evidence:` block joined with "; "; for tier-2 `stale`, `changed: ` and what
    /// the change touched in the comment.
    pub evidence: String,
    pub message: String,
    /// The comment or docstring as it appears in the file, delimiters included.
    pub comment: String,
}

impl FileResult {
    fn summary_line(&self, path: &Path) -> Option<String> {
        if self.deleted == 0
            && self.reduced == 0
            && self.review == 0
            && self.rename == 0
            && self.stale == 0
        {
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
        if self.review > 0 {
            parts.push(format!("{} to review", self.review));
        }
        if self.rename > 0 {
            parts.push(format!("{} rename suggested", self.rename));
        }
        if self.stale > 0 {
            parts.push(format!("{} stale", self.stale));
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

    fn reduce(
        &self,
        source: &str,
        lang: Language,
        client: &llm::LlmClient,
        cfg: &Config,
    ) -> Result<(&'static str, Option<Edit>)> {
        Ok(match self.rewrite(source, lang, client, cfg)? {
            Rewrite::Delete => ("delete", self.delete(source)),
            Rewrite::Unsafe(why) => bail!(why),
            Rewrite::Candidate(edit) => ("reduce", Some(edit)),
        })
    }

    /// Asks the comment or docstring client for a shorter version and builds the edit.
    fn rewrite(
        &self,
        source: &str,
        lang: Language,
        client: &llm::LlmClient,
        cfg: &Config,
    ) -> Result<Rewrite> {
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
                Ok(
                    match client.summarize(&text, context, cfg.max_summary_words)? {
                        llm::Verdict::Delete => Rewrite::Delete,
                        llm::Verdict::Line(summary)
                            if summary.contains(['\n', '\r', '\u{2028}', '\u{2029}'])
                                || summary.contains("*/") =>
                        {
                            Rewrite::Unsafe("unsafe comment replacement")
                        }
                        llm::Verdict::Line(summary) => {
                            Rewrite::Candidate(rewrite::reduce_edit(source, b, lang, &summary))
                        }
                    },
                )
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
                Ok(match client.rewrite_docstring(&request)? {
                    llm::DocVerdict::Delete => Rewrite::Delete,
                    llm::DocVerdict::Text(text) => docstring::replace_edit(source, d, &text)
                        .map_or(Rewrite::Unsafe("unsafe docstring replacement"), |e| {
                            Rewrite::Candidate(e)
                        }),
                })
            }
        }
    }
}

impl Item {
    /// The verified-rewrite generator's `text` as an edit of this item.
    fn replacement(&self, source: &str, lang: Language, text: &str) -> Rewrite {
        match self {
            Self::Comment(b) => rewrite::replace_edit(source, b, lang, text),
            Self::Docstring(d) => docstring::replace_edit(source, d, text),
        }
        .map_or(Rewrite::Unsafe("unsafe replacement"), Rewrite::Candidate)
    }
}

/// What the generator made of an item; `Unsafe` replies are errors on the legacy path.
enum Rewrite {
    Delete,
    Unsafe(&'static str),
    Candidate(Edit),
}

struct Plan {
    source: String,
    items: Vec<Item>,
    /// Items left alone as structural (directives, TODOs, JSDoc, doctests...); reduce never edits these.
    structural: Vec<Item>,
}

fn plan_file(path: &Path, lang: Language, cfg: &Config) -> Result<Plan> {
    ensure!(
        !std::fs::symlink_metadata(path)?.file_type().is_symlink(),
        "source file is a symlink"
    );
    plan_source(path, std::fs::read_to_string(path)?, lang, cfg)
}

/// `plan_file` minus the symlink check and the read: plans `source` as if it were `path`.
pub(crate) fn plan_source(
    path: &Path,
    source: String,
    lang: Language,
    cfg: &Config,
) -> Result<Plan> {
    let mut items = Vec::new();
    let mut kept = Vec::new();
    if cfg.target != Target::Docstrings {
        for b in parse::group_blocks(&source, parse::extract_comments(&source, lang)?) {
            if structural::is_structural(&b, lang, &source) {
                kept.push(Item::Comment(b));
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
                kept.push(Item::Docstring(d));
            } else {
                items.push(Item::Docstring(d));
            }
        }
    }
    items.sort_by_key(|i| i.range().0);
    Ok(Plan {
        source,
        items,
        structural: kept,
    })
}

pub(crate) fn plan_contexts(
    plan: &Plan,
    tree: &tree_sitter::Tree,
    lang: Language,
    path: &Path,
) -> Vec<context::ItemContext> {
    plan.items
        .iter()
        .map(|i| context::item_context(&plan.source, tree, lang, path, i))
        .collect()
}

pub(crate) struct ItemState {
    pub ctx: context::ItemContext,
    pub context: String,
    pub evidence: String,
    pub state: String,
    /// None when no changeset was given.
    pub tier: Option<changeset::Tier>,
    /// Set for tiers 1 and 2 only.
    pub change: Option<ChangeRecord>,
}

#[derive(serde::Serialize)]
pub(crate) struct ChangeRecord {
    pub comment_new: bool,
    pub comment_modified: bool,
    pub scope_changed: bool,
    pub annotated_code_before: Option<String>,
    /// Tier 2: what the comment names that the change touched; empty means no finding.
    pub changed_mentions: Vec<String>,
}

/// Context, evidence and rendered state per item, in item order. Git failures leave the git
/// evidence empty; nothing here can fail a run.
pub(crate) fn plan_states(
    plan: &Plan,
    tree: &tree_sitter::Tree,
    lang: Language,
    path: &Path,
    change: Option<(
        &changeset::ChangedFile,
        &changeset::Base,
        &changeset::Changeset,
    )>,
) -> Vec<ItemState> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    // In changeset mode the items come from the head blob, so blame that revision.
    let repo = match change {
        Some((_, _, cs)) => Some(cs.root.clone()),
        None => path.parent().and_then(evidence::repo_root),
    };
    let blame = repo.as_deref().and_then(|r| match change {
        Some((f, ..)) => evidence::blame(r, &f.path, Some(&f.head)),
        None => evidence::blame(r, path, None),
    });
    let agents = std::sync::Mutex::new(std::collections::HashMap::new());
    plan_contexts(plan, tree, lang, path)
        .into_iter()
        .zip(&plan.items)
        .map(|(ctx, item)| {
            let (start, end, start_line, end_line) = item.range();
            let mut ev = evidence::text_evidence(&ctx, &plan.source, (start, end));
            if let (Some(repo), Some(blame)) = (&repo, &blame) {
                let lines = (start_line.saturating_sub(1), end_line.saturating_sub(1));
                evidence::git_evidence(&mut ev, blame, &ctx, lines, now, repo, &agents);
            }
            let name = |v: Result<serde_json::Value, _>| {
                v.ok().and_then(|v| v.as_str().map(str::to_owned))
            };
            let kind = name(serde_json::to_value(ctx.comment_kind));
            let placement = name(serde_json::to_value(ctx.placement));
            let tier = change.map(|(file, base, _)| {
                changeset::tier(
                    &changeset::SourceBlock::of(item),
                    &ctx,
                    &file.hunks,
                    &base.blocks,
                )
            });
            let record = change.zip(tier).and_then(|((_, base, _), tier)| {
                use changeset::Tier::*;
                let before = (tier == ScopeChanged)
                    .then(|| changeset::before_block(base, &changeset::SourceBlock::of(item)))
                    .flatten();
                // The gate reads the whole window the judge sees: the annotated code and its scope.
                let changed_mentions = before.as_ref().map_or_else(Vec::new, |b| {
                    evidence::changed_mentions(
                        &ctx.comment,
                        &format!("{}\n{}", b.annotated_code, b.enclosing_scope),
                        &format!("{}\n{}", ctx.annotated_code, ctx.enclosing_scope),
                        &plan.source,
                    )
                });
                (tier != Untouched).then(|| ChangeRecord {
                    comment_new: tier == Added,
                    comment_modified: tier == Modified,
                    scope_changed: tier == ScopeChanged,
                    changed_mentions,
                    annotated_code_before: before.map(|b| b.annotated_code),
                })
            });
            // shortcut: a mixed human/agent range marks every unblamed tier-1 comment as
            // agent-authored; upgrade to per-commit blame at head if the 30-PR validation
            // shows it matters.
            if let Some((_, _, cs)) = change
                && matches!(
                    tier,
                    Some(changeset::Tier::Added | changeset::Tier::Modified)
                )
                && ev.authored_by_agent.is_none()
            {
                ev.authored_by_agent = Some(!cs.agents.is_empty());
                ev.agent = cs.agents.first().cloned();
            }
            let state = render::render_state(
                &render::StateRow {
                    language: Some(ctx.language),
                    comment_kind: kind.as_deref(),
                    placement: placement.as_deref(),
                    comment: &ctx.comment,
                    annotated_code: &ctx.annotated_code,
                    enclosing_scope: &ctx.enclosing_scope,
                    symbol_name: ctx.symbol.name.as_deref(),
                    public: ctx.symbol.public,
                    is_test: Some(ctx.symbol.is_test),
                    authored_by_agent: ev.authored_by_agent,
                    agent: ev.agent.as_deref(),
                    comment_age_days: ev.comment_age_days.map(|d| d as i64),
                    code_newer_than_comment: ev.code_newer_than_comment,
                    comment_new: record.as_ref().map(|r| r.comment_new),
                    comment_modified: record.as_ref().map(|r| r.comment_modified),
                    scope_changed: record.as_ref().map(|r| r.scope_changed),
                    annotated_code_before: record
                        .as_ref()
                        .and_then(|r| r.annotated_code_before.as_deref()),
                },
                render::MAX_STATE_TOKENS,
            );
            // Whole-tree rows keep today's field order; a Value would sort the keys.
            let evidence = match &record {
                None => serde_json::to_string(&ev).unwrap_or_default(),
                Some(record) => {
                    let mut v = serde_json::to_value(&ev).unwrap_or_default();
                    if let Some(map) = v.as_object_mut() {
                        map.insert(
                            "change".into(),
                            serde_json::to_value(record).unwrap_or_default(),
                        );
                    }
                    v.to_string()
                }
            };
            ItemState {
                context: serde_json::to_string(&ctx).unwrap_or_default(),
                evidence,
                state,
                tier,
                change: record,
                ctx,
            }
        })
        .collect()
}

/// Print every item of one file as a JSON array (byte range, 1-based lines, kind, text, the
/// item's context fields, and `structural`: true for items reduce leaves alone).
pub fn dump_items(file: &Path, cfg: &Config) -> Result<()> {
    let lang = Language::from_path(file).context("unsupported file extension")?;
    let cfg = Config {
        target: Target::All,
        ..cfg.clone()
    };
    let mut plan = plan_file(file, lang, &cfg)?;
    let structural: std::collections::HashSet<usize> =
        plan.structural.iter().map(|i| i.range().0).collect();
    plan.items.append(&mut plan.structural);
    plan.items.sort_by_key(|i| i.range().0);
    let tree = parse::parse_tree(&plan.source, lang)?;
    let states = plan_states(&plan, &tree, lang, file, None);
    let rows: Vec<_> = plan
        .items
        .iter()
        .zip(states)
        .map(|(i, st)| {
            let (start, end, start_line, end_line) = i.range();
            let mut row = serde_json::json!({"start": start, "end": end, "start_line": start_line,
                "end_line": end_line, "kind": i.kind(), "text": i.text(),
                "structural": structural.contains(&start),
                "evidence": serde_json::from_str::<serde_json::Value>(&st.evidence).ok(),
                "state": st.state});
            // Context keys are flattened in; the existing keys above win on a name clash.
            if let (Some(row), Ok(serde_json::Value::Object(ctx))) =
                (row.as_object_mut(), serde_json::to_value(&st.ctx))
            {
                for (k, v) in ctx {
                    row.entry(k).or_insert(v);
                }
            }
            row
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&rows)?);
    Ok(())
}

/// `check` on the judge path: tiers 1 and 2 of the changed files, report-only. `range`,
/// `input` and `remote` are as `changeset::collect` takes them.
pub fn check_changed(
    root: &Path,
    cfg: &Config,
    range: Option<&str>,
    input: Option<&str>,
    remote: Option<&str>,
) -> Result<Stats> {
    let mut cs = changeset::collect(root, range, input, remote, cfg.language, &cfg.ignore)?;
    if cfg.target == Target::Docstrings {
        cs.files.retain(|f| f.lang == Language::Python);
    }
    let mode = reduction::RunMode {
        changes: Some(&cs),
        report_only: true,
    };
    reduction::run(root, Vec::new(), cfg, mode)
}

pub fn run(root: &Path, cfg: &Config, changed: Option<&str>) -> Result<Stats> {
    ensure!(cfg.llm_concurrency > 0, "workers must be at least 1");
    if let Some(spec) = changed {
        ensure!(cfg.mode == Mode::Reduce, "--changed only applies to reduce");
        let mut cs = changeset::collect(root, Some(spec), None, None, cfg.language, &cfg.ignore)?;
        // collect returns repo-relative paths; the target may be a subdirectory or one file.
        let target = std::fs::canonicalize(root)?;
        let top = std::fs::canonicalize(&cs.root)?;
        cs.files.retain(|f| {
            top.join(&f.path).starts_with(&target)
                && (cfg.target != Target::Docstrings || f.lang == Language::Python)
        });
        let mode = reduction::RunMode {
            changes: Some(&cs),
            report_only: false,
        };
        return reduction::run(root, Vec::new(), cfg, mode);
    }
    let mut tracked = files::tracked_source_files(root, &cfg.ignore)?;
    tracked.retain(|(_, lang)| cfg.language.matches(*lang));
    if cfg.target == Target::Docstrings {
        tracked.retain(|(_, lang)| *lang == Language::Python);
    }
    if cfg.mode == Mode::Reduce {
        let mode = reduction::RunMode {
            changes: None,
            report_only: false,
        };
        return reduction::run(root, tracked, cfg, mode);
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
                kept: plan.structural.len(),
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

        let judged = FileResult {
            review: 2,
            rename: 1,
            ..Default::default()
        };
        assert_eq!(
            judged.summary_line(path).as_deref(),
            Some("src/foo.py: 2 to review, 1 rename suggested")
        );
    }
}
