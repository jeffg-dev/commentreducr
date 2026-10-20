//! Git side of changeset mode: commit ranges, changed files, hunks and committed blobs, for
//! every supported language. Nothing here parses source or touches the working tree.
use crate::context::ItemContext;
use crate::types::{Config, Language, Languages};
use crate::{Item, plan_contexts, plan_source};
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

fn git(repo: &Path, args: &[&OsStr]) -> Result<Output> {
    Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .context("failed to run Git")
}

fn git_bytes(repo: &Path, args: &[&OsStr]) -> Result<Vec<u8>> {
    let out = git(repo, args)?;
    if !out.status.success() {
        bail!(
            "Git failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

pub(crate) fn git_text(repo: &Path, args: &[&str]) -> Result<String> {
    let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    String::from_utf8(git_bytes(repo, &args)?).context("Git output was not UTF-8")
}

fn commit(repo: &Path, revision: &str) -> Result<String> {
    git_text(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{revision}^{{commit}}"),
        ],
    )
    .map(|s| s.trim().to_owned())
    .with_context(|| format!("unknown revision {revision:?}"))
}

fn zero_oid(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|c| c == b'0')
}

/// Compare a new branch with the nearest common ancestor in known remote history. When no
/// remote history is available (including a repository's first push), its whole tree is new.
fn new_branch_base(repo: &Path, tip: &str, remote: Option<&str>) -> Result<Option<String>> {
    let prefix = remote
        .map(|r| format!("refs/remotes/{r}/"))
        .unwrap_or_else(|| "refs/remotes/".to_owned());
    let mut refs = git_text(repo, &["for-each-ref", "--format=%(objectname)", &prefix])?;
    // Git uses the URL as its remote-name argument when pushing directly to a URL.
    if refs.is_empty() && remote.is_some() {
        refs = git_text(
            repo,
            &["for-each-ref", "--format=%(objectname)", "refs/remotes/"],
        )?;
    }
    let mut best: Option<(usize, String)> = None;
    for other in refs.lines().collect::<std::collections::BTreeSet<_>>() {
        let out = git(
            repo,
            &[OsStr::new("merge-base"), OsStr::new(tip), OsStr::new(other)],
        )?;
        if out.status.code() == Some(1) {
            continue; // Unrelated histories.
        }
        if !out.status.success() {
            bail!(
                "Git merge-base failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let base = String::from_utf8(out.stdout)?.trim().to_owned();
        let distance: usize = git_text(repo, &["rev-list", "--count", &format!("{base}..{tip}")])?
            .trim()
            .parse()?;
        if best.as_ref().is_none_or(|(n, _)| distance < *n) {
            best = Some((distance, base));
        }
    }
    // In a shallow clone, missing history looks like unrelated history; never judge it all as new.
    if best.is_none()
        && !refs.is_empty()
        && git_text(repo, &["rev-parse", "--is-shallow-repository"])?.trim() == "true"
    {
        bail!(
            "no common ancestor with remote history in this shallow clone; fetch more history \
             (git fetch --unshallow, or a larger fetch-depth in CI) or pass --changed BASE..HEAD"
        );
    }
    Ok(best.map(|(_, base)| base))
}

fn pushed_ranges(
    repo: &Path,
    input: Option<&str>,
    remote: Option<&str>,
) -> Result<Vec<(Option<String>, String)>> {
    let Some(input) = input else {
        let tip = commit(repo, "HEAD")?;
        let upstream = git(
            repo,
            &[
                OsStr::new("rev-parse"),
                OsStr::new("--verify"),
                OsStr::new("@{upstream}^{commit}"),
            ],
        )?;
        let base = if upstream.status.success() {
            Some(String::from_utf8(upstream.stdout)?.trim().to_owned())
        } else {
            new_branch_base(repo, &tip, remote)?
        };
        return Ok(vec![(base, tip)]);
    };
    let mut ranges = Vec::new();
    for line in input.lines().filter(|line| !line.trim().is_empty()) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() != 4
            || !fields[1].bytes().all(|c| c.is_ascii_hexdigit())
            || !fields[3].bytes().all(|c| c.is_ascii_hexdigit())
        {
            bail!("invalid Git pre-push ref line");
        }
        if zero_oid(fields[1]) {
            continue; // Deleting a remote ref adds no blocks.
        }
        let tip = commit(repo, fields[1])?;
        let base = if zero_oid(fields[3]) {
            new_branch_base(repo, &tip, remote)?
        } else {
            Some(commit(repo, fields[3]).context(
                "remote tip is unavailable locally; fetch the remote before checking this push",
            )?)
        };
        if !ranges.contains(&(base.clone(), tip.clone())) {
            ranges.push((base, tip));
        }
    }
    Ok(ranges)
}

#[cfg(unix)]
fn git_path(bytes: &[u8]) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())))
}

