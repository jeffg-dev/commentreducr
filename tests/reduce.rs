use rusqlite::Connection;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

mod common;
use common::Mock;

static REPO_COUNTER: AtomicU64 = AtomicU64::new(0);

struct Repo(PathBuf);
impl Repo {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "commentreducr-reduce-{}-{}",
            std::process::id(),
            REPO_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        let repo = Self(dir.canonicalize().unwrap());
        repo.git(&["init", "-q"]);
        repo
    }
    fn git(&self, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(&self.0)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    fn write(&self, name: &str, source: &str) {
        std::fs::write(self.0.join(name), source).unwrap();
        self.git(&["add", name]);
    }
    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.0.join(name)).unwrap()
    }
    fn command(&self, endpoint: &str) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_commentreducr"));
        c.args([
            "reduce",
            "--scope",
            "all",
            "--workers",
            "1",
            "--endpoint",
            endpoint,
            "--model",
            "mock",
        ])
        .arg("--config")
        .arg(self.0.join("missing.toml"))
        .env(
            "COMMENTREDUCR_MODEL_PATH",
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("models/python-hook-minilm-l12-v1/model.onnx"),
        )
        .current_dir(&self.0);
        c
    }
    /// The old prompt path, which these tests were written for.
    fn legacy(&self, endpoint: &str) -> Command {
        let mut c = self.command(endpoint);
        c.args(["--judge", "legacy"]);
        c
    }
    fn db(&self) -> Connection {
        Connection::open(self.0.join(".git/commentreducr/state.sqlite")).unwrap()
    }
}
impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(mut command: Command, success: bool) -> String {
    let output = command.output().unwrap();
    assert_eq!(
        output.status.success(),
        success,
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

const CUES: &str = "class CueShifter:\n    \"\"\"A class for shifting subtitle timings.\n\n    It stores an offset and a floor in the constructor. The shift method adds the offset to the start and end times, then clamps them.\n    \"\"\"\n    def __init__(self, offset_ms, floor_ms=0):\n        self.offset_ms = offset_ms\n        self.floor_ms = floor_ms\n\n    def shift(self, start_ms, end_ms):\n        start = max(start_ms + self.offset_ms, self.floor_ms)\n        end = max(end_ms + self.offset_ms, start)\n        return start, end\n";

#[test]
fn classifier_screens_short_items_and_only_flags_reach_the_llm_with_config_and_cache() {
    let repo = Repo::new();
    repo.write("cues.py", CUES);
    repo.write("short.py", "# Assign the count.\ncount = 1\n# noqa\n");
    repo.write("short.js","const value = 1; // Set the value.\nfunction f() { return/* inline narration */undefined; }\n");
    repo.write("protected.py","@tool\ndef agent_action():\n    \"\"\"Necessary instructions to invoke the tool.\"\"\"\n    return 1\n");
    let mock = Mock::new(|r| {
        let final_text = r["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap();
        let reply = if final_text.contains("A class for shifting subtitle") {
            "KEEP\nShift subtitle boundaries by an offset, clipping to a minimum."
        } else if final_text.starts_with("Comment:\ninline narration") {
            "K1 intentional boundary"
        } else {
            "DELETE"
        };
        (200, reply.into())
    });
    let config = repo.0.join("config.toml");
    std::fs::write(&config,format!("judge = \"legacy\"\nscope = \"all\"\nworkers = 1\nendpoint = \"{}\"\nmodel = \"mock\"\ndocstrings_model = \"mock\"\nmin_lines = 1000\nmin_density = 1000\n",mock.endpoint)).unwrap();
    // A separate invocation exercises config-file scope, endpoint and workers.
    let mut command = Command::new(env!("CARGO_BIN_EXE_commentreducr"));
    command
        .arg("reduce")
        .arg("--config")
        .arg(&config)
        .current_dir(&repo.0)
        .env(
            "COMMENTREDUCR_MODEL_PATH",
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("models/python-hook-minilm-l12-v1/model.onnx"),
        );
    let summary = run(command, true);
    assert!(summary.contains("screened"), "{summary}");
    assert!(repo.read("cues.py").contains("Shift subtitle boundaries"));
    assert!(!repo.read("cues.py").contains("It stores an offset"));
    assert_eq!(
        repo.read("short.js"),
        "const value = 1;\nfunction f() { return/* intentional boundary */undefined; }\n"
    );
    assert!(repo.read("protected.py").contains("Necessary instructions"));
    let db = repo.db();
    let short: i64=db.query_row("SELECT count(*) FROM items WHERE file LIKE '%short.py' AND start_line=1 AND end_line=1 AND classification IN ('PASS','FLAG')",[],|r|r.get(0)).unwrap();
    assert_eq!(short, 1);
    let calls = mock.actuals();
    let flagged: i64 = db
        .query_row(
            "SELECT count(*) FROM items WHERE classification IN ('FLAG','DIRECT')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(calls.len(), flagged as usize);
    drop(db);
    // Invalidate the file checkpoint with an unrelated EOF change: classifier inputs stay equal.
    repo.write("short.py", "# Assign the count.\ncount = 1\n# noqa\n\n");
    let mut command = Command::new(env!("CARGO_BIN_EXE_commentreducr"));
    command
        .arg("reduce")
        .arg("--config")
        .arg(&config)
        .current_dir(&repo.0)
        .env(
            "COMMENTREDUCR_MODEL_PATH",
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("models/python-hook-minilm-l12-v1/model.onnx"),
        );
    let summary = run(command, true);
    assert!(summary.contains("1 cached"), "{summary}");
}

#[test]
fn oversized_python_blocks_go_directly_to_the_llm_retry_and_resume() {
    let repo = Repo::new();
    let comment = format!(
        "{}COMMENT_END",
        "This repeats the implementation. ".repeat(100)
    );
    let docstring = format!(
        "{}DOCSTRING_END",
        "This repeats the implementation. ".repeat(100)
    );
    let source = format!("# {comment}\n\"\"\"{docstring}\"\"\"\nvalue = 1\n");
    repo.write("large.py", &source);
    let mut failures = 0;
    let mock = Mock::new(move |r| {
        let text = r["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap();
        if text.contains("DOCSTRING_END") && failures < 2 {
            failures += 1;
            (500, "try later".into())
        } else {
            (200, "DELETE".into())
        }
    });
    run(repo.legacy(&mock.endpoint), false);
    assert_eq!(repo.read("large.py"), source);
    let summary = run(repo.legacy(&mock.endpoint), true);
    assert!(summary.contains("0 errors"), "{summary}");
    assert!(summary.contains("1 verdicts cached"), "{summary}");
    assert_eq!(repo.read("large.py"), "value = 1\n");
    let calls = mock.actuals();
    assert_eq!(calls.len(), 4);
    assert_eq!(
        calls.iter().filter(|s| s.contains("COMMENT_END")).count(),
        1
    );
    assert!(calls.iter().any(|s| s.contains(&comment)));
    assert!(calls.iter().any(|s| s.contains(&docstring)));
    let db = repo.db();
    let direct: i64 = db
        .query_row(
            "SELECT count(*) FROM items WHERE classification='DIRECT' AND state='applied'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(direct, 2);
    let cached: i64 = db
        .query_row("SELECT count(*) FROM classifications", [], |r| r.get(0))
        .unwrap();
    assert_eq!(cached, 0);
    drop(db);
    let summary = run(repo.legacy(&mock.endpoint), true);
    assert!(summary.contains("1 files completed"), "{summary}");
    assert_eq!(mock.actuals().len(), 4);
}

#[test]
fn killed_run_resumes_ready_verdicts_and_does_not_apply_stale_offsets() {
    let repo = Repo::new();
    let source = "// FIRST_ITEM\nconst a = 1;\n\n// SECOND_ITEM\nconst b = 2;\n";
    repo.write("items.js", source);
    let (blocked_tx, blocked_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let mut blocked = false;
    let mock = Mock::new(move |r| {
        let text = r["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap();
        if text.contains("SECOND_ITEM") && !blocked {
            blocked = true;
            blocked_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(20));
        }
        (200, "DELETE".into())
    });
    let mut child = repo
        .legacy(&mock.endpoint)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    blocked_rx.recv_timeout(Duration::from_secs(20)).unwrap();
    assert_eq!(repo.read("items.js"), source);
    let db = repo.db();
    let ready: i64 = db
        .query_row("SELECT count(*) FROM items WHERE state='ready'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(ready, 1);
    drop(db);
    child.kill().unwrap();
    child.wait().unwrap();
    release_tx.send(()).unwrap();
    let summary = run(repo.legacy(&mock.endpoint), true);
    assert!(summary.contains("1 verdicts cached"), "{summary}");
    assert_eq!(repo.read("items.js"), "const a = 1;\n\nconst b = 2;\n");
    assert_eq!(
        mock.actuals()
            .iter()
            .filter(|s| s.contains("FIRST_ITEM"))
            .count(),
        1
    );
    let count = mock.actuals().len();
    run(repo.legacy(&mock.endpoint), true);
    assert_eq!(mock.actuals().len(), count);
    // Human edits invalidate the completed-file snapshot; only the new item is examined.
    repo.write("items.js", "const a = 99;\n// NEW_ITEM\nconst b = 2;\n");
    run(repo.legacy(&mock.endpoint), true);
    assert_eq!(repo.read("items.js"), "const a = 99;\nconst b = 2;\n");
    assert_eq!(mock.actuals().len(), count + 1);
}

#[test]
fn failure_keeps_the_whole_file_and_retries_only_unfinished_items() {
    let repo = Repo::new();
    let source = "// READY_ITEM\nconst a = 1;\n\n// RETRY_ITEM\nconst b = 2;\n";
    repo.write("items.js", source);
    let mut failures = 0;
    let mock = Mock::new(move |r| {
        let text = r["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap();
        if text.contains("RETRY_ITEM") && failures < 2 {
            failures += 1;
            (500, "try later".into())
        } else {
            (200, "DELETE".into())
        }
    });
    run(repo.legacy(&mock.endpoint), false);
    assert_eq!(repo.read("items.js"), source);
    run(repo.legacy(&mock.endpoint), true);
    assert_eq!(
        mock.actuals()
            .iter()
            .filter(|s| s.contains("READY_ITEM"))
            .count(),
        1
    );
    assert!(!repo.read("items.js").contains("RETRY_ITEM"));
    assert!(mock.paths().iter().all(|p| p == "/v1/chat/completions"));
}

#[test]
fn systemone_judge_deletes_reviews_near_thresholds_and_caches_verdicts() {
    let repo = Repo::new();
    repo.write("gone.js", "// DELETE_ME set a\nconst a = 1;\n");
    let near = "// NEAR_THRESHOLD set b\nconst b = 2;\n";
    repo.write("near.js", near);
    let mock = Mock::new(|r| {
        let state = r["state"].as_str().unwrap_or_default();
        // E[c2] is 0 (a clear delete) or 0.7, within the margin of c2_min 0.75 (review).
        let c2 = if state.contains("NEAR_THRESHOLD") {
            serde_json::json!({"0": 0.3, "1": 0.7})
        } else {
            serde_json::json!({"0": 1.0})
        };
        let answers = serde_json::json!({"answers": {
            "g1_contradicts": {"type": "noul", "noul": 0.0},
            "c2_info_gain": {"type": "score", "probabilities": c2},
            "verdict": {"type": "choice", "probabilities": {"delete": 1.0}},
        }, "usage": {"input_tokens": 1000}});
        (200, answers.to_string())
    });
    let mut command = repo.command(&mock.endpoint);
    command.args(["--judge", "systemone"]);
    run(command, true);
    assert_eq!(repo.read("gone.js"), "const a = 1;\n");
    assert_eq!(repo.read("near.js"), near);
    let paths = mock.paths();
    // Chat calls are only the rewrite generator's preflight; nothing was rewritten.
    assert!(paths.iter().any(|p| p == "/v1/systemone"), "{paths:?}");
    assert!(mock.actuals().is_empty(), "{paths:?}");
    let db = repo.db();
    let (verdict, answers): (String, String) = db
        .query_row(
            "SELECT s.verdict, s.answers FROM states s JOIN items i ON i.id = s.item_id \
             WHERE i.file LIKE '%near.js' AND i.result_type = 'review'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(verdict, "review");
    assert!(answers.contains("c2_info_gain"), "{answers}");
    drop(db);
    let count = paths.len();
    let mut command = repo.command(&mock.endpoint);
    command.args(["--judge", "systemone"]);
    run(command, true);
    assert_eq!(mock.paths().len(), count);
}

/// Strict answers that make an inline comment `rewrite_shorter`; verify answers from `verify`.
fn rewrite_mock(preserves: f64) -> Mock {
    Mock::new(move |r| {
        if r.get("messages").is_some() {
            return (200, "hold the lock".into());
        }
        let answers = if r["questions"].get("preserves").is_some() {
            serde_json::json!({
                "preserves": {"type": "noul", "noul": preserves},
                "adds": {"type": "noul", "noul": 0.02},
                "g1_candidate": {"type": "noul", "noul": 0.01},
            })
        } else {
            serde_json::json!({
                "g1_contradicts": {"type": "noul", "noul": 0.0},
                "c2_info_gain": {"type": "score", "probabilities": {"3": 1.0}},
                "c9_replaceable": {"type": "choice", "probabilities": {"no": 1.0}},
                "hazard": {"type": "noul", "noul": 1.0},
                "leak": {"type": "noul", "noul": 1.0},
                "verdict": {"type": "choice", "probabilities": {"rewrite_shorter": 1.0}},
            })
        };
        (200, serde_json::json!({"answers": answers}).to_string())
    })
}

/// Writes `judge_thresholds` overrides into the config file the commands read.
fn set_thresholds(repo: &Repo, json: &str) {
    std::fs::write(repo.0.join("th.json"), json).unwrap();
    let th = repo.0.join("th.json");
    std::fs::write(
        repo.0.join("missing.toml"),
        format!("judge_thresholds = {:?}\n", th.to_str().unwrap()),
    )
    .unwrap();
}

fn enable_rewrite(repo: &Repo) {
    set_thresholds(repo, r#"{"rewrite": true}"#);
}

#[test]
fn rewrite_off_keeps_the_comment() {
    let repo = Repo::new();
    repo.write("lock.js", LOCKED);
    set_thresholds(&repo, r#"{"rewrite": false}"#);
    let mock = rewrite_mock(0.97);
    let mut command = repo.command(&mock.endpoint);
    command.args(["--judge", "systemone"]);
    run(command, true);
    assert_eq!(repo.read("lock.js"), LOCKED);
    assert!(mock.actuals().is_empty());
    let result: String = repo
        .db()
        .query_row("SELECT result_type FROM items", [], |r| r.get(0))
        .unwrap();
    assert_eq!(result, "keep");
}

#[test]
fn ai_mode_reports_rewrites_as_jsonl_and_leaves_them_to_the_agent() {
    let repo = Repo::new();
    repo.write("lock.js", LOCKED);
    let mock = rewrite_mock(0.97);
    let mut command = repo.command(&mock.endpoint);
    command.args(["--judge", "systemone", "--ai"]);
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(repo.read("lock.js"), LOCKED);
    assert!(mock.actuals().is_empty(), "the generator was called");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 1, "{stdout}");
    assert_eq!(lines[0]["path"], "lock.js");
    assert_eq!(lines[0]["start_line"], 2);
    assert_eq!(lines[0]["end_line"], 3);
    assert_eq!(lines[0]["recommendation"], "Reduce via rewrite");
    assert!(
        lines[0]["comment"]
            .as_str()
            .unwrap()
            .contains("K0 hold the lock")
    );
    // A rerun repeats the advice from cached verdicts.
    let calls = mock.paths().len();
    let mut command = repo.command(&mock.endpoint);
    command.args(["--judge", "systemone", "--ai"]);
    let again = command.output().unwrap();
    assert_eq!(String::from_utf8(again.stdout).unwrap(), stdout);
    assert_eq!(mock.paths().len(), calls);
}

const LOCKED: &str = "function set(k, v) {\n  // K0 hold the lock before writing the cache\n  // because the refresher thread writes it too\n  lock(() => cache.set(k, v));\n}\n";

#[test]
fn systemone_verified_rewrite_replaces_comment() {
    let repo = Repo::new();
    repo.write("lock.js", LOCKED);
    enable_rewrite(&repo);
    let mock = rewrite_mock(0.97);
    let mut command = repo.command(&mock.endpoint);
    command.args(["--judge", "systemone"]);
    run(command, true);
    assert_eq!(
        repo.read("lock.js"),
        "function set(k, v) {\n  // hold the lock\n  lock(() => cache.set(k, v));\n}\n"
    );
    let answers: String = repo
        .db()
        .query_row(
            "SELECT answers FROM states WHERE verdict = 'rewrite_shorter'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(answers.contains("\"verify\""), "{answers}");
    // The generator gets the rendered state and the verdict, on generator_model.
    let calls = mock.calls.lock().unwrap();
    let chat = calls
        .iter()
        .map(|(_, r)| r)
        .find(|r| r["messages"].as_array().is_some_and(|m| m.len() > 1))
        .unwrap();
    assert_eq!(chat["model"], "gemma-4-26b-a4b-it-4bit");
    let user = chat["messages"][1]["content"].as_str().unwrap();
    assert!(user.contains("K0 hold the lock"), "{user}");
    assert!(user.ends_with("\nverdict: rewrite_shorter"), "{user}");
}

#[test]
fn systemone_rejected_rewrite_keeps_file() {
    let repo = Repo::new();
    repo.write("lock.js", LOCKED);
    enable_rewrite(&repo);
    let mock = rewrite_mock(0.5);
    let mut command = repo.command(&mock.endpoint);
    command.args(["--judge", "systemone"]);
    let stderr = run(command, true);
    assert!(
        stderr.contains("rewrite rejected (preserves 0.50)"),
        "{stderr}"
    );
    assert_eq!(repo.read("lock.js"), LOCKED);
    let result: String = repo
        .db()
        .query_row("SELECT result_type FROM items", [], |r| r.get(0))
        .unwrap();
    assert_eq!(result, "keep");
}

#[test]
fn escalation_model_resolves_a_review_item() {
    let repo = Repo::new();
    repo.write("near.js", "// NEAR_THRESHOLD set b\nconst b = 2;\n");
    std::fs::write(repo.0.join("missing.toml"), "escalation_model = \"big\"\n").unwrap();
    let mock = Mock::new(|r| {
        // The default model is near the c2 threshold; the escalation model is decisive.
        let c2 = if r["model"] == "big" {
            serde_json::json!({"0": 1.0})
        } else {
            serde_json::json!({"0": 0.3, "1": 0.7})
        };
        let answers = serde_json::json!({"answers": {
            "g1_contradicts": {"type": "noul", "noul": 0.0},
            "c2_info_gain": {"type": "score", "probabilities": c2},
            "verdict": {"type": "choice", "probabilities": {"delete": 1.0}},
        }});
        (200, answers.to_string())
    });
    let mut command = repo.command(&mock.endpoint);
    command.args(["--judge", "systemone"]);
    run(command, true);
    assert_eq!(repo.read("near.js"), "const b = 2;\n");
    let models: Vec<_> = mock
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, r)| r["questions"].get("c2_info_gain").is_some())
        .map(|(_, r)| r["model"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(models.len(), 2, "{models:?}");
    assert!(models.contains(&"big".to_owned()), "{models:?}");
    let answers: String = repo
        .db()
        .query_row("SELECT answers FROM states", [], |r| r.get(0))
        .unwrap();
    assert!(answers.contains("\"escalation\""), "{answers}");
}

#[test]
fn eval_v2_writes_run_files_with_rewrite_scores() {
    let repo = Repo::new();
    let row = serde_json::json!({
        "id": "syn-0001", "source": {"repo": "synthetic"}, "language": "javascript",
        "comment_kind": "inline", "placement": "before_statement",
        "comment": "// K0 hold the lock before writing the cache\n// because the refresher thread writes it too",
        "annotated_code": "lock(() => cache.set(k, v));",
        "enclosing_scope": "function set(k, v) {\n  lock(() => cache.set(k, v));\n}",
        "symbol": {"name": "set", "public": true, "is_test": false},
        "change": {"authored_by_agent": null, "agent": null, "comment_age_days": null, "code_newer_than_comment": null},
        "labels": {"verdict": "rewrite_shorter"},
    });
    let mut doc = row.clone();
    doc["id"] = "syn-0002".into();
    doc["language"] = "python".into();
    doc["comment"] =
        "\"\"\"Hold the lock before writing: the refresher writes it too.\"\"\"".into();
    std::fs::write(repo.0.join("gold.jsonl"), format!("{row}\n{doc}\n")).unwrap();
    enable_rewrite(&repo);
    let mock = rewrite_mock(0.97);
    let mut command = repo.command(&mock.endpoint);
    command.args([
        "--judge",
        "systemone",
        "--eval",
        "gold.jsonl",
        "--eval-runs",
        "runs",
    ]);
    run(command, true);
    let run_file: Value =
        serde_json::from_str(&std::fs::read_to_string(repo.0.join("runs/syn-0001.json")).unwrap())
            .unwrap();
    assert_eq!(run_file["verdict"], "rewrite_shorter");
    assert_eq!(run_file["rewrite"], "// hold the lock");
    assert_eq!(run_file["rewrite_scores"]["preserves"], 0.97);
    assert_eq!(run_file["rewrite_scores"]["adds"], 0.02);
    let doc_run: Value =
        serde_json::from_str(&std::fs::read_to_string(repo.0.join("runs/syn-0002.json")).unwrap())
            .unwrap();
    assert_eq!(doc_run["rewrite"], "\"\"\"hold the lock\"\"\"");
}
