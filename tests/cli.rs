//! End-to-end CLI tests over tests/fixtures without a live LLM.
//!
//! `comments` subcommand: --delete (correct and idempotent), --delete --dry-run (counts, writes
//! nothing), and --reduce against a dead endpoint (hard failure, writes nothing).
//!
//! `docstrings` subcommand: same shape, against tests/fixtures/test_sample.py (plus sample.py,
//! which is a Python file but not part of the docstrings test's own fixture set).
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

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

/// A temp repo with exactly tests/fixtures/test_sample.py, sample.py and sample.ts, for the
/// reduce-mode end-to-end test below (kept small so the mock LLM's expected request count is
/// exact and easy to reason about).
fn setup_reduce_repo() -> TempDir {
    let dir = TempDir::new();
    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for name in ["test_sample.py", "sample.py", "sample.ts"] {
        std::fs::copy(fixtures_dir.join(name), dir.path().join(name)).unwrap();
    }
    commit_all(dir.path());
    dir
}

/// Reads one HTTP request off `stream`: headers up to the blank line, then exactly
/// Content-Length bytes of body. Good enough for the loopback, synchronous requests our own
/// `minreq`-based client makes; not a general HTTP parser.
fn read_http_body(stream: &TcpStream) -> String {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).unwrap();
    String::from_utf8_lossy(&body).into_owned()
}

