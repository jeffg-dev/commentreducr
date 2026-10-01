//! End-to-end CLI tests over tests/fixtures without a live LLM.
//!
//! Covers deletion safety, idempotence, dry runs, scope/language filters, and configuration.
//! Classifier-assisted reduction and interruption recovery are exercised in reduce.rs.
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicU64, Ordering};

/// Minimal RAII temp directory: a unique path under the system temp dir, created eagerly and
/// removed (best-effort) on drop. Stands in for `tempfile::TempDir` without the dependency.
struct TempDir {
    path: PathBuf,
}

static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "commentreducr-test-{}-{}",
            std::process::id(),
            TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        TempDir { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Captured output of a finished child process, with small chainable assertions used in place of
/// `assert_cmd`/`predicates`. Each assertion panics (with full stdout/stderr for context) on
/// mismatch and returns `self`, so calls chain like the old `.assert()...` calls did.
struct Ran {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

fn run(cmd: &mut Command) -> Ran {
    let output = cmd.output().expect("failed to run command");
    Ran {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

impl Ran {
    fn fail(&self, msg: &str) -> ! {
        panic!(
            "{msg}\nstatus: {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.status, self.stdout, self.stderr
        );
    }

    fn success(self) -> Self {
        if !self.status.success() {
            self.fail("expected success");
        }
        self
    }

    fn failure(self) -> Self {
        if self.status.success() {
            self.fail("expected failure");
        }
        self
    }

    fn code(self, n: i32) -> Self {
        if self.status.code() != Some(n) {
            let msg = format!("expected exit code {n}, got {:?}", self.status.code());
            self.fail(&msg);
        }
        self
    }

    fn stdout_contains(self, s: &str) -> Self {
        if !self.stdout.contains(s) {
            let msg = format!("expected stdout to contain {s:?}");
            self.fail(&msg);
        }
        self
    }

    fn stderr_contains(self, s: &str) -> Self {
        if !self.stderr.contains(s) {
            let msg = format!("expected stderr to contain {s:?}");
            self.fail(&msg);
        }
        self
    }
}

fn commentreducr() -> Command {
    Command::new(env!("CARGO_BIN_EXE_commentreducr"))
}

struct Fixture {
    name: &'static str,
    init_comment: &'static str,
    trailing_remark: &'static str,
    /// Text that must survive both modes byte-for-byte: strings, docstring/JSDoc, license, directives.
    survive: &'static [&'static str],
}

const FIXTURES: &[Fixture] = &[
    Fixture {
        name: "sample.py",
        init_comment: "# init",
        trailing_remark: "# trailing remark about the return shape",
        survive: &[
            "# Copyright 2024 Example Corp. Licensed under the MIT License.",
            "# SPDX-License-Identifier: MIT",
            "\"\"\"Module docstring: a small stats helper, not a comment at all.\"\"\"",
            "\"# not a comment\"",
            "# noqa: E501",
        ],
    },
    Fixture {
        name: "sample.js",
        init_comment: "// init",
        trailing_remark: "// trailing remark about the divisor",
        survive: &[
            "// Copyright 2024 Example Corp. Licensed under the MIT License.",
            "// SPDX-License-Identifier: MIT",
            "\"// not a comment\"",
            "`value is // not a comment either: ${marker}`",
            "/\\/\\/ still not a comment/",
            "// eslint-disable-next-line no-unused-vars",
        ],
    },
    Fixture {
        name: "sample.ts",
        init_comment: "// init",
        trailing_remark: "// trailing remark about the divisor",
        survive: &[
            "// Copyright 2024 Example Corp. Licensed under the MIT License.",
            "// SPDX-License-Identifier: MIT",
            "* Computes a running mean and variance over a stream of numbers.",
            "\"// not a comment\"",
            "`value is // not a comment either: ${marker}`",
            "/\\/\\/ still not a comment/",
            "// @ts-ignore",
        ],
    },
    Fixture {
        name: "sample.tsx",
        init_comment: "// init",
        trailing_remark: "// trailing remark about the divisor",
        survive: &[
            "// Copyright 2024 Example Corp. Licensed under the MIT License.",
            "// SPDX-License-Identifier: MIT",
            "* Renders a small stats summary panel.",
            "\"// not a comment\"",
            "`value is // not a comment either: ${marker}`",
            "/\\/\\/ still not a comment/",
            "// @ts-ignore",
            "Path notation uses // as a separator, not a comment",
        ],
    },
    Fixture {
        name: "sample.yaml",
        init_comment: "# init",
        trailing_remark: "# trailing remark about the divisor",
        survive: &[
            "# Copyright 2024 Example Corp. Licensed under the MIT License.",
            "# SPDX-License-Identifier: MIT",
            "# yaml-language-server: $schema=https://json.schemastore.org/github-workflow.json",
            "# TODO: keep me",
            "# yamllint disable-line rule:line-length",
            "count: 5  # noqa",
            "\"# not a comment\"",
            "      echo \"starting build\"\n      # not a comment\n      exit 0",
            "a#b",
        ],
    },
    Fixture {
        name: "sample.rs",
        init_comment: "// init",
        trailing_remark: "// trailing remark about the divisor",
        survive: &[
            "// Copyright 2024 Example Corp. Licensed under the MIT License.",
            "// SPDX-License-Identifier: MIT",
            "//! A small stats helper.",
            "/// assert_eq!(sample::compute_stats(&[1.0, 3.0]), (2.0, 1.0));",
            "/// Sample input.",
            "\"// not a comment\"",
            "r#\"/* not a comment */ \"quoted\" // either\"#",
            "// SAFETY: the bytes are an ASCII literal, so they are valid UTF-8.",
            "b\"// still not a comment\"",
            "'/'",
        ],
    },
];

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

fn commit_all(dir: &Path) {
    git(dir, &["init", "-q"]);
    git(dir, &["add", "-A"]);
    git(
        dir,
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "fixtures",
        ],
    );
}

/// Copies tests/fixtures into a fresh temp dir and commits them, so `git ls-files` sees them.
fn setup_repo() -> TempDir {
    let dir = setup_repo_uncommitted();
    commit_all(dir.path());
    dir
}

/// Like `setup_repo`, plus tests/fixtures/test_sample.py: a pytest-shaped Python module used by
/// the docstrings tests below. It is deliberately kept out of `FIXTURES` (and so out of the
/// generic comment assertions above) so those tests' file counts stay exactly as before.
fn setup_docstrings_repo() -> TempDir {
    let dir = setup_repo_uncommitted();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test_sample.py"),
        dir.path().join("test_sample.py"),
    )
    .unwrap();
    commit_all(dir.path());
    dir
}

fn setup_repo_uncommitted() -> TempDir {
    let dir = TempDir::new();
    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for f in FIXTURES {
        std::fs::copy(fixtures_dir.join(f.name), dir.path().join(f.name)).unwrap();
    }
    dir
}

fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap()
}

