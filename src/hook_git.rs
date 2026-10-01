//! Inspect Python blocks in the committed trees Git is about to push.
use crate::{docstring, parse, structural, types::Language};
use anyhow::{Context, Result, anyhow, bail};
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone)]
pub struct HookBlock {
    pub path: PathBuf,
    /// One-based source lines in the pushed tree.
    pub start_line: usize,
    pub end_line: usize,
    pub kind: String,
    pub text: String,
    pub context: String,
}

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

fn git_text(repo: &Path, args: &[&str]) -> Result<String> {
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
    paths.retain(|p| Language::from_path(&p.new) == Some(Language::Python));
    Ok(paths)
}

fn blob(repo: &Path, revision: &str, path: &Path) -> Result<String> {
    let mut object = OsString::from(revision);
    object.push(":");
    object.push(path);
    String::from_utf8(git_bytes(repo, &[OsStr::new("show"), &object])?)
        .with_context(|| format!("{} is not UTF-8", path.display()))
}

#[derive(Clone, Copy)]
struct Hunk {
    old_start: usize,
    old_count: usize,
    new_start: usize,
    new_count: usize,
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

fn intersects(start: usize, end: usize, hunk_start: usize, count: usize) -> bool {
    count > 0 && start <= hunk_start.saturating_add(count - 1) && end >= hunk_start
}

struct SourceBlock {
    start: usize,
    end: usize,
    start_line: usize,
    end_line: usize,
    kind: String,
    text: String,
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

fn source_blocks(
    source: &str,
    path: &Path,
    keep_decorators: &[String],
    keep_bases: &[String],
    preserve_structural: bool,
) -> Result<Vec<SourceBlock>> {
    let mut out = Vec::new();
    for block in parse::group_blocks(source, parse::extract_comments(source, Language::Python)?) {
        if preserve_structural && structural::is_structural(&block, Language::Python, source) {
            continue;
        }
        out.push(SourceBlock {
            start: block.start,
            end: block.end,
            start_line: block.start_line + 1,
            end_line: block.end_line + 1,
            kind: "comment".to_owned(),
            text: block
                .comments
                .iter()
                .map(|c| c.text.trim())
                .collect::<Vec<_>>()
                .join("\n"),
        });
    }
    for doc in docstring::extract_docstrings(source, docstring::is_test_file(path), keep_bases)? {
        if preserve_structural && docstring::is_structural(&doc, source, keep_decorators) {
            continue;
        }
        let kind = if doc.is_test {
            "test_docstring"
        } else {
            match doc.kind {
                docstring::DocKind::Module => "module_docstring",
                docstring::DocKind::Class => "class_docstring",
                docstring::DocKind::Function => "function_docstring",
            }
        };
        out.push(SourceBlock {
            start: doc.start,
            end: doc.end,
            start_line: doc.start_line + 1,
            end_line: doc.end_line + 1,
            kind: kind.to_owned(),
            text: doc.text,
        });
    }
    out.sort_by_key(|b| b.start);
    Ok(out)
}

/// The enclosing function/class (including its header), or a nearby module neighborhood. Large
/// scopes retain the header and forty lines on either side; the target itself is removed.
fn context(source: &str, tree: &tree_sitter::Tree, block: &SourceBlock) -> String {
    context_for_range(
        source,
        tree,
        block.start,
        block.end,
        block.start_line,
        block.end_line,
    )
}

pub(crate) fn context_for_range(
    source: &str,
    tree: &tree_sitter::Tree,
    start: usize,
    end_byte: usize,
    start_line: usize,
    end_line: usize,
) -> String {
    let root = tree.root_node();
    let mut scope = root;
    let mut node = root.descendant_for_byte_range(start, end_byte);
    while let Some(n) = node {
        if matches!(n.kind(), "function_definition" | "class_definition") {
            scope = n;
            break;
        }
        node = n.parent();
    }
    let offsets: Vec<usize> = std::iter::once(0)
        .chain(source.match_indices('\n').map(|(i, _)| i + 1))
        .chain(std::iter::once(source.len()))
        .collect();
    let first = scope.start_position().row;
    let last = scope.end_position().row.min(offsets.len() - 2);
    let large = scope.kind() == "module" || last.saturating_sub(first) > 80;
    let from = if large {
        (start_line - 1).saturating_sub(40).max(first)
    } else {
        first
    };
    let to = if large {
        (end_line - 1 + 40).min(last)
    } else {
        last
    };
    let begin = offsets[from];
    let end = offsets[to + 1];
    let mut result = String::new();
    if from > first
        && scope.kind() != "module"
        && let Some(body) = scope.child_by_field_name("body")
    {
        result.push_str(source[scope.start_byte()..body.start_byte()].trim_end());
        result.push('\n');
    }
    result.push_str(&source[begin..start]);
    result.push_str(&source[end_byte..end]);
    result.trim().to_owned()
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempPath(PathBuf);

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn temp_file(dir: &Path, stem: &str, content: &[u8]) -> Result<TempPath> {
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

/// Some(input) is Git's pre-push stdin, including an empty list of refs. None checks
/// upstream..HEAD, falling back to known remote history or the full tree on the first push.
/// Returned targets are complete blocks; source bytes always come from committed Git blobs.
pub fn changed_blocks(
    repo: &Path,
    input: Option<&str>,
    remote: Option<&str>,
    ignore: &[String],
    keep_decorators: &[String],
    keep_bases: &[String],
) -> Result<Vec<HookBlock>> {
    let root =
        PathBuf::from(git_text(repo, &["rev-parse", "--show-toplevel"])?.trim_end_matches('\n'));
    let repo = root.as_path();
    let mut output = Vec::new();
    let mut seen = HashSet::new();
    for (base, tip) in pushed_ranges(repo, input, remote)? {
        let paths = changed_paths(repo, base.as_deref(), &tip)?;
        let ignored = ignored_paths(repo, &paths, ignore)?;
        for path in paths.into_iter().filter(|p| !ignored.contains(&p.new)) {
            let changes = if let Some(base) = &base {
                hunks(repo, base, &tip, &path)?
            } else {
                vec![Hunk {
                    old_start: 0,
                    old_count: 0,
                    new_start: 1,
                    new_count: usize::MAX,
                }]
            };
            if changes.is_empty() {
                continue;
            }
            let source = blob(repo, &tip, &path.new)?;
            let blocks = source_blocks(&source, &path.new, keep_decorators, keep_bases, true)
                .with_context(|| format!("{} in pushed tree", path.new.display()))?;
            let old_blocks = if let (Some(base), Some(old)) = (&base, &path.old) {
                let old_source = blob(repo, base, old)?;
                // A previously broken file should not stop a push that repairs its syntax.
                source_blocks(&old_source, old, keep_decorators, keep_bases, false)
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let mut parser = tree_sitter::Parser::new();
            parser.set_language(&tree_sitter_python::LANGUAGE.into())?;
            let tree = parser
                .parse(&source, None)
                .ok_or_else(|| anyhow!("Python parse failed"))?;
            for block in blocks {
                let relevant: Vec<&Hunk> = changes
                    .iter()
                    .filter(|h| changes_block(h, &block, &old_blocks))
                    .collect();
                if relevant.is_empty() {
                    continue;
                }
                // A changed code line may carry an unchanged trailing comment or inline docstring.
                if relevant.iter().all(|h| {
                    old_blocks.iter().any(|old| {
                        old.kind == block.kind
                            && old.text == block.text
                            && intersects(old.start_line, old.end_line, h.old_start, h.old_count)
                    })
                }) {
                    continue;
                }
                let ctx = context(&source, &tree, &block);
                let key = (
                    path.new.clone(),
                    block.start_line,
                    block.end_line,
                    block.kind.clone(),
                    block.text.clone(),
                    ctx.clone(),
                );
                if seen.insert(key) {
                    output.push(HookBlock {
                        path: path.new.clone(),
                        start_line: block.start_line,
                        end_line: block.end_line,
                        kind: block.kind,
                        text: block.text,
                        context: ctx,
                    });
                }
            }
        }
    }
    Ok(output)
}

const HOOK_HEADER: &str = "#!/bin/sh\n# commentreducr pre-push hook v2\n";

fn shell_quote(path: &Path) -> Result<String> {
    let path = path
        .to_str()
        .ok_or_else(|| anyhow!("hook path is not UTF-8"))?;
    Ok(format!("'{}'", path.replace('\'', "'\\''")))
}

/// Install a warning-only checker at Git's configured hook path. Preserve an existing hook,
/// its arguments, its stdin and its failure status; reinstalling our wrapper is a no-op.
pub fn install(repo: &Path) -> Result<PathBuf> {
    let path = PathBuf::from(
        git_text(
            repo,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "hooks/pre-push",
            ],
        )?
        .trim_end_matches('\n'),
    );
    let backup = path.with_file_name("pre-push.commentreducr-original");
    let mut managed = false;
    let existing = match std::fs::symlink_metadata(&path) {
        Ok(meta) => {
            if !meta.is_file() || meta.file_type().is_symlink() {
                bail!("refusing to replace non-file hook {}", path.display());
            }
            if std::fs::read_to_string(&path).is_ok_and(|s| s.starts_with(HOOK_HEADER)) {
                return Ok(path);
            }
            managed = std::fs::read_to_string(&path)
                .is_ok_and(|s| s.starts_with("#!/bin/sh\n# commentreducr pre-push hook v1\n"));
            !managed
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => return Err(err.into()),
    };
    if std::fs::symlink_metadata(&backup).is_ok() && !managed {
        bail!(
            "existing hook backup {}; preserve or move it before installing",
            backup.display()
        );
    }
    let original = if existing || (managed && backup.exists()) {
        let quoted = shell_quote(&backup)?;
        format!("if [ -x {quoted} ]; then\n    {quoted} \"$@\" < \"$input_file\" || exit $?\nfi\n")
    } else {
        String::new()
    };
    let executable = shell_quote(&std::env::current_exe()?)?;
    let script = format!(
        "{HOOK_HEADER}input_file=$(mktemp \"${{TMPDIR:-/tmp}}/commentreducr-pre-push.XXXXXX\") || exit 1\ntrap 'rm -f \"$input_file\"' 0\ntrap 'exit 1' 1 2 3 15\ncat > \"$input_file\" || exit $?\n{original}if command -v commentreducr >/dev/null 2>&1; then\n    commentreducr check --warn \"$@\" < \"$input_file\"\nelse\n    {executable} check --warn \"$@\" < \"$input_file\"\nfi\n"
    );
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("hook has no parent directory"))?;
    std::fs::create_dir_all(dir)?;
    let tmp = temp_file(dir, ".pre-push.commentreducr", script.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp.0, std::fs::Permissions::from_mode(0o755))?;
    }
    if existing {
        std::fs::rename(&path, &backup)?;
    }
    if let Err(err) = std::fs::rename(&tmp.0, &path) {
        if existing {
            let _ = std::fs::rename(&backup, &path);
        }
        return Err(err.into());
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Repo(PathBuf);

    impl Repo {
        fn new() -> Self {
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

        fn git(&self, args: &[&str]) -> String {
            git_text(&self.0, args).unwrap()
        }

        fn write(&self, path: &str, text: &str) {
            let path = self.0.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }

        fn commit(&self) -> String {
            self.git(&["add", "-A"]);
            self.git(&["commit", "-qm", "fixture"]);
            self.git(&["rev-parse", "HEAD"]).trim().to_owned()
        }

        fn blocks(&self, old: &str, new: &str) -> Vec<HookBlock> {
            let refs = format!("refs/heads/main {new} refs/heads/main {old}\n");
            changed_blocks(&self.0, Some(&refs), Some("origin"), &[], &[], &[]).unwrap()
        }
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn changed_lines_select_complete_committed_blocks_and_ignore_existing_noise() {
        let repo = Repo::new();
        let old = "# existing narration\nx = 1\n\ndef example():\n    \"\"\"Existing summary.\n    Existing continuation.\n    \"\"\"\n    # first comment line\n    # existing middle line\n    # last comment line\n    return 1  # unchanged trailing narration\n";
        repo.write("file with spaces.py", old);
        let base = repo.commit();
        let new = old
            .replace("Existing continuation.", "Changed continuation.")
            .replace("existing middle line", "changed middle line")
            .replace("return 1 ", "return 2 ");
        repo.write("file with spaces.py", &new);
        let tip = repo.commit();
        // The hook must see the committed version even with a broken, unrelated working copy.
        repo.write(
            "file with spaces.py",
            "def broken(:\n# uncommitted comment\n",
        );
        let blocks = repo.blocks(&base, &tip);
        assert_eq!(blocks.len(), 2, "{blocks:?}");
        assert_eq!(blocks[0].kind, "function_docstring");
        assert_eq!(blocks[0].text, "Existing summary.\nChanged continuation.");
        assert_eq!(
            blocks[1].text,
            "# first comment line\n# changed middle line\n# last comment line"
        );
        assert_eq!((blocks[1].start_line, blocks[1].end_line), (8, 10));
        for block in &blocks {
            assert_eq!(block.path, Path::new("file with spaces.py"));
            assert!(block.context.contains("def example():"));
            assert!(block.context.contains("return 2"));
            assert!(!block.context.contains(&block.text));
            assert!(!block.context.contains("uncommitted"));
        }
    }

    #[test]
    fn pure_deletions_renames_and_code_edits_add_no_blocks() {
        let repo = Repo::new();
        repo.write(
            "original.py",
            "# existing narration\nx = 1\n\n# delete me\n",
        );
        let base = repo.commit();
        repo.write("original.py", "# existing narration\nx = 2\n");
        let deletion = repo.commit();
        assert!(repo.blocks(&base, &deletion).is_empty());
        repo.git(&["mv", "original.py", "renamed [literal].py"]);
        let rename = repo.commit();
        assert!(repo.blocks(&deletion, &rename).is_empty());
        repo.write("renamed [literal].py", "# changed short narration\nx = 2\n");
        let modified = repo.commit();
        assert_eq!(
            repo.blocks(&rename, &modified)[0].text,
            "# changed short narration"
        );
    }

    #[test]
    fn deletion_within_a_surviving_block_checks_the_whole_remainder() {
        let fixtures = [
            (
                "comment",
                "    # first line\n    # interior line\n    # last line\n",
            ),
            (
                "function_docstring",
                "    \"\"\"\n    first line\n    interior line\n    last line\n    \"\"\"\n",
            ),
        ];
        for (kind, target) in fixtures {
            for removed in ["first line", "interior line", "last line"] {
                let repo = Repo::new();
                let source = |block: &str| {
                    format!(
                        "def example():\n{block}    return 1\n\n# unrelated existing narration\n"
                    )
                };
                repo.write("file.py", &source(target));
                let base = repo.commit();
                let deleted = if kind == "comment" {
                    format!("    # {removed}\n")
                } else {
                    format!("    {removed}\n")
                };
                let remainder = target.replace(&deleted, "");
                repo.write("file.py", &source(&remainder));
                let tip = repo.commit();
                let blocks = repo.blocks(&base, &tip);
                assert_eq!(blocks.len(), 1, "{kind} removing {removed}: {blocks:?}");
                assert_eq!(blocks[0].kind, kind);
                assert!(!blocks[0].text.contains(removed));
                assert_eq!(blocks[0].text.lines().count(), 2);
                // Removing the entire block should not select the unrelated surviving comment.
                repo.write("file.py", &source(""));
                let removed_whole = repo.commit();
                assert!(repo.blocks(&tip, &removed_whole).is_empty());
            }
        }
    }

    #[test]
    fn multiple_refs_inspect_non_head_trees_and_skip_deleted_and_duplicate_refs() {
        let repo = Repo::new();
        repo.write("file.py", "x = 1\n");
        let base = repo.commit();
        repo.git(&["checkout", "-qb", "topic"]);
        repo.write("file.py", "x = 1\n\n# short new comment\n");
        let topic = repo.commit();
        repo.git(&["checkout", "-q", "main"]);
        repo.git(&["checkout", "-qb", "other"]);
        repo.write(
            "tests/test_other.py",
            "def test_value():\n    \"\"\"Tests the value.\"\"\"\n    assert True\n",
        );
        let other = repo.commit();
        repo.git(&["checkout", "-q", "main"]);
        let zero = "0".repeat(40);
        let input = format!(
            "refs/heads/topic {topic} refs/heads/topic {base}\nrefs/heads/other {other} refs/heads/other {base}\nrefs/heads/topic {topic} refs/heads/duplicate {base}\n(delete) {zero} refs/heads/deleted {base}\n"
        );
        let blocks = changed_blocks(&repo.0, Some(&input), Some("origin"), &[], &[], &[]).unwrap();
        assert_eq!(blocks.len(), 2, "{blocks:?}");
        assert!(blocks.iter().any(|b| b.text == "# short new comment"));
        assert!(blocks.iter().any(|b| b.kind == "test_docstring"));
        assert!(
            changed_blocks(&repo.0, Some(""), Some("origin"), &[], &[], &[])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn new_branch_and_manual_checks_use_known_history_with_first_push_fallback() {
        let repo = Repo::new();
        repo.write("file.py", "# old narration\nx = 1\n");
        let base = repo.commit();
        assert_eq!(
            changed_blocks(&repo.0, None, None, &[], &[], &[])
                .unwrap()
                .len(),
            1
        );
        repo.git(&["remote", "add", "origin", "/unused-hook-test-remote"]);
        repo.git(&["update-ref", "refs/remotes/origin/main", &base]);
        repo.git(&["branch", "--set-upstream-to=origin/main", "main"]);
        repo.write("file.py", "# old narration\nx = 1\n\n# new narration\n");
        let tip = repo.commit();
        let zero = "0".repeat(40);
        let new_branch = format!("refs/heads/new {tip} refs/heads/new {zero}\n");
        let blocks =
            changed_blocks(&repo.0, Some(&new_branch), Some("origin"), &[], &[], &[]).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "# new narration");
        let direct_url = changed_blocks(
            &repo.0,
            Some(&new_branch),
            Some("/direct-push-url"),
            &[],
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(direct_url.len(), 1);
        assert_eq!(direct_url[0].text, "# new narration");
        let manual = changed_blocks(&repo.0, None, None, &[], &[], &[]).unwrap();
        assert_eq!(manual.len(), 1);
        assert_eq!(manual[0].text, "# new narration");
        repo.git(&["checkout", "-qb", "without-upstream"]);
        assert_eq!(
            changed_blocks(&repo.0, None, None, &[], &[], &[])
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn structural_exemptions_and_ignore_patterns_apply_without_a_length_gate() {
        let repo = Repo::new();
        repo.write("file.py", "\"\"\"Help shown by the parser.\"\"\"\nprint(__doc__)\n# SPDX-License-Identifier: MIT\n\n@tool\ndef agent():\n    \"\"\"Instructions for the agent.\"\"\"\n    pass\n\nclass Signature:\n    pass\n\nclass Input(Signature):\n    \"\"\"Instructions for the schema.\"\"\"\n    pass\n\ndef documented():\n    \"\"\">>> documented()\n    1\n    \"\"\"\n    return 1\n\n# noqa: E501\n\n# short narration\n");
        repo.write("vendor/skip.py", "# ignored narration\n");
        repo.write(
            "stub.pyi",
            "\"\"\"Ordinary module description.\"\"\"\nx: int\n",
        );
        repo.commit();
        let blocks = changed_blocks(
            &repo.0,
            None,
            None,
            &["vendor/".to_owned()],
            &["tool".to_owned()],
            &["Signature".to_owned()],
        )
        .unwrap();
        assert_eq!(blocks.len(), 2, "{blocks:?}");
        assert!(blocks.iter().any(|b| b.text == "# short narration"));
        assert!(
            blocks
                .iter()
                .any(|b| b.kind == "module_docstring" && b.path == Path::new("stub.pyi"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn installer_preserves_existing_hook_stdin_arguments_status_and_configured_path() {
        use std::os::unix::fs::PermissionsExt;
        let repo = Repo::new();
        repo.git(&["config", "core.hooksPath", "hooks with 'quote"]);
        let hooks = repo.0.join("hooks with 'quote");
        std::fs::create_dir_all(&hooks).unwrap();
        let original = "#!/bin/sh\ncat > original.stdin\nprintf '%s\\n' \"$@\" > original.args\nexit \"${ORIGINAL_STATUS:-0}\"\n";
        std::fs::write(hooks.join("pre-push"), original).unwrap();
        std::fs::set_permissions(
            hooks.join("pre-push"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let installed = install(&repo.0).unwrap();
        assert_eq!(
            installed.canonicalize().unwrap(),
            hooks.join("pre-push").canonicalize().unwrap()
        );
        assert_eq!(install(&repo.0).unwrap(), installed);
        assert_eq!(
            std::fs::read_to_string(hooks.join("pre-push.commentreducr-original")).unwrap(),
            original
        );
        repo.write(
            "bin/commentreducr",
            "#!/bin/sh\ncat > checker.stdin\nprintf '%s\\n' \"$@\" > checker.args\n",
        );
        std::fs::set_permissions(
            repo.0.join("bin/commentreducr"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let path = format!(
            "{}:{}",
            repo.0.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let stdin = "refs/heads/main abc refs/heads/main def\n";
        let run = |status: &str| {
            let mut child = Command::new(&installed)
                .args(["origin", "remote path"])
                .env("PATH", &path)
                .env("ORIGINAL_STATUS", status)
                .current_dir(&repo.0)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(stdin.as_bytes())
                .unwrap();
            child.wait_with_output().unwrap()
        };
        let out = run("0");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(repo.0.join("original.stdin")).unwrap(),
            stdin
        );
        assert_eq!(
            std::fs::read_to_string(repo.0.join("checker.stdin")).unwrap(),
            stdin
        );
        assert_eq!(
            std::fs::read_to_string(repo.0.join("original.args")).unwrap(),
            "origin\nremote path\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo.0.join("checker.args")).unwrap(),
            "check\n--warn\norigin\nremote path\n"
        );
        std::fs::remove_file(repo.0.join("checker.stdin")).unwrap();
        assert_eq!(run("7").status.code(), Some(7));
        assert!(!repo.0.join("checker.stdin").exists());
        let legacy = std::fs::read_to_string(&installed)
            .unwrap()
            .replace("pre-push hook v2", "pre-push hook v1")
            .replace("check --warn", "prepush-check --warn");
        std::fs::write(&installed, legacy).unwrap();
        assert_eq!(install(&repo.0).unwrap(), installed);
        let updated = std::fs::read_to_string(&installed).unwrap();
        assert!(updated.starts_with(HOOK_HEADER));
        assert!(!updated.contains("prepush-check"));
        assert!(run("0").status.success());
        assert_eq!(
            std::fs::read_to_string(hooks.join("pre-push.commentreducr-original")).unwrap(),
            original
        );
    }
}
