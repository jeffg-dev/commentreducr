#![cfg(feature = "hook")]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Repo(PathBuf);

impl Repo {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "commentreducr-hook-cli-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        let repo = Self(path);
        repo.git(&["init", "-q", "-b", "main"]);
        repo.git(&["config", "user.name", "Hook Test"]);
        repo.git(&["config", "user.email", "hook@example.invalid"]);
        repo
    }

    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.0)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn commit(&self, source: &str) -> String {
        std::fs::write(self.0.join("cues.py"), source).unwrap();
        self.git(&["add", "cues.py"]);
        self.git(&["commit", "-qm", "fixture"]);
        self.git(&["rev-parse", "HEAD"]).trim().to_owned()
    }

    fn cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_commentreducr"))
            .args(args)
            .env("COMMENTREDUCR_MODEL_PATH", model_path())
            .current_dir(&self.0)
            .output()
            .unwrap()
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn model_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("models/python-hook-minilm-l12-v1/model.onnx")
}

const SOURCE: &str = "class CueShifter:\n    def __init__(self, offset_ms, floor_ms=0):\n        self.offset_ms = offset_ms\n        self.floor_ms = floor_ms\n\n    def shift(self, start_ms, end_ms):\n        start = max(start_ms + self.offset_ms, self.floor_ms)\n        end = max(end_ms + self.offset_ms, start)\n        return start, end\n";

fn bloated_source() -> String {
    SOURCE.replacen("class CueShifter:\n", "class CueShifter:\n    \"\"\"A class for shifting subtitle timings.\n\n    It stores an offset and a floor in the constructor. The shift method adds the offset to the start and end times, then clamps them.\n    \"\"\"\n", 1)
}

#[test]
fn warnings_findings_and_errors_exit_zero_while_strict_checks_fail() {
    let repo = Repo::new();
    let base = repo.commit(SOURCE);
    repo.git(&["remote", "add", "origin", "."]);
    repo.git(&["update-ref", "refs/remotes/origin/main", &base]);
    repo.git(&["branch", "--set-upstream-to=origin/main"]);
    repo.commit(&bloated_source());
    let dirty = "not valid Python (\n";
    std::fs::write(repo.0.join("cues.py"), dirty).unwrap();

    let warning = repo.cli(&["check", "--warn"]);
    assert!(warning.status.success());
    let stdout = String::from_utf8(warning.stdout).unwrap();
    assert!(stdout.contains("cues.py:2-"), "{stdout}");
    assert!(
        stdout.contains("possible class docstring bloat"),
        "{stdout}"
    );
    assert!(warning.stderr.is_empty());
    let strict = repo.cli(&["check"]);
    assert_eq!(strict.status.code(), Some(1));
    assert_eq!(
        std::fs::read_to_string(repo.0.join("cues.py")).unwrap(),
        dirty
    );

    repo.git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    assert!(repo.cli(&["check"]).status.success());

    let mut outside = Command::new(env!("CARGO_BIN_EXE_commentreducr"));
    outside
        .args(["check", "--warn"])
        .current_dir(std::env::temp_dir());
    let warning = outside.output().unwrap();
    assert!(warning.status.success());
    assert!(
        String::from_utf8(warning.stdout)
            .unwrap()
            .contains("could not complete")
    );
    assert!(warning.stderr.is_empty());
}

#[cfg(unix)]
#[test]
fn installed_hook_reports_real_push_without_blocking_or_reading_dirty_files() {
    let repo = Repo::new();
    let remote = repo.0.join("remote.git");
    repo.git(&["init", "--bare", "-q", remote.to_str().unwrap()]);
    repo.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
    repo.commit(SOURCE);
    repo.git(&["push", "-q", "-u", "origin", "main"]);
    let pushed = repo.commit(&bloated_source());
    std::fs::write(repo.0.join("cues.py"), "broken Python (\n").unwrap();
    assert!(repo.cli(&["install-git-hook"]).status.success());
    let hook = repo.0.join(".git/hooks/pre-push");
    let contents = std::fs::read(&hook).unwrap();
    assert!(repo.cli(&["install-git-hook"]).status.success());
    assert_eq!(std::fs::read(&hook).unwrap(), contents);

    let bin = repo.0.join("bin");
    std::fs::create_dir(&bin).unwrap();
    std::os::unix::fs::symlink(
        env!("CARGO_BIN_EXE_commentreducr"),
        bin.join("commentreducr"),
    )
    .unwrap();
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let output = Command::new("git")
        .args(["push", "origin", "main"])
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("COMMENTREDUCR_MODEL_PATH", model_path())
        .current_dir(&repo.0)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("possible class docstring bloat"),
        "{stdout}"
    );
    let actual = Command::new("git")
        .args(["rev-parse", "main"])
        .current_dir(remote)
        .output()
        .unwrap();
    assert!(actual.status.success());
    assert_eq!(String::from_utf8(actual.stdout).unwrap().trim(), pushed);
}