#[cfg(not(unix))]
fn git_path(bytes: &[u8]) -> Result<PathBuf> {
    Ok(PathBuf::from(std::str::from_utf8(bytes)?))
}

struct ChangedPath {
    old: Option<PathBuf>,
    new: PathBuf,
}

fn changed_paths(repo: &Path, base: Option<&str>, tip: &str) -> Result<Vec<ChangedPath>> {
    let raw = if let Some(base) = base {
        git_bytes(
            repo,
            &[
                OsStr::new("diff"),
                OsStr::new("--no-ext-diff"),
                OsStr::new("--no-textconv"),
                OsStr::new("--name-status"),
                OsStr::new("--diff-filter=AMR"),
                OsStr::new("--find-renames"),
                OsStr::new("-z"),
                OsStr::new(base),
                OsStr::new(tip),
            ],
        )?
    } else {
        git_bytes(
            repo,
            &[
                OsStr::new("ls-tree"),
                OsStr::new("-r"),
                OsStr::new("--name-only"),
                OsStr::new("-z"),
                OsStr::new(tip),
            ],
        )?
    };
    let mut fields = raw.split(|&b| b == 0).filter(|s| !s.is_empty());
    let mut paths = Vec::new();
    if base.is_none() {
        for field in fields {
            paths.push(ChangedPath {
                old: None,
                new: git_path(field)?,
            });
        }
    } else {
        while let Some(status) = fields.next() {
            let first = git_path(
                fields
                    .next()
                    .ok_or_else(|| anyhow!("missing Git diff path"))?,
            )?;
            let (old, new) = if status.starts_with(b"R") {
                let new = git_path(
                    fields
                        .next()
                        .ok_or_else(|| anyhow!("missing Git rename path"))?,
                )?;
                (Some(first), new)
            } else if status == b"A" {
                (None, first)
            } else if status == b"M" {
                (Some(first.clone()), first)
            } else {
                bail!("unexpected Git diff status");
            };
            paths.push(ChangedPath { old, new });
        }
    }
    paths.retain(|p| Language::from_path(&p.new).is_some());
    Ok(paths)
}

/// A `.rs` file with a `.stderr` sibling at `revision` is a UI-test fixture whose line numbers
/// that snapshot pins; the whole-tree run skips these too (`files::tracked_source_files`).
fn is_ui_test(repo: &Path, revision: &str, path: &Path) -> Result<bool> {
    if path.extension() != Some(OsStr::new("rs")) {
        return Ok(false);
    }
    let mut object = OsString::from(revision);
    object.push(":");
    object.push(path.with_extension("stderr"));
    Ok(
        git(repo, &[OsStr::new("cat-file"), OsStr::new("-e"), &object])?
            .status
            .success(),
    )
}

