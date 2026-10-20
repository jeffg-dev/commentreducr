//! Judge backends: a state and some questions in, answer probabilities out.
use crate::context::ItemContext;
use crate::llm::{LlmClient, TokenTotals};
use crate::policy::Thresholds;
use crate::render::{Json, Questions, estimate_tokens};
use crate::types::Config;
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// question id -> option -> probability
pub type Answers = BTreeMap<String, BTreeMap<String, f64>>;

pub struct Judgement {
    pub answers: Answers,
    /// argmax of the `verdict` question, if it was asked
    pub holistic: Option<String>,
    pub input_tokens: Option<u64>,
    pub latency_ms: u64,
}

pub trait Judge: Sync + Send {
    fn judge(&self, state: &str, questions: &Questions) -> Result<Judgement>;
    /// True when answers are one-hot and the policy must be bypassed.
    fn one_hot(&self) -> bool {
        false
    }
    /// For the ledger and progress, e.g. `systemone:clef-flash-4bit`.
    fn name(&self) -> String;
    /// Preflight: prove the backend is reachable.
    fn check(&self) -> Result<()>;
    fn tokens(&self) -> &TokenTotals;
}

pub struct SystemOne {
    url: String,
    model: String,
    api_key: Option<String>,
    pub tokens: TokenTotals,
    truncation_warned: AtomicBool,
}

#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    state: &'a str,
    questions: Json,
}

#[derive(Deserialize)]
struct Response {
    answers: BTreeMap<String, Answer>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct Answer {
    #[serde(rename = "type")]
    kind: String,
    noul: Option<f64>,
    probabilities: Option<BTreeMap<String, f64>>,
}

/// judges/systemone.py::convert
fn convert(a: Answer) -> Result<BTreeMap<String, f64>> {
    if a.kind == "noul" {
        let p = a
            .noul
            .ok_or_else(|| anyhow!("noul answer without `noul`"))?;
        Ok(BTreeMap::from([
            ("true".into(), p),
            ("false".into(), 1.0 - p),
        ]))
    } else {
        a.probabilities
            .ok_or_else(|| anyhow!("{} answer without `probabilities`", a.kind))
    }
}

impl SystemOne {
    pub fn new(url: &str, model: &str, api_key: Option<&str>) -> Self {
        SystemOne {
            url: url.to_string(),
            model: model.to_string(),
            api_key: api_key.map(str::to_string),
            tokens: TokenTotals::default(),
            truncation_warned: AtomicBool::new(false),
        }
    }