fn python3_available() -> bool {
    Command::new("python3").arg("--version").output().is_ok()
}

fn rustc_available() -> bool {
    Command::new("rustc").arg("--version").output().is_ok()
}

#[test]
fn delete_mode_removes_non_structural_comments_and_is_idempotent() {
    let dir = setup_repo();

    run(commentreducr()
        .arg("delete")
        .args(["--scope", "comments"])
        .arg(dir.path()))
    .success();

    for f in FIXTURES {
        let content = read(dir.path(), f.name);
        for s in f.survive {
            assert!(content.contains(s), "{}: lost {:?}", f.name, s);
        }
        assert!(
            !content.contains("This function walks the list of samples"),
            "{}: big block not deleted",
            f.name
        );
        assert!(
            !content.contains(f.init_comment),
            "{}: short comment not deleted",
            f.name
        );
        assert!(
            !content.contains(f.trailing_remark),
            "{}: trailing comment not deleted",
            f.name
        );
    }

    // Idempotent: running delete again changes nothing.
    let before: Vec<String> = FIXTURES.iter().map(|f| read(dir.path(), f.name)).collect();
    run(commentreducr()
        .arg("delete")
        .args(["--scope", "comments"])
        .arg(dir.path()))
    .success();
    for (f, before) in FIXTURES.iter().zip(before) {
        assert_eq!(
            read(dir.path(), f.name),
            before,
            "{}: not idempotent",
            f.name
        );
    }
}