/// None when the blob is not UTF-8.
fn blob(repo: &Path, revision: &str, path: &Path) -> Result<Option<String>> {
    let mut object = OsString::from(revision);
    object.push(":");
    object.push(path);
    Ok(String::from_utf8(git_bytes(repo, &[OsStr::new("show"), &object])?).ok())
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Hunk {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
}

fn line_range(token: &str) -> Result<(usize, usize)> {
    let token = token.get(1..).ok_or_else(|| anyhow!("invalid Git hunk"))?;
    let (start, count) = token.split_once(',').unwrap_or((token, "1"));
    Ok((start.parse()?, count.parse()?))
}

fn hunks(repo: &Path, base: &str, tip: &str, path: &ChangedPath) -> Result<Vec<Hunk>> {
    let mut args = vec![
        OsString::from("diff"),
        OsString::from("--no-ext-diff"),
        OsString::from("--no-textconv"),
        OsString::from("--no-color"),
        OsString::from("--unified=0"),
        OsString::from("--inter-hunk-context=0"),
        OsString::from("--find-renames"),
        OsString::from(base),
        OsString::from(tip),
        OsString::from("--"),
    ];
    for p in path.old.iter().chain(std::iter::once(&path.new)) {
        let mut literal = OsString::from(":(literal)");
        literal.push(p);
        args.push(literal);
    }
    let args: Vec<&OsStr> = args.iter().map(OsString::as_os_str).collect();
    let diff = git_bytes(repo, &args)?;
    let mut out = Vec::new();
    for line in diff
        .split(|&b| b == b'\n')
        .filter(|s| s.starts_with(b"@@ "))
    {
        let line = std::str::from_utf8(line)?;
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 4 || fields[3] != "@@" {
            bail!("invalid Git diff hunk");
        }
        let (old_start, old_count) = line_range(fields[1])?;
        let (new_start, new_count) = line_range(fields[2])?;
        out.push(Hunk {
            old_start,
            old_count,
            new_start,
            new_count,
        });
    }
    Ok(out)
}

pub(crate) fn intersects(start: usize, end: usize, hunk_start: usize, count: usize) -> bool {
    count > 0 && start <= hunk_start.saturating_add(count - 1) && end >= hunk_start
}

pub(crate) struct SourceBlock {
    pub start: usize,
    pub end: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub kind: String,
    pub text: String,
}

impl SourceBlock {
    pub(crate) fn of(item: &Item) -> Self {
        let (start, end, start_line, end_line) = item.range();
        Self {
            start,
            end,
            start_line,
            end_line,
            kind: item.kind().to_owned(),
            text: item.text(),
        }
    }
}

fn changes_block(hunk: &Hunk, block: &SourceBlock, old_blocks: &[SourceBlock]) -> bool {
    if hunk.new_count > 0 {
        return intersects(
            block.start_line,
            block.end_line,
            hunk.new_start,
            hunk.new_count,
        );
    }
    // A deletion has no added line, but can shorten a surviving block. A surviving line just
    // before/after the deletion must belong to both the old and new versions of that block.
    old_blocks.iter().any(|old| {
        old.kind == block.kind
            && intersects(old.start_line, old.end_line, hunk.old_start, hunk.old_count)
            && ((old.start_line < hunk.old_start
                && intersects(block.start_line, block.end_line, hunk.new_start, 1))
                || (old.end_line >= hunk.old_start + hunk.old_count
                    && intersects(block.start_line, block.end_line, hunk.new_start + 1, 1)))
    })
}

/// The hunks that change `block`; none when every such hunk only touches a line that already
/// carried the same block (a changed code line with an unchanged trailing comment).
pub(crate) fn changing_hunks<'h>(
    hunks: &'h [Hunk],
    block: &SourceBlock,
    old_blocks: &[SourceBlock],
) -> Vec<&'h Hunk> {
    let relevant: Vec<&Hunk> = hunks
        .iter()
        .filter(|h| changes_block(h, block, old_blocks))
        .collect();
    if relevant.iter().all(|h| {
        old_blocks.iter().any(|old| {
            old.kind == block.kind
                && old.text == block.text
                && intersects(old.start_line, old.end_line, h.old_start, h.old_count)
        })
    }) {
        return Vec::new();
    }
    relevant
}

/// Added and Modified are tier 1, ScopeChanged is tier 2, Untouched is tier 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Added,
    Modified,
    ScopeChanged,
    Untouched,
}

/// For tier 2: the annotated code of the same comment before the change.
#[derive(Debug)]
pub(crate) struct Before {
    pub annotated_code: String,
    pub enclosing_scope: String,
}

/// The base version's items, structural ones included, with their contexts in the same order.
#[derive(Default)]
pub(crate) struct Base {
    pub blocks: Vec<SourceBlock>,
    pub contexts: Vec<ItemContext>,
}

impl Base {
    /// A base that does not plan or parse has no items; that is never an error.
    pub(crate) fn new(path: &Path, source: String, lang: Language, cfg: &Config) -> Self {
        let Ok(mut plan) = plan_source(path, source, lang, cfg) else {
            return Self::default();
        };
        let Ok(tree) = crate::parse::parse_tree(&plan.source, lang) else {
            return Self::default();
        };
        plan.items.append(&mut plan.structural);
        plan.items.sort_by_key(|i| i.range().0);
        Self {
            contexts: plan_contexts(&plan, &tree, lang, path),
            blocks: plan.items.iter().map(SourceBlock::of).collect(),
        }
    }
}

