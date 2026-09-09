//! Enumerate git-tracked source files under a directory.
use crate::types::Language;
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

/// Runs `git ls-files -z` in `root` and returns tracked files with a supported extension, as
/// absolute paths paired with their language. `root` may be a subdirectory of the repo (only
/// files under it are returned) or a single tracked file.
///
/// Two kinds of tracked file are then dropped, via one extra `git check-ignore` call (see
/// `check_ignore`): files the repo's own gitignore rules would now exclude (e.g. `git add -f`'d,
/// or ignored after being tracked), and files matching `ignore` (gitignore-syntax patterns,
/// already merged with the shipped default by the caller).
pub fn tracked_source_files(root: &Path, ignore: &[String]) -> Result<Vec<(PathBuf, Language)>> {
    let (dir, pathspec) = if root.is_file() {
        let parent = root.parent().filter(|p| !p.as_os_str().is_empty());
        (parent.unwrap_or(Path::new(".")), root.file_name())
    } else {
        (root, None)
    };
    let output = Command::new("git")
        .arg("ls-files")
        .arg("-z")
        .args(pathspec)
        .current_dir(dir)
        .output()
        .with_context(|| format!("failed to run `git ls-files` in {}", root.display()))?;

    if !output.status.success() {
        anyhow::bail!(
            "`git ls-files` failed in {}: {}",
            root.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let dir_canon = dir
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", root.display()))?;

    let stdout =
        String::from_utf8(output.stdout).context("`git ls-files` output was not valid UTF-8")?;
    let rels: Vec<&str> = stdout.split('\0').filter(|s| !s.is_empty()).collect();

    let ignored = check_ignore(dir, &rels, ignore)?;

    let mut files = Vec::new();
    for rel in rels {
        if ignored.contains(rel) {
            continue;
        }
        let path = Path::new(rel);
        if let Some(lang) = Language::from_path(path) {
            let full = dir_canon.join(path);
            let full = full
                .canonicalize()
                .with_context(|| format!("failed to canonicalize {}", full.display()))?;
            files.push((full, lang));
        }
    }
    Ok(files)
}

/// Best-effort cleanup of the temp excludes file `check_ignore` writes, even on an early `?`
/// return.
struct TempFileGuard(Option<PathBuf>);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Writes `patterns`, one per line, to a unique temp file, for use as a one-off
/// `core.excludesFile`.
fn write_ignore_patterns(patterns: &[String]) -> Result<PathBuf> {
    let path = std::env::temp_dir().join(format!(
        "commentreducr-ignore-{}-{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let mut content = String::new();
    for p in patterns {
        content.push_str(p);
        content.push('\n');
    }
    std::fs::write(&path, content)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

/// Runs `git check-ignore --no-index -z --stdin` in `dir` over `candidates` (paths relative to
/// `dir`, as returned by `git ls-files`) and returns the subset that match. `--no-index` is what
/// makes check-ignore report on already-tracked files at all, so this catches both the repo's own
/// gitignore rules and, when `ignore` is non-empty, the extra patterns: they are written to a temp
/// file and passed as `core.excludesFile` for just this one call. Because that overrides
/// `core.excludesFile` for the call, the user's *global* excludes file is not consulted here --
/// an accepted limitation. When `ignore` is empty the temp file (and the override) is skipped
/// entirely, but check-ignore still runs to catch the repo's own rules.
///
/// Exit status 0 means at least one candidate matched, 1 means none did; anything else is an
/// error. With `-z` and `--stdin`, stdout is one matched path per NUL-terminated entry (the
/// non-verbose form).
fn check_ignore(dir: &Path, candidates: &[&str], ignore: &[String]) -> Result<HashSet<String>> {
    if candidates.is_empty() {
        return Ok(HashSet::new());
    }

    let tmp_path = if ignore.is_empty() {
        None
    } else {
        Some(write_ignore_patterns(ignore)?)
    };

    let mut cmd = Command::new("git");
    if let Some(path) = &tmp_path {
        cmd.arg("-c")
            .arg(format!("core.excludesFile={}", path.display()));
    }
    let _cleanup = TempFileGuard(tmp_path);

    cmd.args(["check-ignore", "--no-index", "-z", "--stdin"])
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().context("failed to run `git check-ignore`")?;
    // git prints matches as it reads, so feed stdin from a thread while `wait_with_output`
    // drains stdout: writing everything first would deadlock once the matched set outgrows the
    // pipe buffer (a few thousand migration files is enough).
    let mut input = Vec::new();
    for c in candidates {
        input.extend_from_slice(c.as_bytes());
        input.push(0);
    }
    let mut stdin = child.stdin.take().expect("stdin was piped");
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let output = child
        .wait_with_output()
        .context("failed to read `git check-ignore` output")?;
    // A closed pipe (git exited early) is reported through the exit status below.
    let _ = writer.join();

    match output.status.code() {
        Some(0) | Some(1) => {}
        _ => anyhow::bail!(
            "`git check-ignore` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
    }

    let stdout = String::from_utf8(output.stdout)
        .context("`git check-ignore` output was not valid UTF-8")?;
    Ok(stdout
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_on_repo_and_filters_by_extension() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let files = tracked_source_files(root, &[]).unwrap();
        // This is a Rust project; .rs files are not a supported language, so none
        // of the returned paths should end in .rs, but every path should be absolute.
        for (p, _) in &files {
            assert!(p.is_absolute());
            assert_ne!(p.extension().unwrap(), "rs");
        }
    }

    /// Minimal throwaway git repo for the ignore test below.
    struct TestRepo {
        dir: PathBuf,
    }

    static REPO_COUNTER: AtomicU64 = AtomicU64::new(0);

    impl TestRepo {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "commentreducr-files-test-{}-{}",
                std::process::id(),
                REPO_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let repo = TestRepo { dir };
            repo.git(&["init", "-q"]);
            repo
        }

        fn write(&self, rel: &str, content: &str) {
            let path = self.dir.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, content).unwrap();
        }

        fn git(&self, args: &[&str]) {
            let status = Command::new("git")
                .args(args)
                .current_dir(&self.dir)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        }
    }

    impl Drop for TestRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn ignore_patterns_and_repo_gitignore_drop_matching_files() {
        let repo = TestRepo::new();
        repo.write("a.py", "# a\n");
        repo.write("migrations/0001_x.py", "# migration\n");
        repo.write("keep/b.py", "# b\n");
        repo.write(".gitignore", "gen.py\n");
        repo.write("gen.py", "# generated\n");
        repo.git(&["add", "-A"]);
        // gen.py is excluded by the .gitignore just written; force-track it anyway so it is a
        // tracked-but-gitignored file, the case `check_ignore`'s own-rules half exists for.
        repo.git(&["add", "-f", "gen.py"]);
        repo.git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "init",
        ]);

        let root = repo.dir.canonicalize().unwrap();
        let rel_names = |files: &[(PathBuf, Language)]| -> Vec<String> {
            let mut names: Vec<String> = files
                .iter()
                .map(|(p, _)| {
                    p.strip_prefix(&root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/")
                })
                .collect();
            names.sort();
            names
        };

        let files = tracked_source_files(&root, &["migrations/".to_string()]).unwrap();
        assert_eq!(rel_names(&files), vec!["a.py", "keep/b.py"]);

        // Empty ignore list: migrations/ comes back, but gen.py (tracked-but-gitignored) never
        // does -- that half comes from the repo's own gitignore rules, not the `ignore` config.
        let files = tracked_source_files(&root, &[]).unwrap();
        assert_eq!(
            rel_names(&files),
            vec!["a.py", "keep/b.py", "migrations/0001_x.py"]
        );
    }
}