/// sample.rs after `delete` still compiles, and its `#![deny(missing_docs)]` makes rustc
/// itself confirm that every doc comment survived.
#[test]
fn rust_delete_output_still_compiles() {
    let dir = setup_repo();
    run(commentreducr()
        .arg("delete")
        .args(["--scope", "comments"])
        .arg(dir.path()))
    .success();

    let after = read(dir.path(), "sample.rs");
    assert!(after.contains("    (mean, variance)\n"), "{after}");
    if rustc_available() {
        let status = Command::new("rustc")
            .args([
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--emit=metadata",
            ])
            .arg("-o")
            .arg(dir.path().join("sample.rmeta"))
            .arg(dir.path().join("sample.rs"))
            .status()
            .unwrap();
        assert!(
            status.success(),
            "rustc rejected the rewritten file:\n{after}"
        );
    }
}

fn snapshot(dir: &Path) -> Vec<String> {
    FIXTURES.iter().map(|f| read(dir, f.name)).collect()
}

#[test]
fn delete_dry_run_counts_but_writes_nothing() {
    let dir = setup_repo();
    let before = snapshot(dir.path());

    run(commentreducr()
        .arg("delete")
        .args(["--scope", "comments"])
        .arg(dir.path())
        .arg("--dry-run"))
    .success()
    .stdout_contains(" deleted (")
    .stderr_contains("6 files scanned, 6 changed, 0 skipped");

    assert_eq!(snapshot(dir.path()), before, "dry run modified files");
}

#[test]
fn broken_file_is_skipped_and_others_still_processed() {
    let dir = setup_repo();
    std::fs::write(dir.path().join("bad.py"), b"# comment\ndef f(:\n  \xff\n").unwrap();
    Command::new("git")
        .args(["add", "-A"])
        .current_dir(dir.path())
        .status()
        .unwrap();

    run(commentreducr()
        .arg("delete")
        .args(["--scope", "comments"])
        .arg(dir.path()))
    .code(1)
    .stderr_contains("warning: skipping")
    .stderr_contains("7 files scanned, 6 changed, 1 skipped");

    for f in FIXTURES {
        assert!(
            !read(dir.path(), f.name).contains(f.init_comment),
            "{}: not processed",
            f.name
        );
    }
}

#[test]
fn dry_run_requires_delete() {
    let dir = setup_repo();
    run(commentreducr()
        .arg("reduce")
        .args(["--scope", "comments"])
        .arg(dir.path())
        .arg("--dry-run"))
    .failure()
    .stderr_contains("--dry-run only applies to delete");
}

#[test]
fn dry_run_with_explicit_reduce_is_also_rejected() {
    // Reject before inference or writes, even when an endpoint is explicitly configured.
    let dir = setup_repo();
    let before = snapshot(dir.path());

    run(commentreducr()
        .arg("reduce")
        .args(["--scope", "comments"])
        .arg(dir.path())
        .arg("--dry-run")
        .arg("--endpoint")
        .arg("http://127.0.0.1:1/v1"))
    .failure()
    .stderr_contains("--dry-run only applies to delete");

    assert_eq!(snapshot(dir.path()), before, "rejected run modified files");
}

#[test]
fn missing_subcommand_fails_with_usage_error() {
    let dir = setup_repo();
    run(commentreducr().arg(dir.path()).arg("--delete"))
        .failure()
        .stderr_contains("subcommand")
        .stderr_contains("Usage: commentreducr <COMMAND>");
}