    fn check_impl(&self) -> Result<()> {
        let q: Questions = vec![(
            "ping".into(),
            Json::Object(vec![
                ("type".into(), Json::String("noul".into())),
                (
                    "instructions".into(),
                    Json::String("Is this a greeting?".into()),
                ),
            ]),
        )];
        self.judge("hi", &q).with_context(|| {
            format!(
                "cannot reach judge at {} with model {}",
                self.url, self.model
            )
        })?;
        Ok(())
    }
}

/// POST `body` to `url`, with one retry on a transport error other than a timeout: a server
/// that never answered would only be waited on twice.
fn post(url: &str, api_key: Option<&str>, body: &[u8], secs: u64) -> Result<minreq::Response> {
    let mut last = None;
    for _ in 0..2 {
        let mut req = minreq::post(url)
            .with_timeout(secs)
            .with_header("Content-Type", "application/json; charset=UTF-8")
            .with_body(body);
        if let Some(key) = api_key {
            req = req.with_header("Authorization", format!("Bearer {key}"));
        }
        match req.send() {
            Ok(r) => return Ok(r),
            Err(minreq::Error::IoError(e)) if e.kind() == std::io::ErrorKind::TimedOut => {
                return Err(e).context("judge request timed out");
            }
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap()).context("judge request failed")
}

impl Judge for SystemOne {
    fn judge(&self, state: &str, questions: &Questions) -> Result<Judgement> {
        let body = serde_json::to_vec(&Request {
            model: &self.model,
            state,
            questions: Json::Object(questions.clone()),
        })
        .context("encoding judge request")?;
        let start = Instant::now();
        let resp = post(&self.url, self.api_key.as_deref(), &body, 120)?;
        let latency_ms = start.elapsed().as_millis() as u64;
        if !(200..300).contains(&resp.status_code) {
            bail!(
                "judge returned HTTP {} {}",
                resp.status_code,
                resp.reason_phrase
            );
        }
        let parsed: Response =
            serde_json::from_slice(resp.as_bytes()).context("failed to parse judge response")?;
        let input_tokens = parsed.usage.and_then(|u| u.input_tokens);
        self.tokens.requests.fetch_add(1, Ordering::Relaxed);
        if let Some(n) = input_tokens {
            self.tokens.prompt.fetch_add(n, Ordering::Relaxed);
            if n < estimate_tokens(state) as u64 / 2
                && !self.truncation_warned.swap(true, Ordering::Relaxed)
            {
                eprintln!(
                    "warning: state truncated by the server; raise its max_length or lower MAX_STATE_TOKENS"
                );
            }
        }
        let mut answers = Answers::new();
        for (qid, a) in parsed.answers {
            answers.insert(
                qid.clone(),
                convert(a).with_context(|| format!("question {qid}"))?,
            );
        }
        let holistic = answers.get("verdict").and_then(|v| {
            v.iter()
                .fold(None::<(&String, f64)>, |best, (k, &p)| match best {
                    Some((_, bp)) if bp >= p => best,
                    _ => Some((k, p)),
                })
                .map(|(k, _)| k.clone())
        });
        Ok(Judgement {
            answers,
            holistic,
            input_tokens,
            latency_ms,
        })
    }

    fn name(&self) -> String {
        format!("systemone:{}", self.model)
    }

    /// One request with a one-question schema and the state "hi".
    fn check(&self) -> Result<()> {
        self.check_impl()
    }

    fn tokens(&self) -> &TokenTotals {
        &self.tokens
    }
}

pub struct VerifyOutcome {
    pub accepted: bool,
    /// The first failed check, e.g. `preserves 0.61`; None when accepted.
    pub reason: Option<String>,
    pub answers: Answers,
}

/// Asks the judge whether `candidate` may replace `original`. A missing answer is a rejection.
/// Systemone only: the generative backend cannot answer these questions.
pub(crate) fn verify_rewrite(
    judge: &dyn Judge,
    original: &str,
    candidate: &str,
    ctx: &ItemContext,
    th: &Thresholds,
) -> Result<VerifyOutcome> {
    verify_texts(
        judge,
        original,
        candidate,
        &ctx.annotated_code,
        &ctx.enclosing_scope,
        th,
    )
}

/// `verify_rewrite` for callers that hold the code strings but no `ItemContext` (eval v2).
pub(crate) fn verify_texts(
    judge: &dyn Judge,
    original: &str,
    candidate: &str,
    annotated_code: &str,
    enclosing_scope: &str,
    th: &Thresholds,
) -> Result<VerifyOutcome> {
    let state = crate::render::render_verify(
        original,
        candidate,
        annotated_code,
        enclosing_scope,
        crate::render::MAX_STATE_TOKENS,
    );
    let questions = crate::render::questions_for(crate::render::SCHEMA_VERIFY, None);
    let answers = judge.judge(&state, &questions)?.answers;
    let p = |q: &str| answers.get(q).and_then(|o| o.get("true")).copied();
    let checks = [
        (
            "preserves",
            p("preserves"),
            p("preserves").is_some_and(|x| x > th.verify_preserves),
        ),
        (
            "adds",
            p("adds"),
            p("adds").is_some_and(|x| x < th.verify_adds),
        ),
        (
            "g1_candidate",
            p("g1_candidate"),
            p("g1_candidate").is_some_and(|x| x < th.verify_g1),
        ),
    ];
    let reason = checks.iter().find(|c| !c.2).map(|(q, x, _)| match x {
        Some(x) => format!("{q} {x:.2}"),
        None => format!("{q} missing"),
    });
    Ok(VerifyOutcome {
        accepted: reason.is_none(),
        reason,
        answers,
    })
}

/// The deterministic half of `rewrite_shorter`: fewer lines, or as many lines and fewer chars.
pub(crate) fn shorter(candidate: &str, original: &str) -> bool {
    let (c, o) = (candidate.lines().count(), original.lines().count());
    c < o || (c == o && candidate.chars().count() < original.chars().count())
}

const FRONTIER_PROMPT: &str = include_str!("schema/frontier.md");

/// A generative model run on the rubric checklist; its labels become one-hot answers.
/// Its request lives here, not in llm.rs: any edit to llm.rs invalidates legacy cached verdicts.
pub struct Generative {
    /// For the preflight and the token totals.
    client: LlmClient,
    url: String,
    model: String,
    api_key: Option<String>,
}

impl Generative {
    pub fn new(cfg: &Config) -> Self {
        // judge_endpoint is documented as a full URL; the client wants the base it extends.
        let url = cfg.judge_url();
        let base = url.trim_end_matches('/');
        let base = base.strip_suffix("/chat/completions").unwrap_or(base);
        let mut c = cfg.clone();
        c.endpoint = base.to_string();
        c.model = cfg.judge_model.clone();
        Generative {
            url: format!("{base}/chat/completions"),
            client: LlmClient::new(&c),
            model: cfg.judge_model.clone(),
            api_key: cfg.api_key.clone(),
        }
    }

    /// One chat completion of `system` and `state`, waiting up to `secs`; returns the reply text.
    fn chat(&self, system: &str, state: &str, secs: u64) -> Result<String> {
        let body = serde_json::to_vec(&serde_json::json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": state},
            ],
            "max_tokens": 4096,
            "temperature": 0,
        }))
        .context("encoding judge request")?;
        let resp = post(&self.url, self.api_key.as_deref(), &body, secs)?;
        if !(200..300).contains(&resp.status_code) {
            bail!(
                "judge returned HTTP {} {}",
                resp.status_code,
                resp.reason_phrase
            );
        }
        let v: Value =
            serde_json::from_slice(resp.as_bytes()).context("failed to parse judge response")?;
        if v.get("usage").is_some_and(|u| !u.is_null()) {
            let n = |p: &str| v.pointer(p).and_then(Value::as_u64).unwrap_or(0);
            let t = &self.client.tokens;
            t.requests.fetch_add(1, Ordering::Relaxed);
            t.prompt
                .fetch_add(n("/usage/prompt_tokens"), Ordering::Relaxed);
            t.completion
                .fetch_add(n("/usage/completion_tokens"), Ordering::Relaxed);
            let cached = n("/usage/prompt_tokens_details/cached_tokens");
            t.cached.fetch_add(cached, Ordering::Relaxed);
        }
        v.pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .map(str::to_string)
            .context("judge response had no reply text")
    }
}