/// Whether hunk `h` changes 1-based lines `lo..=hi`; a pure deletion counts when a line on
/// either side of it is in the range.
fn touches(lo: usize, hi: usize, h: &Hunk) -> bool {
    if h.new_count > 0 {
        intersects(lo, hi, h.new_start, h.new_count)
    } else {
        intersects(lo, hi, h.new_start, 2)
    }
}

/// Tier of one item of the head version. `ctx` is its context at head; `old` the base's blocks.
pub(crate) fn tier(
    item: &SourceBlock,
    ctx: &ItemContext,
    hunks: &[Hunk],
    old: &[SourceBlock],
) -> Tier {
    let relevant = changing_hunks(hunks, item, old);
    if !relevant.is_empty() {
        // Modified when the same text existed (moved), when part of the item lies outside the
        // changed lines (it grew or shrank), or when an old block of this kind sat at the same
        // offset in a replaced region (edited in place). Otherwise Added.
        let inside = |line: usize| {
            relevant
                .iter()
                .any(|h| intersects(line, line, h.new_start, h.new_count))
        };
        let modified = (item.start_line..=item.end_line).any(|l| !inside(l))
            || old.iter().filter(|o| o.kind == item.kind).any(|o| {
                o.text == item.text
                    || relevant.iter().any(|h| {
                        intersects(o.start_line, o.end_line, h.old_start, h.old_count)
                            && o.start_line.saturating_sub(h.old_start)
                                == item.start_line.saturating_sub(h.new_start)
                    })
            });
        return if modified {
            Tier::Modified
        } else {
            Tier::Added
        };
    }
    let ranges = [ctx.annotated_lines, ctx.scope_lines];
    if ranges
        .into_iter()
        .flatten()
        .any(|(lo, hi)| hunks.iter().any(|h| touches(lo + 1, hi + 1, h)))
    {
        return Tier::ScopeChanged;
    }
    Tier::Untouched
}