#[test]
fn docstrings_delete_removes_docstrings_and_is_idempotent() {
    let dir = setup_docstrings_repo();

    run(commentreducr()
        .arg("delete")
        .args(["--scope", "docstrings"])
        .arg(dir.path()))
    .success()
    .stderr_contains("docstrings:")
    .stderr_contains("2 files scanned");

    let after = read(dir.path(), "test_sample.py");

    // Every real (non-structural) docstring is gone.
    for gone in [
        "consolidate a handful of near-duplicate helper functions",
        "Normalizes a list of items before they are compared",
        "Guards the empty-list case.",
        "Ensures mixed-case, whitespace-padded items are normalized",
        "Placeholder test class reserved for future thing-related tests.",
    ] {
        assert!(
            !after.contains(gone),
            "docstring survived deletion: {gone:?}\n{after}"
        );
    }

    // The doctest docstring is structural and must survive.
    assert!(
        after.contains(">>> add(1, 2)"),
        "doctest docstring lost\n{after}"
    );

    // Strings that are not docstrings (an assignment, a bare non-first string, an f-string)
    // are never touched.
    assert!(after.contains("MESSAGE = \"\"\"not a docstring\"\"\""));
    assert!(
        after.contains("\"\"\"also not a docstring, the bare string after an assignment\"\"\"")
    );
    assert!(after.contains("f\"\"\"this looks like a docstring but is an f-string, not one\"\"\""));

    // Comments are untouched by the docstrings target.
    assert!(after.contains("# comment"));

    // TestThing's only statement (its docstring) becomes `pass` at the right indent.
    assert!(after.contains("class TestThing:\n    pass\n"), "{after}");
    // Widget.probe's inline-with-code docstring becomes `pass` too.
    assert!(after.contains("def probe(self): pass"), "{after}");

    // The rewritten file still parses cleanly.
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(after.as_bytes(), None).unwrap();
    assert!(
        !tree.root_node().has_error(),
        "rewritten file has parse errors:\n{after}"
    );
    if python3_available() {
        let status = Command::new("python3")
            .args(["-m", "py_compile"])
            .arg(dir.path().join("test_sample.py"))
            .status()
            .unwrap();
        assert!(
            status.success(),
            "python3 -m py_compile failed on the rewritten file"
        );
    }

    // sample.py's comments (a separate Python file in the same repo) are untouched by the
    // docstrings target; only its own (non-structural) module docstring is removed.
    let sample_py = read(dir.path(), "sample.py");
    for comment in [
        "# Copyright 2024 Example Corp. Licensed under the MIT License.",
        "# SPDX-License-Identifier: MIT",
        "# noqa: E501",
        "# init",
        "# trailing remark about the return shape",
        "This function walks the list of samples",
    ] {
        assert!(
            sample_py.contains(comment),
            "sample.py comment lost: {comment:?}"
        );
    }
    assert!(
        !sample_py.contains("Module docstring: a small stats helper"),
        "sample.py's non-structural module docstring should be gone"
    );

    // Idempotent: running delete --scope docstrings again changes nothing.
    let before = (
        read(dir.path(), "test_sample.py"),
        read(dir.path(), "sample.py"),
    );
    run(commentreducr()
        .arg("delete")
        .args(["--scope", "docstrings"])
        .arg(dir.path()))
    .success();
    assert_eq!(
        read(dir.path(), "test_sample.py"),
        before.0,
        "test_sample.py: not idempotent"
    );
    assert_eq!(
        read(dir.path(), "sample.py"),
        before.1,
        "sample.py: not idempotent"
    );
}

#[test]
fn docstrings_delete_dry_run_prints_and_writes_nothing() {
    let dir = setup_docstrings_repo();
    let before = read(dir.path(), "test_sample.py");

    run(commentreducr()
        .arg("delete")
        .args(["--scope", "docstrings"])
        .arg(dir.path())
        .arg("--dry-run"))
    .success()
    .stdout_contains("test_sample.py: ")
    .stdout_contains(" deleted (");

    assert_eq!(
        read(dir.path(), "test_sample.py"),
        before,
        "dry run modified files"
    );
}

#[test]
fn comments_delete_leaves_docstrings_in_test_sample_intact() {
    let dir = setup_docstrings_repo();
    let before = read(dir.path(), "test_sample.py");

    run(commentreducr()
        .arg("delete")
        .args(["--scope", "comments"])
        .arg(dir.path()))
    .success();

    let after = read(dir.path(), "test_sample.py");
    for survives in [
        "consolidate a handful of near-duplicate helper functions",
        "Normalizes a list of items before they are compared",
        "Guards the empty-list case.",
        "Ensures mixed-case, whitespace-padded items are normalized",
        "Placeholder test class reserved for future thing-related tests.",
        "def probe(self): \"\"\"inline\"\"\"",
    ] {
        assert!(
            after.contains(survives),
            "delete --scope comments touched a docstring: lost {survives:?}"
        );
    }
    // The plain `# comment` line, on the other hand, is a non-structural comment and is exactly
    // what the comments target is for.
    assert!(!after.contains("# comment"), "{after}");
    assert_ne!(
        before, after,
        "delete --scope comments should have removed # comment"
    );
}