const REWRITE_PROMPT: &str = include_str!("schema/rewrite.md");

/// The verified-rewrite generator: `generator_model` at `endpoint`, on the rewrite prompt.
pub(crate) fn generator(cfg: &Config) -> Generative {
    Generative::new(&Config {
        judge: crate::types::JudgeKind::Generative,
        judge_endpoint: None,
        judge_model: cfg.generator_model.clone(),
        ..cfg.clone()
    })
}

impl Generative {
    /// The replacement text for a comment the policy sent to `verdict` (rewrite_shorter or
    /// fix_wrong_claim); None when the reply holds no usable text.
    pub(crate) fn rewrite(&self, state: &str, verdict: &str) -> Result<Option<String>> {
        // A reasoning model's rewrite takes about a minute; some servers drop a request without
        // closing it, so do not wait the judge's 600 s.
        let reply = self.chat(REWRITE_PROMPT, &format!("{state}\nverdict: {verdict}"), 240)?;
        Ok(clean_rewrite(&reply))
    }
}

/// The reply minus reasoning, code fences and comment markers the model added anyway.
fn clean_rewrite(reply: &str) -> Option<String> {
    let reply = reply.rsplit("</think>").next().unwrap_or(reply);
    let marker = regex::Regex::new(r"^(?://+!?|#+|/\*+|\*)(?:\s|$)").unwrap();
    let lines: Vec<String> = reply
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("```") && *l != "*/")
        .map(|l| {
            marker
                .replace(l, "")
                .trim_end_matches("*/")
                .trim()
                .to_string()
        })
        .collect();
    let text = lines.join("\n").trim().to_string();
    let text = text.trim_matches(['"', '`']).trim();
    (!text.is_empty() && !text.eq_ignore_ascii_case("delete")).then(|| text.to_string())
}