/// The annotated code of the base item with the same kind and text; with several, the one whose
/// start line is closest to `item`'s. None when the comment is new to this file.
pub(crate) fn before_block(base: &Base, item: &SourceBlock) -> Option<Before> {
    base.blocks
        .iter()
        .zip(&base.contexts)
        .filter(|(b, _)| b.kind == item.kind && b.text == item.text)
        .min_by_key(|(b, _)| b.start_line.abs_diff(item.start_line))
        .map(|(_, c)| Before {
            annotated_code: c.annotated_code.clone(),
            enclosing_scope: c.enclosing_scope.clone(),
        })
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) struct TempPath(pub(crate) PathBuf);

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub(crate) fn temp_file(dir: &Path, stem: &str, content: &[u8]) -> Result<TempPath> {
    let path = dir.join(format!(
        "{stem}-{}-{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    let guard = TempPath(path);
    file.write_all(content)?;
    Ok(guard)
}

fn ignored_paths(
    repo: &Path,
    paths: &[ChangedPath],
    patterns: &[String],
) -> Result<HashSet<PathBuf>> {
    if paths.is_empty() {
        return Ok(HashSet::new());
    }
    let excludes = temp_file(
        &std::env::temp_dir(),
        "commentreducr-hook-ignore",
        patterns.join("\n").as_bytes(),
    )?;
    let mut setting = OsString::from("core.excludesFile=");
    setting.push(&excludes.0);
    let mut child = Command::new("git")
        .arg("-c")
        .arg(setting)
        .args(["check-ignore", "--no-index", "-z", "--stdin"])
        .current_dir(repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run Git check-ignore")?;
    let mut input = Vec::new();
    for path in paths {
        input.extend_from_slice(path.new.as_os_str().as_encoded_bytes());
        input.push(0);
    }
    let mut stdin = child.stdin.take().expect("piped stdin");
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output()?;
    let _ = writer.join();
    if !matches!(out.status.code(), Some(0 | 1)) {
        bail!(
            "Git check-ignore failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    out.stdout
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(git_path)
        .collect()
}

/// One range of commits. `base` is None on a first push (the whole tree is new).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Range {
    pub base: Option<String>,
    pub head: String,
}

// shortcut: fields are first read by later Phase 4 steps.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct ChangedFile {
    /// Path at `head`, relative to the repo root.
    pub path: PathBuf,
    /// Path at `base`; None when added.
    pub old_path: Option<PathBuf>,
    pub lang: Language,
    /// With no base, one hunk covering every line.
    pub hunks: Vec<Hunk>,
    pub head_source: String,
    /// The commit `head_source` was read from.
    pub head: String,
    /// None when added or when the range has no base.
    pub base_source: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct Changeset {
    pub root: PathBuf,
    pub ranges: Vec<Range>,
    pub files: Vec<ChangedFile>,
    /// Agent tools named by the commits in `ranges`, deduplicated.
    pub agents: Vec<String>,
    /// Changed files in a supported language that `languages` left out.
    pub other_languages: usize,
    /// Changed files skipped because a version is not UTF-8.
    pub not_utf8: usize,
}

/// Parse `a..b`; both sides are resolved to commits.
fn parse_range(repo: &Path, spec: &str) -> Result<Range> {
    let (base, head) = spec
        .split_once("..")
        .filter(|(a, b)| !a.is_empty() && !b.is_empty() && !b.starts_with('.'))
        .ok_or_else(|| anyhow!("expected a range like BASE..HEAD, got {spec:?}"))?;
    Ok(Range {
        base: Some(commit(repo, base)?),
        head: commit(repo, head)?,
    })
}

/// `input` is Git's pre-push stdin (Some) or None for upstream..HEAD. `range` is an explicit
/// `<base>..<head>`; when Some, `input` and `remote` are ignored. Nothing is parsed here; an
/// unreadable blob is an error, and a file with a non-UTF-8 version is skipped with a warning.
pub(crate) fn collect(
    repo: &Path,
    range: Option<&str>,
    input: Option<&str>,
    remote: Option<&str>,
    languages: Languages,
    ignore: &[String],
) -> Result<Changeset> {
    let root =
        PathBuf::from(git_text(repo, &["rev-parse", "--show-toplevel"])?.trim_end_matches('\n'));
    let repo = root.as_path();
    let ranges: Vec<Range> = match range {
        Some(spec) => vec![parse_range(repo, spec)?],
        None => pushed_ranges(repo, input, remote)?
            .into_iter()
            .map(|(base, head)| Range { base, head })
            .collect(),
    };
    let mut files = Vec::new();
    let mut other_languages = 0;
    let mut not_utf8 = 0;
    // Ranges are already unique. A path in two ranges that share a head keeps both entries,
    // because each range's hunks are relative to its own base.
    for r in &ranges {
        let (base, tip) = (r.base.as_deref(), r.head.as_str());
        let paths = changed_paths(repo, base, tip)?;
        let ignored = ignored_paths(repo, &paths, ignore)?;
        for path in paths.into_iter().filter(|p| !ignored.contains(&p.new)) {
            if !Language::from_path(&path.new).is_some_and(|l| languages.matches(l)) {
                other_languages += 1;
                continue;
            }
            if is_ui_test(repo, tip, &path.new)? {
                continue;
            }
            let hunks = if let Some(base) = base {
                hunks(repo, base, tip, &path)?
            } else {
                vec![Hunk {
                    old_start: 0,
                    old_count: 0,
                    new_start: 1,
                    new_count: usize::MAX,
                }]
            };
            if hunks.is_empty() {
                continue;
            }
            let base_source = match (base, &path.old) {
                (Some(base), Some(old)) => Some(blob(repo, base, old)?),
                _ => None,
            };
            let head_source = blob(repo, tip, &path.new)?;
            let (Some(head_source), false) = (head_source, base_source == Some(None)) else {
                eprintln!("warning: skipping {}: not UTF-8", path.new.display());
                not_utf8 += 1;
                continue;
            };
            files.push(ChangedFile {
                lang: Language::from_path(&path.new).expect("filtered by changed_paths"),
                head_source,
                head: tip.to_owned(),
                old_path: path.old,
                path: path.new,
                hunks,
                base_source: base_source.flatten(),
            });
        }
    }
    let cache = Mutex::new(HashMap::new());
    let mut agents: Vec<String> = Vec::new();
    for a in ranges.iter().flat_map(|r| agents_in_range(repo, r, &cache)) {
        if !agents.contains(&a) {
            agents.push(a);
        }
    }
    Ok(Changeset {
        root,
        ranges,
        files,
        agents,
        other_languages,
        not_utf8,
    })
}

/// Lower-case tool names of every commit in `base..head` (or up to 200 commits reachable from
/// head when base is None) that `evidence::agent_of` recognises, deduplicated, first-seen
/// order. Any Git failure gives an empty list.
pub(crate) fn agents_in_range(
    repo: &Path,
    range: &Range,
    cache: &Mutex<HashMap<String, Option<String>>>,
) -> Vec<String> {
    let spec = match &range.base {
        Some(b) => format!("{b}..{}", range.head),
        None => range.head.clone(),
    };
    let args = ["log", "--max-count=200", "--format=%H", spec.as_str()];
    let mut out: Vec<String> = Vec::new();
    for sha in git_text(repo, &args).unwrap_or_default().lines() {
        if let Some(a) = crate::evidence::agent_of(repo, sha, cache)
            && !out.contains(&a)
        {
            out.push(a);
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) struct Repo(pub PathBuf);

    impl Repo {
        pub fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "commentreducr-hook-test-{}-{}",
                std::process::id(),
                TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            let repo = Self(path);
            repo.git(&["init", "-q", "-b", "main"]);
            repo.git(&["config", "user.name", "Hook Test"]);
            repo.git(&["config", "user.email", "hook-test@example.invalid"]);
            repo
        }

        pub fn git(&self, args: &[&str]) -> String {
            git_text(&self.0, args).unwrap()
        }

        pub fn write(&self, path: &str, text: &str) {
            let path = self.0.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }

        pub fn commit(&self) -> String {
            self.git(&["add", "-A"]);
            self.git(&["commit", "-qm", "fixture"]);
            self.git(&["rev-parse", "HEAD"]).trim().to_owned()
        }
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn collect_returns_files_of_every_language_and_filters_by_language() {
        let repo = Repo::new();
        repo.write("a.py", "x = 1\n");
        repo.write("b.ts", "const x = 1;\n");
        repo.write("c.yaml", "k: 1\n");
        repo.write("d.txt", "ignored\n");
        let base = repo.commit();
        repo.write("a.py", "x = 1\n# new\n");
        repo.write("b.ts", "const x = 1;\n// new\n");
        repo.write("c.yaml", "k: 1\n# new\n");
        repo.write("d.txt", "changed\n");
        let tip = repo.commit();
        let spec = format!("{base}..{tip}");
        let all = collect(&repo.0, Some(&spec), None, None, Languages::All, &[]).unwrap();
        assert_eq!(
            all.ranges,
            [Range {
                base: Some(base),
                head: tip
            }]
        );
        let got: Vec<_> = all
            .files
            .iter()
            .map(|f| (f.path.to_str().unwrap(), f.lang, f.hunks[0].new_start))
            .collect();
        assert_eq!(
            got,
            [
                ("a.py", Language::Python, 2),
                ("b.ts", Language::TypeScript, 2),
                ("c.yaml", Language::Yaml, 2)
            ]
        );
        assert_eq!(all.files[0].base_source.as_deref(), Some("x = 1\n"));
        assert!(all.files[0].head_source.ends_with("# new\n"));
        let py = collect(&repo.0, Some(&spec), None, None, Languages::Python, &[]).unwrap();
        assert_eq!(py.files.len(), 1);
        assert!(parse_range(&repo.0, "nonsense").is_err());
    }

    #[test]
    fn shallow_clone_without_a_shared_ancestor_is_an_error() {
        let origin = Repo::new();
        origin.write("a.py", "x = 1\n");
        origin.commit();
        origin.git(&["checkout", "-qb", "feat"]);
        origin.write("a.py", "x = 2\n");
        origin.commit();
        origin.git(&["checkout", "-q", "main"]);
        origin.write("a.py", "x = 3\n");
        origin.commit();
        let url = format!("file://{}", origin.0.display());
        let clone = Repo::new();
        clone.git(&[
            "fetch",
            "-q",
            "--depth=1",
            &url,
            "main:refs/remotes/origin/main",
        ]);
        clone.git(&["fetch", "-q", "--depth=1", &url, "feat"]);
        clone.git(&["checkout", "-qb", "x", "FETCH_HEAD"]);
        let tip = clone.git(&["rev-parse", "HEAD"]).trim().to_owned();
        let err = new_branch_base(&clone.0, &tip, None).unwrap_err();
        assert!(err.to_string().contains("shallow"), "{err}");
    }

    #[test]
    fn collect_skips_non_utf8_files() {
        let repo = Repo::new();
        repo.write("a.py", "x = 1\n");
        repo.write("b.py", "x = 1\n");
        let base = repo.commit();
        std::fs::write(repo.0.join("a.py"), b"x = 1\n# caf\xe9\n").unwrap();
        repo.write("b.py", "x = 1\n# new\n");
        let tip = repo.commit();
        let spec = format!("{base}..{tip}");
        let cs = collect(&repo.0, Some(&spec), None, None, Languages::All, &[]).unwrap();
        let got: Vec<_> = cs.files.iter().map(|f| f.path.to_str().unwrap()).collect();
        assert_eq!(got, ["b.py"]);
        assert_eq!(cs.not_utf8, 1);
    }

    #[test]
    fn collect_skips_rust_ui_test_fixtures() {
        let repo = Repo::new();
        repo.write("ui/bad.rs", "fn main() {}\n");
        repo.write("ui/bad.stderr", "error\n");
        repo.write("ui/ok.rs", "fn main() {}\n");
        let base = repo.commit();
        repo.write("ui/bad.rs", "// new\nfn main() {}\n");
        repo.write("ui/ok.rs", "// new\nfn main() {}\n");
        let tip = repo.commit();
        let spec = format!("{base}..{tip}");
        let cs = collect(&repo.0, Some(&spec), None, None, Languages::All, &[]).unwrap();
        let got: Vec<_> = cs.files.iter().map(|f| f.path.to_str().unwrap()).collect();
        assert_eq!(got, ["ui/ok.rs"]);
    }

    /// Every item of the head version of each fixture pair, with its tier and, for tier 2, the
    /// annotated code before the change.
    #[test]
    fn tiers_from_fixture() {
        let dir = Path::new("tests/fixtures/changeset");
        let read = |n: &str| std::fs::read_to_string(dir.join(n)).unwrap();
        let repo = Repo::new();
        repo.write("a.py", &read("before.py"));
        repo.write("b.ts", &read("before.ts"));
        let base = repo.commit();
        repo.write("a.py", &read("after.py"));
        repo.write("b.ts", &read("after.ts"));
        let head = repo.commit();
        let spec = format!("{base}..{head}");
        let cs = collect(&repo.0, Some(&spec), None, None, Languages::All, &[]).unwrap();
        let cfg = crate::reduction::tests::cfg(crate::JudgeKind::SystemOne);
        let mut got = serde_json::Map::new();
        for f in &cs.files {
            let base = Base::new(
                f.old_path.as_deref().unwrap(),
                f.base_source.clone().unwrap(),
                f.lang,
                &cfg,
            );
            let plan = plan_source(&f.path, f.head_source.clone(), f.lang, &cfg).unwrap();
            let tree = crate::parse::parse_tree(&plan.source, f.lang).unwrap();
            let rows: Vec<_> = plan
                .items
                .iter()
                .zip(plan_contexts(&plan, &tree, f.lang, &f.path))
                .map(|(i, ctx)| {
                    let item = SourceBlock::of(i);
                    let t = tier(&item, &ctx, &f.hunks, &base.blocks);
                    let before = (t == Tier::ScopeChanged)
                        .then(|| before_block(&base, &item).map(|b| b.annotated_code))
                        .flatten();
                    serde_json::json!({"line": item.start_line, "tier": t, "before": before})
                })
                .collect();
            got.insert(f.path.display().to_string(), rows.into());
        }
        let got = serde_json::to_string_pretty(&got).unwrap() + "\n";
        let json = dir.join("expected.json");
        if std::env::var_os("BLESS").is_some() {
            std::fs::write(&json, &got).unwrap();
        }
        assert_eq!(got, std::fs::read_to_string(&json).unwrap());
    }

    #[test]
    fn agents_from_commit_trailers() {
        let repo = Repo::new();
        repo.write("a.py", "x = 1\n");
        let first = repo.commit();
        repo.write("a.py", "x = 2\n");
        repo.git(&["add", "-A"]);
        repo.git(&[
            "commit",
            "-qm",
            "second\n\nCo-Authored-By: Claude <noreply@anthropic.com>",
        ]);
        let second = repo.git(&["rev-parse", "HEAD"]).trim().to_owned();
        let cache = Mutex::new(HashMap::new());
        let r = |b: &str, h: &str| Range {
            base: Some(b.into()),
            head: h.into(),
        };
        assert_eq!(
            agents_in_range(&repo.0, &r(&first, &second), &cache),
            ["claude-code"]
        );
        assert!(agents_in_range(&repo.0, &r(&first, &first), &cache).is_empty());
        let cs = collect(
            &repo.0,
            Some(&format!("{first}..{second}")),
            None,
            None,
            Languages::All,
            &[],
        )
        .unwrap();
        assert_eq!(cs.agents, ["claude-code"]);
    }

    /// Blame at a revision ignores the working tree.
    #[test]
    fn blame_at_head_ignores_dirty_tree() {
        let repo = Repo::new();
        repo.write("a.py", "x = 1\ny = 2\n");
        let head = repo.commit();
        repo.write("a.py", "# a\n# b\nx = 1\ny = 2\n");
        let b = crate::evidence::blame(&repo.0, Path::new("a.py"), Some(&head)).unwrap();
        assert_eq!(b.line_count(), 2);
    }

    /// `plan_states` with a changeset records the tier and renders the change evidence.
    #[test]
    fn states_carry_change_record() {
        let dir = Path::new("tests/fixtures/changeset");
        let read = |n: &str| std::fs::read_to_string(dir.join(n)).unwrap();
        let repo = Repo::new();
        repo.write("a.py", &read("before.py"));
        let base = repo.commit();
        repo.write("a.py", &read("after.py"));
        let head = repo.commit();
        let spec = format!("{base}..{head}");
        let cs = collect(&repo.0, Some(&spec), None, None, Languages::All, &[]).unwrap();
        let cfg = crate::reduction::tests::cfg(crate::JudgeKind::SystemOne);
        let f = &cs.files[0];
        let b = Base::new(
            f.old_path.as_deref().unwrap(),
            f.base_source.clone().unwrap(),
            f.lang,
            &cfg,
        );
        let plan = plan_source(&f.path, f.head_source.clone(), f.lang, &cfg).unwrap();
        let tree = crate::parse::parse_tree(&plan.source, f.lang).unwrap();
        let states = crate::plan_states(&plan, &tree, f.lang, &f.path, Some((f, &b, &cs)));
        let tiers: Vec<_> = states.iter().filter_map(|s| s.tier).collect();
        for t in [
            Tier::Added,
            Tier::Modified,
            Tier::ScopeChanged,
            Tier::Untouched,
        ] {
            assert!(tiers.contains(&t), "{t:?}");
        }
        for s in &states {
            assert_eq!(s.change.is_some(), s.tier != Some(Tier::Untouched));
            assert_eq!(s.evidence.contains("\"change\""), s.change.is_some());
            let before = s
                .change
                .as_ref()
                .and_then(|c| c.annotated_code_before.as_ref());
            assert_eq!(s.state.contains("annotated_code_before:"), before.is_some());
        }
    }

    /// The tier-2 gate reads the enclosing scope, not just the line under the comment.
    #[test]
    fn gate_sees_scope_changes() {
        let repo = Repo::new();
        let src = "def f():\n    limit = 3\n    # Retry up to 3 times.\n    tries = 0\n";
        repo.write("a.py", src);
        let base = repo.commit();
        repo.write("a.py", &src.replace("= 3", "= 5"));
        let head = repo.commit();
        let spec = format!("{base}..{head}");
        let cs = collect(&repo.0, Some(&spec), None, None, Languages::All, &[]).unwrap();
        let cfg = crate::reduction::tests::cfg(crate::JudgeKind::SystemOne);
        let f = &cs.files[0];
        let b = Base::new(
            f.old_path.as_deref().unwrap(),
            f.base_source.clone().unwrap(),
            f.lang,
            &cfg,
        );
        let plan = plan_source(&f.path, f.head_source.clone(), f.lang, &cfg).unwrap();
        let tree = crate::parse::parse_tree(&plan.source, f.lang).unwrap();
        let states = crate::plan_states(&plan, &tree, f.lang, &f.path, Some((f, &b, &cs)));
        let s = &states[0];
        assert_eq!(s.tier, Some(Tier::ScopeChanged));
        assert_eq!(s.change.as_ref().unwrap().changed_mentions, ["3"]);
    }
}
