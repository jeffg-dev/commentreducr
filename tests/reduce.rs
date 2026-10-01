#![cfg(feature = "hook")]
use rusqlite::Connection;
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

struct Repo(PathBuf);
impl Repo {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "commentreducr-reduce-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
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
    fn db(&self) -> Connection {
        Connection::open(self.0.join(".git/commentreducr/state.sqlite")).unwrap()
    }
}
impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn request(stream: &TcpStream) -> Value {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut length = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some(n) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = n.trim().parse().unwrap();
        }
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}
fn respond(stream: &mut TcpStream, status: u16, text: &str) {
    let body=serde_json::json!({"choices":[{"message":{"content":text}}],"usage":{"prompt_tokens":10,"completion_tokens":2}}).to_string();
    let response = format!(
        "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}
struct Mock {
    endpoint: String,
    calls: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
}
impl Mock {
    fn new(mut handler: impl FnMut(&Value) -> (u16, String) + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_calls = calls.clone();
        let worker_stop = stop.clone();
        std::thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(10)))
                            .unwrap();
                        let req = request(&stream);
                        worker_calls.lock().unwrap().push(req.clone());
                        let (status, text) = handler(&req);
                        respond(&mut stream, status, &text);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            endpoint,
            calls,
            stop,
        }
    }
    fn actuals(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["messages"].as_array().unwrap().len() > 1)
            .map(|r| {
                r["messages"].as_array().unwrap().last().unwrap()["content"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    }
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
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
    std::fs::write(&config,format!("scope = \"all\"\nworkers = 1\nendpoint = \"{}\"\nmodel = \"mock\"\ndocstrings_model = \"mock\"\nmin_lines = 1000\nmin_density = 1000\n",mock.endpoint)).unwrap();
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
        .command(&mock.endpoint)
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
    let summary = run(repo.command(&mock.endpoint), true);
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
    run(repo.command(&mock.endpoint), true);
    assert_eq!(mock.actuals().len(), count);
    // Human edits invalidate the completed-file snapshot; only the new item is examined.
    repo.write("items.js", "const a = 99;\n// NEW_ITEM\nconst b = 2;\n");
    run(repo.command(&mock.endpoint), true);
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
    run(repo.command(&mock.endpoint), false);
    assert_eq!(repo.read("items.js"), source);
    run(repo.command(&mock.endpoint), true);
    assert_eq!(
        mock.actuals()
            .iter()
            .filter(|s| s.contains("READY_ITEM"))
            .count(),
        1
    );
    assert!(!repo.read("items.js").contains("RETRY_ITEM"));
}