/// judges/generative.py::parse_labels: the last JSON object holding "verdict", bare or under
/// "labels", after `<think>` blocks are removed.
pub(crate) fn parse_labels(text: &str) -> Option<Map<String, Value>> {
    let text = regex::Regex::new(r"(?s)<think>.*?</think>")
        .unwrap()
        .replace_all(text, "");
    let mut found = None;
    for (i, _) in text.match_indices('{') {
        let Some(Ok(Value::Object(obj))) = serde_json::Deserializer::from_str(&text[i..])
            .into_iter::<Value>()
            .next()
        else {
            continue;
        };
        let obj = match obj.get("labels") {
            Some(Value::Object(l)) => l.clone(),
            Some(_) => continue, // generative.py: a non-object "labels" is not a labels object
            None => obj,
        };
        if obj.contains_key("verdict") {
            found = Some(obj);
        }
    }
    found
}

/// judges/__init__.py::one_hot: labels to `{qid: {option: 1.0}}`; nulls and the free-text
/// `rewrite` and `rename_suggestion` are skipped.
pub(crate) fn one_hot(labels: &Map<String, Value>) -> Answers {
    labels
        .iter()
        .filter(|(q, v)| !v.is_null() && !matches!(q.as_str(), "rewrite" | "rename_suggestion"))
        .map(|(q, v)| {
            let option = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            (q.clone(), BTreeMap::from([(option, 1.0)]))
        })
        .collect()
}

impl Judge for Generative {
    fn judge(&self, state: &str, _questions: &Questions) -> Result<Judgement> {
        let start = Instant::now();
        // 600 s as in judges/__init__.py::post_json: a 4096-token reply can outlast 60 s.
        let text = self.chat(FRONTIER_PROMPT, state, 600)?;
        let latency_ms = start.elapsed().as_millis() as u64;
        // As in generative.py: no labels (say a reply cut off at max_tokens) is a scored review.
        let labels = parse_labels(&text).unwrap_or_default();
        let holistic = labels
            .get("verdict")
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok(Judgement {
            answers: one_hot(&labels),
            holistic,
            input_tokens: None,
            latency_ms,
        })
    }

    fn one_hot(&self) -> bool {
        true
    }

    fn name(&self) -> String {
        format!("generative:{}", self.model)
    }

    fn check(&self) -> Result<()> {
        self.client.check()
    }