/// `ignore` skips files even though `git ls-files` tracks them: the shipped default
/// (`migrations/`) applies with no config at all, and a config's `ignore` list adds to that
/// default instead of replacing it.
#[test]
fn ignore_skips_default_migrations_and_user_configured_patterns() {
    let dir = TempDir::new();
    let comment = "# a deletable comment describing nothing structural, just filler prose\n";
    let src = format!("{comment}def f():\n    pass\n");
    std::fs::write(dir.path().join("keep.py"), &src).unwrap();
    std::fs::create_dir_all(dir.path().join("migrations")).unwrap();
    std::fs::write(dir.path().join("migrations/foo.py"), &src).unwrap();
    commit_all(dir.path());

    // No user config: the shipped default `ignore = ["migrations/"]` alone skips
    // migrations/foo.py, so only keep.py is scanned and changed.
    run(commentreducr()
        .arg("delete")
        .args(["--scope", "comments"])
        .arg(dir.path())
        .arg("--config")
        .arg(dir.path().join("no-such-config.toml")))
    .success()
    .stderr_contains("1 files scanned, 1 changed, 0 skipped");
    assert_eq!(
        read(dir.path(), "migrations/foo.py"),
        src,
        "migrations/ file should not have been scanned"
    );
    assert!(
        !read(dir.path(), "keep.py").contains("a deletable comment"),
        "keep.py should have been processed"
    );

    // Add a legacy/ file and reset keep.py, then rerun with a config `ignore = ["legacy/"]`:
    // it should add to, not replace, the shipped default, so both migrations/ and legacy/ stay
    // untouched while keep.py is processed again.
    std::fs::write(dir.path().join("keep.py"), &src).unwrap();
    std::fs::create_dir_all(dir.path().join("legacy")).unwrap();
    std::fs::write(dir.path().join("legacy/bar.py"), &src).unwrap();
    git(dir.path(), &["add", "-A"]);
    git(
        dir.path(),
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "legacy",
        ],
    );

    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "ignore = [\"legacy/\"]\n").unwrap();
    run(commentreducr()
        .arg("delete")
        .args(["--scope", "comments"])
        .arg(dir.path())
        .arg("--config")
        .arg(&config_path))
    .success()
    .stderr_contains("1 files scanned, 1 changed, 0 skipped");

    assert_eq!(
        read(dir.path(), "migrations/foo.py"),
        src,
        "migrations/ file should still not have been scanned"
    );
    assert_eq!(
        read(dir.path(), "legacy/bar.py"),
        src,
        "legacy/ file should not have been scanned"
    );
    assert!(
        !read(dir.path(), "keep.py").contains("a deletable comment"),
        "keep.py should have been processed again"
    );
}

/// A `@tool`-decorated function (Strands), a `dspy.Signature` subclass, and a `BaseModel`
/// subclass all have their docstrings sent to an LLM at runtime -- `delete` must leave them
/// byte-for-byte untouched, unlike a plain function's docstring in the same file.
#[test]
fn docstrings_delete_keeps_agentic_docstrings() {
    let dir = TempDir::new();
    let src = r#"from strands import tool


@tool
def do_thing(x: int) -> int:
    """Adds one to x.

    This text is sent to the model as the tool description.
    """
    return x + 1


class Q(dspy.Signature):
    """Answers a question, given some context."""

    question: str = dspy.InputField()


class M(BaseModel):
    """A structured-output schema sent to the model."""

    name: str


def helper(x):
    """This docstring says nothing the code doesn't already say."""
    return x * 2
"#;
    std::fs::write(dir.path().join("tools.py"), src).unwrap();
    commit_all(dir.path());

    run(commentreducr()
        .arg("delete")
        .args(["--scope", "docstrings"])
        .arg(dir.path())
        .arg("--config")
        .arg(dir.path().join("no-such-config.toml")))
    .success();

    let after = read(dir.path(), "tools.py");
    assert!(
        after.contains(
            "Adds one to x.\n\n    This text is sent to the model as the tool description."
        ),
        "strands @tool docstring lost:\n{after}"
    );
    assert!(
        after.contains("Answers a question, given some context."),
        "dspy.Signature class docstring lost:\n{after}"
    );
    assert!(
        after.contains("A structured-output schema sent to the model."),
        "pydantic BaseModel class docstring lost:\n{after}"
    );
    assert!(
        !after.contains("This docstring says nothing the code doesn't already say."),
        "plain function docstring should have been deleted:\n{after}"
    );
}

