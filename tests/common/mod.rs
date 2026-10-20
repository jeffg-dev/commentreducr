//! Mock OpenAI-compatible and System One server shared by the integration tests.
#![allow(dead_code)]
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The request path and JSON body.
fn request(stream: &TcpStream) -> (String, Value) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut length = 0;
    let mut path = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if path.is_empty() {
            path = line.split(' ').nth(1).unwrap_or_default().to_owned();
        }
        if let Some(n) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = n.trim().parse().unwrap();
        }
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).unwrap();
    (path, serde_json::from_slice(&bytes).unwrap())
}
/// `/systemone` replies are the handler's JSON as-is; chat replies wrap the text.
fn respond(stream: &mut TcpStream, path: &str, status: u16, text: &str) {
    let body = if path.ends_with("/systemone") {
        text.to_owned()
    } else {
        serde_json::json!({"choices":[{"message":{"content":text}}],"usage":{"prompt_tokens":10,"completion_tokens":2}}).to_string()
    };
    let response = format!(
        "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}
pub struct Mock {
    pub endpoint: String,
    pub calls: Arc<Mutex<Vec<(String, Value)>>>,
    stop: Arc<AtomicBool>,
}
impl Mock {
    pub fn new(mut handler: impl FnMut(&Value) -> (u16, String) + Send + 'static) -> Self {
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
                        // macOS inherits the listener's nonblocking mode.
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(Duration::from_secs(10)))
                            .unwrap();
                        let (path, req) = request(&stream);
                        worker_calls
                            .lock()
                            .unwrap()
                            .push((path.clone(), req.clone()));
                        let (status, text) = handler(&req);
                        respond(&mut stream, &path, status, &text);
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
    pub fn actuals(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(_, r)| r)
            .filter(|r| r["messages"].as_array().is_some_and(|m| m.len() > 1))
            .map(|r| {
                r["messages"].as_array().unwrap().last().unwrap()["content"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    }
}
impl Mock {
    pub fn paths(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(p, _)| p.clone())
            .collect()
    }
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
