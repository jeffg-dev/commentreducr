//! End-to-end CLI test over tests/fixtures: --delete and --no-llm (reduce), each checked for
//! correctness and idempotency, without touching the live LLM.
use assert_cmd::Command;
use std::path::Path;

struct Fixture {
    name: &'static str,
    prefix: &'static str,
    block_start_anchor: &'static str,
    block_end_anchor: &'static str,
    indent: &'static str,
    init_comment: &'static str,
    trailing_remark: &'static str,
    /// Text that must survive both modes byte-for-byte: strings, docstring/JSDoc, license, directives.
    survive: &'static [&'static str],
}

const FIXTURES: &[Fixture] = &[
    Fixture {
        name: "sample.py",
        prefix: "#",
        block_start_anchor: "    # This function walks",
        block_end_anchor: "    count = 0\n",
        indent: "    ",
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
        prefix: "//",
        block_start_anchor: "    // This function walks",
        block_end_anchor: "    let count = 0;\n",
        indent: "    ",
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
        prefix: "//",
        block_start_anchor: "    // This function walks",
        block_end_anchor: "    let count = 0;\n",
        indent: "    ",
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
        prefix: "//",
        block_start_anchor: "    // This function walks",
        block_end_anchor: "    let count = 0;\n",
        indent: "    ",
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
        assert_eq!(read(dir.path(), f.name), before, "{}: not idempotent", f.name);
    }
}

#[test]
fn reduce_mode_collapses_big_block_and_is_idempotent() {
    let dir = setup_repo();
    let originals: Vec<String> = FIXTURES.iter().map(|f| read(dir.path(), f.name)).collect();

    Command::cargo_bin("commentreducr")
        .unwrap()
        .arg(dir.path())
        .arg("--no-llm")
        .assert()
        .success();

    for (f, original) in FIXTURES.iter().zip(&originals) {
        let updated = read(dir.path(), f.name);

        // Everything outside the big block is untouched: strings, docstring/JSDoc, license,
        // directives, the short comment and the trailing comment all survive verbatim.
        for s in f.survive {
            assert!(updated.contains(s), "{}: lost {:?}", f.name, s);
        }
        assert!(updated.contains(f.init_comment), "{}: short comment changed", f.name);
        assert!(updated.contains(f.trailing_remark), "{}: trailing comment changed", f.name);

        // The big block collapsed to exactly one `{indent}{prefix} <summary>` line; the file
        // before the block and after it is byte-identical.
        let start = original.find(f.block_start_anchor).unwrap();
        let end = original.find(f.block_end_anchor).unwrap();
        let before = &original[..start];
        let after = &original[end..];
        assert!(updated.starts_with(before), "{}: prefix changed", f.name);
        assert!(updated.ends_with(after), "{}: suffix changed", f.name);
        let middle = &updated[before.len()..updated.len() - after.len()];
        let want_prefix = format!("{}{} ", f.indent, f.prefix);
        assert!(
            middle.starts_with(&want_prefix),
            "{}: middle {:?} missing prefix {:?}",
            f.name,
            middle,
            want_prefix
        );
        assert_eq!(middle.matches('\n').count(), 1, "{}: middle {:?} not one line", f.name, middle);
        assert!(
            middle.trim_end().len() > want_prefix.len(),
            "{}: summary line empty",
            f.name
        );
    }

    // Idempotent: the reduced block is short, so a second pass makes no further change.
    let before: Vec<String> = FIXTURES.iter().map(|f| read(dir.path(), f.name)).collect();
    Command::cargo_bin("commentreducr")
        .unwrap()
        .arg(dir.path())
        .arg("--no-llm")
        .assert()
        .success();
    for (f, before) in FIXTURES.iter().zip(before) {
        assert_eq!(read(dir.path(), f.name), before, "{}: not idempotent", f.name);
    }
}