/// A config's `keep_decorators` list adds to the shipped default rather than replacing it: a
/// `@pytest.fixture` docstring (not covered by the default `tool`/`command`/`group` list) is
/// deleted with no config, and kept once the config adds `fixture`.
#[test]
fn docstrings_delete_keep_decorators_config_extends_default() {
    let src = r#"import pytest


@pytest.fixture
def sample_data():
    """Provides the sample data every test in this module reuses."""
    return {"a": 1}
"#;

    let dir = TempDir::new();
    std::fs::write(dir.path().join("fixtures.py"), src).unwrap();
    commit_all(dir.path());
    run(commentreducr()
        .arg("delete")
        .args(["--scope", "docstrings"])
        .arg(dir.path())
        .arg("--config")
        .arg(dir.path().join("no-such-config.toml")))
    .success();
    assert!(
        !read(dir.path(), "fixtures.py").contains("Provides the sample data"),
        "fixture docstring should be deleted without a keep_decorators config"
    );

    let dir = TempDir::new();
    std::fs::write(dir.path().join("fixtures.py"), src).unwrap();
    commit_all(dir.path());
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "keep_decorators = [\"fixture\"]\n").unwrap();
    run(commentreducr()
        .arg("delete")
        .args(["--scope", "docstrings"])
        .arg(dir.path())
        .arg("--config")
        .arg(&config_path))
    .success();
    assert!(
        read(dir.path(), "fixtures.py")
            .contains("Provides the sample data every test in this module reuses."),
        "fixture docstring should survive with keep_decorators = [\"fixture\"] in the config"
    );
}

#[test]
fn language_and_scope_filters_combine_and_flags_override_config() {
    let dir = TempDir::new();
    let python = "\"\"\"Narration.\"\"\"\n# Narration.\ndef f():\n    \"\"\"Return a constant.\"\"\"\n    # Read the constant.\n    return 1\n";
    for (name, source) in [
        ("item.py", python),
        ("item.ts", "// narration\nconst a = 1;\n"),
        ("item.tsx", "// narration\nconst a = <div/>;\n"),
        ("item.js", "// narration\nconst a = 1;\n"),
        ("item.rs", "// narration\nfn f() {}\n"),
    ] {
        std::fs::write(dir.path().join(name), source).unwrap();
    }
    commit_all(dir.path());
    run(commentreducr().arg("delete").arg(dir.path()).args([
        "--scope",
        "all",
        "--language",
        "typescript",
    ]))
    .success()
    .stderr_contains("2 files scanned, 2 changed");
    assert_eq!(read(dir.path(), "item.py"), python);
    assert_eq!(read(dir.path(), "item.ts"), "const a = 1;\n");
    assert_eq!(read(dir.path(), "item.tsx"), "const a = <div/>;\n");
    assert!(read(dir.path(), "item.js").starts_with("// narration"));
    let config = dir.path().join("config.toml");
    std::fs::write(&config, "scope = \"docstrings\"\nlanguage = \"python\"\n").unwrap();
    run(commentreducr()
        .arg("delete")
        .arg(dir.path())
        .arg("--config")
        .arg(&config)
        .args(["--scope", "comments", "--language", "rust"]))
    .success()
    .stderr_contains("1 files scanned, 1 changed");
    assert_eq!(read(dir.path(), "item.py"), python);
    assert_eq!(read(dir.path(), "item.rs"), "fn f() {}\n");
    run(commentreducr().arg("delete").arg(dir.path()).args([
        "--scope",
        "all",
        "--language",
        "python",
    ]))
    .success();
    assert_eq!(read(dir.path(), "item.py"), "def f():\n    return 1\n");
}

#[cfg(not(feature = "hook"))]
#[test]
fn reduce_without_optional_feature_explains_install_and_leaves_files_intact() {
    let dir = setup_repo();
    let before = snapshot(dir.path());
    run(commentreducr().arg("reduce").arg(dir.path()))
        .failure()
        .stderr_contains("cargo install commentreducr --features hook");
    assert_eq!(snapshot(dir.path()), before);
}