    fn tokens(&self) -> &TokenTotals {
        &self.client.tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    /// Serves `reply` to `n` requests and returns the URL plus the request bodies it saw.
    fn serve(reply: Value, n: usize) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        let h = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..n {
                let (mut s, _) = listener.accept().unwrap();
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut len = 0;
                let mut request_line = String::new();
                r.read_line(&mut request_line).unwrap();
                loop {
                    let mut l = String::new();
                    r.read_line(&mut l).unwrap();
                    if l == "\r\n" {
                        break;
                    }
                    if let Some(v) = l.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                }
                let mut b = vec![0; len];
                r.read_exact(&mut b).unwrap();
                seen.push(request_line + &String::from_utf8(b).unwrap());
                let body = reply.to_string();
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
            seen
        });
        (url, h)
    }

    fn questions() -> Questions {
        let q = |t: &str| Json::Object(vec![("type".into(), Json::String(t.into()))]);
        vec![("zeta".into(), q("noul")), ("alpha".into(), q("choice"))]
    }

    /// Replies with fixed noul probabilities and records the states it was asked about.
    struct MockJudge(
        Vec<(&'static str, f64)>,
        std::sync::Mutex<Vec<String>>,
        TokenTotals,
    );
    impl Judge for MockJudge {
        fn judge(&self, state: &str, _q: &Questions) -> Result<Judgement> {
            self.1.lock().unwrap().push(state.to_string());
            let answers = self
                .0
                .iter()
                .map(|&(q, p)| (q.to_string(), BTreeMap::from([("true".into(), p)])))
                .collect();
            Ok(Judgement {
                answers,
                holistic: None,
                input_tokens: None,
                latency_ms: 0,
            })
        }
        fn name(&self) -> String {
            "mock".into()
        }
        fn check(&self) -> Result<()> {
            Ok(())
        }
        fn tokens(&self) -> &TokenTotals {
            &self.2
        }
    }

    #[test]
    fn post_does_not_resend_after_a_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let err = post(&url, None, b"{}", 1).unwrap_err();
        assert!(format!("{err:#}").contains("timed out"), "{err:#}");
        let mut accepted = 0;
        while listener.accept().is_ok() {
            accepted += 1;
        }
        assert_eq!(accepted, 1);
    }

    #[test]
    fn clean_rewrite_strips_reasoning_fences_and_markers() {
        let reply = "<think>maybe delete</think>\n```python\n# Hold the lock: the refresher writes it.\n```";
        assert_eq!(
            clean_rewrite(reply).as_deref(),
            Some("Hold the lock: the refresher writes it.")
        );
        assert_eq!(
            clean_rewrite("/**\n * Summary.\n * @param x - id\n */").as_deref(),
            Some("Summary.\n@param x - id")
        );
        assert_eq!(clean_rewrite("DELETE"), None);
        assert_eq!(clean_rewrite("  \n"), None);
    }

    #[test]
    fn verify_rewrite_accepts_and_rejects() {
        let src = "def f(x):\n    # hold the lock: the refresher writes it\n    return x\n";
        let tree = crate::parse::parse_tree(src, crate::Language::Python).unwrap();
        let block = crate::parse::group_blocks(
            src,
            crate::parse::extract_comments(src, crate::Language::Python).unwrap(),
        )
        .remove(0);
        let ctx = crate::context::item_context(
            src,
            &tree,
            crate::Language::Python,
            std::path::Path::new("a.py"),
            &crate::Item::Comment(block),
        );
        let th = Thresholds::strict();
        let run = |p: f64, a: f64, g: f64| {
            let j = MockJudge(
                vec![("preserves", p), ("adds", a), ("g1_candidate", g)],
                Default::default(),
                TokenTotals::default(),
            );
            let out = verify_rewrite(&j, &ctx.comment, "# hold the lock", &ctx, &th).unwrap();
            let state = j.1.lock().unwrap().remove(0);
            assert!(
                state.starts_with("original:\n# hold the lock: the"),
                "{state}"
            );
            assert_eq!(out.accepted, out.reason.is_none());
            out.reason
        };
        assert_eq!(run(0.97, 0.02, 0.2), None);
        assert_eq!(run(0.9, 0.02, 0.2).as_deref(), Some("preserves 0.90"));
        assert_eq!(run(0.97, 0.1, 0.2).as_deref(), Some("adds 0.10"));
        assert_eq!(run(0.97, 0.02, 0.5).as_deref(), Some("g1_candidate 0.50"));
        let j = MockJudge(
            vec![("preserves", 0.97)],
            Default::default(),
            TokenTotals::default(),
        );
        let out = verify_rewrite(&j, "# a", "# b", &ctx, &th).unwrap();
        assert_eq!(out.reason.as_deref(), Some("adds missing"));
    }

    #[test]
    fn shorter_rule() {
        assert!(shorter("# a\n", "# a\n# b\n"));
        assert!(shorter("# ab", "# abc"));
        assert!(!shorter("# abc", "# abc"));
        assert!(!shorter("# a\n# b\n", "# abcdefgh\n"));
    }

    #[test]
    fn parse_labels_shapes() {
        let clean = parse_labels(r#"{"verdict": "keep", "c2": true}"#).unwrap();
        assert_eq!(clean["verdict"], "keep");
        let nested = parse_labels(r#"Done: {"labels": {"verdict": "delete"}, "note": 1}"#).unwrap();
        assert_eq!(nested["verdict"], "delete");
        let thinking = parse_labels(
            r#"<think>maybe {"verdict": "keep"}</think> {"verdict": "keep"} then {"verdict": "review", "c2": false}"#,
        )
        .unwrap();
        assert_eq!(thinking["verdict"], "review");
        assert!(parse_labels("no json {here}").is_none());
    }

    #[test]
    fn one_hot_values() {
        let labels = serde_json::json!({
            "a": true, "b": 3, "c": "keep", "d": null, "rewrite": "x", "rename_suggestion": "y"
        });
        let got = one_hot(labels.as_object().unwrap());
        assert_eq!(got.len(), 3);
        assert_eq!(got["a"]["true"], 1.0);
        assert_eq!(got["b"]["3"], 1.0);
        assert_eq!(got["c"]["keep"], 1.0);
    }

    #[test]
    fn systemone_round_trip() {
        let reply = serde_json::json!({
            "answers": {
                "zeta": {"type": "noul", "noul": 0.25},
                "alpha": {"type": "choice", "probabilities": {"a": 0.2, "b": 0.8}},
                "verdict": {"type": "choice", "probabilities": {"keep": 0.1, "delete": 0.9}},
            },
            "usage": {"input_tokens": 3},
        });
        let (url, h) = serve(reply, 2);
        let j = SystemOne::new(&url, "m", None);
        let long = "x".repeat(400);
        let got = j.judge(&long, &questions()).unwrap();
        assert_eq!(got.answers["zeta"]["true"], 0.25);
        assert_eq!(got.answers["zeta"]["false"], 0.75);
        assert_eq!(got.answers["alpha"]["b"], 0.8);
        assert_eq!(got.holistic.as_deref(), Some("delete"));
        assert_eq!(got.input_tokens, Some(3));
        // 3 < 400 / 4 / 2: warned once, not twice
        assert!(j.truncation_warned.load(Ordering::Relaxed));
        assert_eq!(j.name(), "systemone:m");
        Judge::check(&j).unwrap();
        // request carries questions in the order given, not sorted
        let body = &h.join().unwrap()[0];
        assert!(body.find("zeta").unwrap() < body.find("alpha").unwrap());
    }

    #[test]
    fn generative_reply_without_labels_is_review() {
        let reply = serde_json::json!({
            "choices": [{"message": {"content": "{\"c2\": tru"}}],
            "usage": {"prompt_tokens": 7, "completion_tokens": 4096},
        });
        let (url, h) = serve(reply, 1);
        let mut cfg = crate::reduction::tests::cfg(crate::JudgeKind::Generative);
        cfg.judge_endpoint = Some(format!("{url}/chat/completions"));
        let g = Generative::new(&cfg);
        let got = g.judge("s", &questions()).unwrap();
        assert_eq!(g.tokens().prompt.load(Ordering::Relaxed), 7);
        assert!(got.answers.is_empty() && got.holistic.is_none());
        let th = crate::policy::Thresholds::strict();
        let v = crate::policy::judge_verdict(&got, true, "inline", "# x", "", &th);
        assert_eq!(v, crate::policy::Verdict::Review);
        assert!(h.join().unwrap()[0].starts_with("POST /v1/systemone/chat/completions "));
    }
}
