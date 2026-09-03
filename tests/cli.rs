//! End-to-end CLI tests over tests/fixtures without a live LLM: --delete (correct and idempotent),
//! --delete --dry-run (counts, writes nothing), and --reduce against a dead endpoint (hard failure,
//! writes nothing).
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
];

/// Copies tests/fixtures into a fresh temp dir and commits them, so `git ls-files` sees them.
fn setup_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for f in FIXTURES {
        std::fs::copy(fixtures_dir.join(f.name), dir.path().join(f.name)).unwrap();
    }
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["add", "-A"]);
    git(&[
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
    ]);
    dir
}

fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap()
}

#[test]
fn delete_mode_removes_non_structural_comments_and_is_idempotent() {
    let dir = setup_repo();

    Command::cargo_bin("commentreducr")
        .unwrap()
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
        .arg(dir.path())
        .arg("--delete")
        .arg("--dry-run")
        .assert()
        .success()
        .stdout(predicates::str::contains(": delete "))
        .stderr(predicates::str::contains("4 files scanned, 4 changed"));

    assert_eq!(snapshot(dir.path()), before, "dry run modified files");
}

#[test]
fn dry_run_requires_delete() {
    let dir = setup_repo();
    Command::cargo_bin("commentreducr")
        .unwrap()
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