/// A tiny in-process mock of an OpenAI-compatible `/v1/chat/completions` endpoint. Accepts up to
/// `expected_requests` connections (then the accept loop -- and with it the listener -- ends, so
/// nothing can hang the test), and for each one answers with the reply from the first `rules`
/// entry whose marker is a substring of the request body, or `default_reply` otherwise. Returns
/// the endpoint base URL.
fn mock_llm(
    rules: Vec<(&'static str, String)>,
    default_reply: String,
    expected_requests: u32,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for _ in 0..expected_requests {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
            stream.set_write_timeout(Some(Duration::from_secs(10))).ok();
            let body = read_http_body(&stream);
            let reply = rules
                .iter()
                .find(|(marker, _)| body.contains(marker))
                .map_or_else(|| default_reply.clone(), |(_, r)| r.clone());
            let payload = serde_json::json!({
                "choices": [{"message": {"content": reply}}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 2},
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len(),
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://127.0.0.1:{port}/v1")
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

#[test]
fn delete_mode_removes_non_structural_comments_and_is_idempotent() {
    let dir = setup_repo();

    run(commentreducr()
        .arg("comments")
        .arg(dir.path())
        .arg("--delete"))
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
    run(commentreducr()
        .arg("comments")
        .arg(dir.path())
        .arg("--delete"))
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

    run(commentreducr()
        .arg("comments")
        .arg(dir.path())
        .arg("--delete")
        .arg("--dry-run"))
    .success()
    .stdout_contains(" deleted (")
    .stderr_contains("5 files scanned, 5 changed, 0 skipped");

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
        .arg("comments")
        .arg(dir.path())
        .arg("--delete"))
    .code(1)
    .stderr_contains("warning: skipping")
    .stderr_contains("6 files scanned, 5 changed, 1 skipped");

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
        .arg("comments")
        .arg(dir.path())
        .arg("--dry-run"))
    .failure()
    .stderr_contains("--delete");
}

#[test]
fn dry_run_with_explicit_reduce_is_also_rejected() {
    // clap's `requires = "delete"` on --dry-run does not fire against an explicit --reduce (only
    // against --reduce's absence), so this combination must be checked by hand -- otherwise it
    // would run the full reduce pipeline, including live LLM calls, before being silently
    // no-op'd at the final write.
    let dir = setup_repo();
    let before = snapshot(dir.path());

    run(commentreducr()
        .arg("comments")
        .arg(dir.path())
        .arg("--reduce")
        .arg("--dry-run")
        .arg("--endpoint")
        .arg("http://127.0.0.1:1/v1"))
    .failure()
    .stderr_contains("--dry-run only applies to --delete");

    assert_eq!(snapshot(dir.path()), before, "rejected run modified files");
}

#[test]
fn reduce_fails_hard_when_llm_unreachable() {
    let dir = setup_repo();
    let before = snapshot(dir.path());

    run(commentreducr()
        .arg("comments")
        .arg(dir.path())
        .arg("--reduce")
        .arg("--config")
        .arg(dir.path().join("no-such-config.toml"))
        .arg("--endpoint")
        .arg("http://127.0.0.1:1/v1"))
    .failure()
    .stderr_contains("cannot reach LLM at http://127.0.0.1:1/v1");

    assert_eq!(snapshot(dir.path()), before, "failed reduce modified files");
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
        .arg("docstrings")
        .arg(dir.path())
        .arg("--delete"))
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

    // Idempotent: running docstrings --delete again changes nothing.
    let before = (
        read(dir.path(), "test_sample.py"),
        read(dir.path(), "sample.py"),
    );
    run(commentreducr()
        .arg("docstrings")
        .arg(dir.path())
        .arg("--delete"))
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
        .arg("docstrings")
        .arg(dir.path())
        .arg("--delete")
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
fn docstrings_reduce_fails_hard_when_llm_unreachable() {
    let dir = setup_docstrings_repo();
    let before = read(dir.path(), "test_sample.py");

    run(commentreducr()
        .arg("docstrings")
        .arg(dir.path())
        .arg("--reduce")
        .arg("--config")
        .arg(dir.path().join("no-such-config.toml"))
        .arg("--endpoint")
        .arg("http://127.0.0.1:1/v1"))
    .failure()
    .stderr_contains("cannot reach LLM at http://127.0.0.1:1/v1");

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

    run(commentreducr()
        .arg("comments")
        .arg(dir.path())
        .arg("--delete"))
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

/// End-to-end reduce mode, against an in-process mock LLM instead of a live one: `docstrings
/// --reduce` over a small repo (module docstring KEEP+3 lines, a test function KEEP+1 line, a
/// helper DELETE, and a KEEP containing `"""` that must be refused as unsafe), then `comments
/// --reduce` over the same repo (a K-class keep and a D-class delete).
#[test]
fn reduce_mode_end_to_end_with_a_mock_llm() {
    let dir = setup_reduce_repo();
    let no_config = dir.path().join("no-such-config.toml");

    let endpoint = mock_llm(
        vec![
            (
                "Q3 test-infra cleanup",
                "KEEP\nShared pytest helpers for this package.\nKeep new tests colocated here.\nSee wiki for conventions.".to_string(),
            ),
            ("Normalizes a list of items", "DELETE".to_string()),
            (
                "Guards the empty-list case",
                "KEEP\nGuards the empty case, nothing else.".to_string(),
            ),
            (
                "mixed-case, whitespace-padded items",
                "KEEP\nSays \"\"\" here by mistake.".to_string(),
            ),
        ],
        "DELETE".to_string(),
        8, // preflight + {module, helper_thing, test_short, test_dedup, TestThing, probe} + sample.py's module
    );

    run(commentreducr()
        .arg("docstrings")
        .arg(dir.path())
        .arg("--reduce")
        .arg("--min-lines")
        .arg("1")
        .arg("-n")
        .arg("1")
        .arg("--endpoint")
        .arg(&endpoint)
        .arg("--config")
        .arg(&no_config))
    .code(1)
    .stderr_contains("2 files scanned, 2 changed, 0 skipped")
    .stderr_contains("docstrings: 2 kept, 4 deleted (")
    .stderr_contains("lines), 2 reduced (")
    .stderr_contains("lines saved), 1 LLM failures");

    let test_sample = read(dir.path(), "test_sample.py");
    // Module docstring: own_line, indent "" -- KEEP text spliced in with PEP 257 closing quotes
    // on their own line.
    assert!(
        test_sample.contains(
            "\"\"\"Shared pytest helpers for this package.\nKeep new tests colocated here.\nSee wiki for conventions.\n\"\"\""
        ),
        "module docstring not rewritten as expected:\n{test_sample}"
    );
    // helper_thing: DELETE removes the whole docstring.
    assert!(
        !test_sample.contains("Normalizes a list of items"),
        "helper_thing docstring not deleted:\n{test_sample}"
    );
    // test_short: single-line KEEP at its original 4-space indent.
    assert!(
        test_sample.contains("    \"\"\"Guards the empty case, nothing else.\"\"\""),
        "test_short docstring not rewritten as expected:\n{test_sample}"
    );
    // test_dedup_and_lowercase: the KEEP reply contains the quote delimiter, so replace_edit
    // refuses it as unsafe -- the docstring is left byte-for-byte unchanged.
    assert!(
        test_sample.contains("Ensures mixed-case, whitespace-padded items are normalized"),
        "test_dedup docstring should survive a refused reply unchanged:\n{test_sample}"
    );
    // TestThing and Widget.probe: DELETE on an only_statement docstring becomes `pass`.
    assert!(
        test_sample.contains("class TestThing:\n    pass\n"),
        "TestThing docstring not deleted:\n{test_sample}"
    );
    assert!(
        test_sample.contains("def probe(self): pass"),
        "Widget.probe docstring not deleted:\n{test_sample}"
    );
    // The rewritten file must still parse cleanly.
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(test_sample.as_bytes(), None).unwrap();
    assert!(
        !tree.root_node().has_error(),
        "rewritten test_sample.py has parse errors:\n{test_sample}"
    );

    let sample_py = read(dir.path(), "sample.py");
    assert!(
        !sample_py.contains("Module docstring: a small stats helper"),
        "sample.py's module docstring should have been deleted:\n{sample_py}"
    );

    // --- comments --reduce, same repo ---
    let endpoint = mock_llm(
        vec![
            ("pulling in numpy", "K1 keep this trap".to_string()),
            ("pulling in a math library", "D1".to_string()),
        ],
        "DELETE".to_string(),
        3, // preflight + sample.py's block + sample.ts's block
    );

    run(commentreducr()
        .arg("comments")
        .arg(dir.path())
        .arg("--reduce")
        .arg("--min-lines")
        .arg("1")
        .arg("-n")
        .arg("1")
        .arg("--endpoint")
        .arg(&endpoint)
        .arg("--config")
        .arg(&no_config))
    .success()
    .stderr_contains("tokens: 3 requests,");

    let sample_py = read(dir.path(), "sample.py");
    assert!(
        sample_py.contains("    # keep this trap\n"),
        "sample.py's big block not reduced to the K1 reply:\n{sample_py}"
    );
    let sample_ts = read(dir.path(), "sample.ts");
    assert!(
        !sample_ts.contains("walks the list of samples"),
        "sample.ts's big block not deleted:\n{sample_ts}"
    );
}
