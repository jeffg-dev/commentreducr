//! End-to-end CLI tests over tests/fixtures without a live LLM.
//!
//! `comments` subcommand: --delete (correct and idempotent), --delete --dry-run (counts, writes
//! nothing), and --reduce against a dead endpoint (hard failure, writes nothing).
//!
//! `docstrings` subcommand: same shape, against tests/fixtures/test_sample.py (plus sample.py,
//! which is a Python file but not part of the docstrings test's own fixture set).
use assert_cmd::Command;
use std::path::Path;

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
];

fn git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
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
fn setup_repo() -> tempfile::TempDir {
    let dir = setup_repo_uncommitted();
    commit_all(dir.path());
    dir
}

/// Like `setup_repo`, plus tests/fixtures/test_sample.py: a pytest-shaped Python module used by
/// the docstrings tests below. It is deliberately kept out of `FIXTURES` (and so out of the
/// generic comment assertions above) so those tests' file counts stay exactly as before.
fn setup_docstrings_repo() -> tempfile::TempDir {
    let dir = setup_repo_uncommitted();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test_sample.py"),
        dir.path().join("test_sample.py"),
    )
    .unwrap();
    commit_all(dir.path());
    dir
}

fn setup_repo_uncommitted() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
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
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok()
}

#[test]
fn delete_mode_removes_non_structural_comments_and_is_idempotent() {
    let dir = setup_repo();

    Command::cargo_bin("commentreducr")
        .unwrap()
        .arg("comments")
        .arg(dir.path())
        .arg("--delete")
        .assert()
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

    // Idempotent: running --delete again changes nothing.
    let before: Vec<String> = FIXTURES.iter().map(|f| read(dir.path(), f.name)).collect();
    Command::cargo_bin("commentreducr")
        .unwrap()
        .arg("comments")
        .arg(dir.path())
        .arg("--delete")
        .assert()
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

fn snapshot(dir: &Path) -> Vec<String> {
    FIXTURES.iter().map(|f| read(dir, f.name)).collect()
}

#[test]
fn delete_dry_run_counts_but_writes_nothing() {
    let dir = setup_repo();
    let before = snapshot(dir.path());

    Command::cargo_bin("commentreducr")
        .unwrap()
        .arg("comments")
        .arg(dir.path())
        .arg("--delete")
        .arg("--dry-run")
        .assert()
        .success()
        .stdout(predicates::str::contains(": delete "))
        .stderr(predicates::str::contains(
            "5 files scanned, 5 changed, 0 skipped",
        ));

    assert_eq!(snapshot(dir.path()), before, "dry run modified files");
}

#[test]
fn broken_file_is_skipped_and_others_still_processed() {
    let dir = setup_repo();
    std::fs::write(dir.path().join("bad.py"), b"# comment\ndef f(:\n  \xff\n").unwrap();
    std::process::Command::new("git")
        .args(["add", "-A"])
        .current_dir(dir.path())
        .status()
        .unwrap();

    Command::cargo_bin("commentreducr")
        .unwrap()
        .arg("comments")
        .arg(dir.path())
        .arg("--delete")
        .assert()
        .code(1)
        .stderr(predicates::str::contains("warning: skipping"))
        .stderr(predicates::str::contains(
            "6 files scanned, 5 changed, 1 skipped",
        ));

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
    Command::cargo_bin("commentreducr")
        .unwrap()
        .arg("comments")
        .arg(dir.path())
        .arg("--dry-run")
        .assert()
        .failure()
        .stderr(predicates::str::contains("--delete"));
}

#[test]
fn reduce_fails_hard_when_llm_unreachable() {
    let dir = setup_repo();
    let before = snapshot(dir.path());

    Command::cargo_bin("commentreducr")
        .unwrap()
        .arg("comments")
        .arg(dir.path())
        .arg("--reduce")
        .arg("--config")
        .arg(dir.path().join("no-such-config.toml"))
        .arg("--endpoint")
        .arg("http://127.0.0.1:1/v1")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "cannot reach LLM at http://127.0.0.1:1/v1",
        ));

    assert_eq!(snapshot(dir.path()), before, "failed reduce modified files");
}

#[test]
fn missing_subcommand_fails_with_usage_error() {
    let dir = setup_repo();
    Command::cargo_bin("commentreducr")
        .unwrap()
        .arg(dir.path())
        .arg("--delete")
        .assert()
        .failure()
        .stderr(predicates::str::contains("subcommand"))
        .stderr(predicates::str::contains("Usage: commentreducr <COMMAND>"));
}

#[test]
fn docstrings_delete_removes_docstrings_and_is_idempotent() {
    let dir = setup_docstrings_repo();

    Command::cargo_bin("commentreducr")
        .unwrap()
        .arg("docstrings")
        .arg(dir.path())
        .arg("--delete")
        .assert()
        .success()
        .stderr(predicates::str::contains("docstrings:"))
        .stderr(predicates::str::contains("2 files scanned"));

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
        let status = std::process::Command::new("python3")
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

    // Idempotent: running docstrings --delete again changes nothing.
    let before = (
        read(dir.path(), "test_sample.py"),
        read(dir.path(), "sample.py"),
    );
    Command::cargo_bin("commentreducr")
        .unwrap()
        .arg("docstrings")
        .arg(dir.path())
        .arg("--delete")
        .assert()
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

    Command::cargo_bin("commentreducr")
        .unwrap()
        .arg("docstrings")
        .arg(dir.path())
        .arg("--delete")
        .arg("--dry-run")
        .assert()
        .success()
        .stdout(predicates::str::contains("delete docstring"));

    assert_eq!(
        read(dir.path(), "test_sample.py"),
        before,
        "dry run modified files"
    );
}

#[test]
fn docstrings_reduce_fails_hard_when_llm_unreachable() {
    let dir = setup_docstrings_repo();
    let before = read(dir.path(), "test_sample.py");

    Command::cargo_bin("commentreducr")
        .unwrap()
        .arg("docstrings")
        .arg(dir.path())
        .arg("--reduce")
        .arg("--config")
        .arg(dir.path().join("no-such-config.toml"))
        .arg("--endpoint")
        .arg("http://127.0.0.1:1/v1")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "cannot reach LLM at http://127.0.0.1:1/v1",
        ));

    assert_eq!(
        read(dir.path(), "test_sample.py"),
        before,
        "failed reduce modified files"
    );
}

#[test]
fn comments_delete_leaves_docstrings_in_test_sample_intact() {
    let dir = setup_docstrings_repo();
    let before = read(dir.path(), "test_sample.py");

    Command::cargo_bin("commentreducr")
        .unwrap()
        .arg("comments")
        .arg(dir.path())
        .arg("--delete")
        .assert()
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
            "comments --delete touched a docstring: lost {survives:?}"
        );
    }
    // The plain `# comment` line, on the other hand, is a non-structural comment and is exactly
    // what the comments target is for.
    assert!(!after.contains("# comment"), "{after}");
    assert_ne!(
        before, after,
        "comments --delete should have removed # comment"
    );
}
