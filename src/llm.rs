//! OpenAI-compatible chat completions client used to abstractively summarize a comment.
use crate::types::Config;
use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use serde::Deserialize;
use serde_json::json;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::Semaphore;

/// What the model decided about a comment block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The comment carries nothing the code does not already say.
    Delete,
    /// Replace the block with this single terse line.
    Line(String),
}

const SYSTEM_PROMPT: &str = "You classify a source code comment and reply with exactly one line. First the class \
code (K1-K7 or D1-D8 below). For a D class that is the whole reply. For a K class, follow the code \
with a space and a replacement comment of at most {max_words} words. No quotes, no markdown, no \
preamble, no comment delimiters.\n\n\
KEEP only if the comment falls in one of these classes:\n\
K1 Library, runtime, OS or browser quirk: the call behaves differently than its name or docs \
suggest (silently drops, reuses, truncates, fires twice, hangs) and this code works around it.\n\
K2 Ordering requirement: this line must come before or after another or something breaks silently.\n\
K3 Shared or mutable state hazard: lock must be held, object shared across threads, instances or \
requests, must not be mutated.\n\
K4 Security or privacy hazard: the value holds secrets or untrusted input; never log, never trust.\n\
K5 Unit, index or encoding mismatch that the code does not show: seconds vs milliseconds, device \
vs CSS pixels, sectors vs bytes, 1-indexed vs 0-indexed. If the code line already converts \
(* 1000, + 1, errors=...) it is visible and not K5.\n\
K6 Silent failure: without this exact form something hangs, leaks or loses data with no error.\n\
K7 Looks like a bug or dead code but is required: an unused parameter a framework needs, a \
redundant-looking call that must stay. Not for compatibility notes or design choices.\n\n\
DELETE if the comment is any of these, even when phrased as a warning:\n\
D1 Describes what the code does or the steps it takes.\n\
D2 Justifies a call or operator whose name already says why: compare_digest, safe_load, freeze, \
debounce, ??, range bounds.\n\
D3 History: tickets, authors, dates, past bugs, what the code used to do.\n\
D4 Describes other modules, services or files, or what must be kept in sync elsewhere.\n\
D5 General education about a library or language feature.\n\
D6 Section banners, overviews, tables of contents, plans.\n\
D7 Commented-out code, apologies, opinions, tuning anecdotes.\n\
D8 Merely useful context that does not prevent a mistake.\n\n\
Before choosing a K class, check the next code line: if the identifier, argument name, operator \
or arithmetic on it already reveals the fact (safe_load, compare_digest, errors='replace', \
?? '', * 1000, + 1, allow_redirects=False) the reader can see it and the class is D2.\n\n\
When you keep, write the line as: the trap, then a semicolon, then what to do. Name the library \
or identifier. Drop the story, the reasoning and the consequences beyond the trap itself. \
Example reply: K1 requests drops Authorization on cross-host redirects; follow them manually. \
Most comments are a D class.";

/// Few-shot demos: (comment prose, next code line, expected reply).
const DEMOS: &[(&str, &str, &str)] = &[
    (
        "Loop over each user in the list and check whether their subscription has expired. If it \
         has, add them to the expired list so we can send the reminder email later on in the batch job.",
        "for user in users:",
        "D1",
    ),
    (
        "Important: this must be called with the lock held. The cache map is not thread safe and \
         we've seen corruption in production when two workers refresh at the same time. See the \
         incident from last March.",
        "def _refresh_cache(self):",
        "K3 caller must hold self._lock; the cache map is not thread safe",
    ),
    (
        "We use compare_digest here instead of == because == short-circuits on the first differing \
         byte and an attacker could measure the response time to learn the token one byte at a time. \
         This is a classic timing attack.",
        "if not hmac.compare_digest(provided, expected):",
        "D2",
    ),
    (
        "Previously this used the legacy HttpClient from utils/http, but that was removed in the v3 \
         refactor (ticket PLAT-2211). The new fetch wrapper handles retries itself, so we just call \
         it here and let the middleware layer deal with auth headers.",
        "const res = await fetchJson(url);",
        "D3",
    ),
    (
        "Note that the timeout here is in seconds, not milliseconds like everywhere else in this \
         file, because the upstream API multiplies it by 1000 on its side. Passing 5000 here means \
         the request will wait over an hour.",
        "timeout: 5,",
        "K5 timeout is seconds, not ms; upstream multiplies by 1000",
    ),
    (
        "We freeze the config object so that nothing downstream can accidentally mutate it. An \
         earlier version had a nasty bug where a plugin overwrote the base URL at runtime and every \
         request went to the wrong host.",
        "export const config = Object.freeze({",
        "D2",
    ),
    (
        "We use Array.prototype.reduce here to build up the lookup object in a single pass. reduce \
         takes an accumulator and the current item and returns the new accumulator, which is more \
         efficient than creating a new object each iteration with the spread operator.",
        "const byId = items.reduce((acc, item) => {",
        "D5",
    ),
    (
        "Ugly hack: we sleep for 50ms before closing because the underlying C library (libfoo 2.3) \
         drops the last buffered write if close() is called immediately after write(). This is fixed \
         upstream in 2.4 but we can't upgrade yet.",
        "time.sleep(0.05)",
        "K1 libfoo 2.3 drops the last buffered write if close() follows write() immediately",
    ),
    (
        "Wait 300ms after the last keystroke before firing the search so that we don't hammer the \
         API with a request per character. 300 felt about right in testing; 500 felt laggy.",
        "const search = debounce(runSearch, 300);",
        "D7",
    ),
    (
        "Initialize the running total to zero. Then for every row that matches the filter, add its \
         amount to the total. Finally return the total to the caller.",
        "total = 0",
        "D1",
    ),
    (
        "We decode with errors='replace' here because the partner feed occasionally contains \
         invalid UTF-8 sequences and we would rather show a replacement character than crash the \
         whole import for one bad row.",
        "lines = raw.decode(\"utf-8\", errors=\"replace\").splitlines()",
        "D2",
    ),
    (
        "The fallback locale is 'en' because that is the only bundle guaranteed to ship with every \
         build; the others are loaded lazily from the CDN and may be missing in offline mode.",
        "const locale = SUPPORTED.has(wanted) ? wanted : 'en';",
        "D8",
    ),
];

/// Bound what we send: traps are stated early, and long blocks are mostly story.
const MAX_PROSE_WORDS: usize = 150;
const MAX_CONTEXT_CHARS: usize = 80;

fn user_message(prose: &str, context: &str) -> String {
    let prose: Vec<&str> = prose.split_whitespace().take(MAX_PROSE_WORDS).collect();
    let mut m = format!("Comment:\n{}", prose.join(" "));
    if !context.is_empty() {
        let ctx: String = context.chars().take(MAX_CONTEXT_CHARS).collect();
        m.push_str(&format!("\n\nNext code line: {ctx}"));
    }
    m
}

pub struct LlmClient {
    client: reqwest::Client,
    endpoint: String,
    model: String,
    api_key: Option<String>,
    semaphore: Semaphore,
    pub tokens: TokenTotals,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize, Default)]
struct Usage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<PromptDetails>,
}

#[derive(Deserialize, Default)]
struct PromptDetails {
    #[serde(default)]
    cached_tokens: u64,
}

/// Running token totals across all requests (prompt, completion, cached-prompt).
#[derive(Debug, Default)]
pub struct TokenTotals {
    pub prompt: AtomicU64,
    pub completion: AtomicU64,
    pub cached: AtomicU64,
    pub requests: AtomicU64,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

#[derive(Deserialize)]
struct ChatMessage {
    content: String,
}

impl LlmClient {
    pub fn new(cfg: &Config) -> LlmClient {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .expect("failed to build reqwest client");
        let endpoint = cfg
            .endpoint
            .clone()
            .unwrap_or_default()
            .trim_end_matches('/')
            .to_string();
        LlmClient {
            client,
            endpoint,
            model: cfg.model.clone(),
            api_key: cfg.api_key.clone(),
            semaphore: Semaphore::new(cfg.llm_concurrency.max(1)),
            tokens: TokenTotals::default(),
        }
    }

    /// Ask the model what to do with a comment block. `prose` is the cleaned comment text and
    /// `context` the first non-blank code line after it (may be empty). Returns Err on transport
    /// failure or unusable output; the caller then falls back to the extractive summary.
    pub async fn summarize(&self, prose: &str, context: &str, max_words: usize) -> Result<Verdict> {
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|e| anyhow!("semaphore closed: {e}"))?;

        let mut messages = vec![json!({
            "role": "system",
            "content": SYSTEM_PROMPT.replace("{max_words}", &max_words.to_string()),
        })];
        for (demo_prose, demo_ctx, reply) in DEMOS {
            messages.push(json!({"role": "user", "content": user_message(demo_prose, demo_ctx)}));
            messages.push(json!({"role": "assistant", "content": reply}));
        }
        messages.push(json!({"role": "user", "content": user_message(prose, context)}));

        let body = json!({
            "model": self.model,
            "messages": messages,
            "max_tokens": 48,
            "temperature": 0,
        });

        let url = format!("{}/chat/completions", self.endpoint);

        let raw = match self.post(&url, &body).await {
            Ok(r) => r,
            Err(_) => self.post(&url, &body).await?, // one retry on transport error
        };

        let cleaned = clean_reply(&raw);
        if std::env::var_os("COMMENTREDUCR_DEBUG").is_some() {
            eprintln!("llm raw: {raw:?}");
        }
        if cleaned.is_empty() {
            bail!("empty reply from LLM after cleanup");
        }
        // Reply shape: "D3" (delete) or "K1 <line>" (keep). Tolerate a bare DELETE too.
        let (code, rest) = cleaned.split_once(' ').unwrap_or((cleaned.as_str(), ""));
        let code = code.trim_end_matches([':', '.', '-']);
        let is_class =
            |c: char| code.len() == 2 && code.starts_with(c) && code.as_bytes()[1].is_ascii_digit();
        if is_class('D') || code.eq_ignore_ascii_case("delete") {
            return Ok(Verdict::Delete);
        }
        let cleaned = if is_class('K') {
            rest.trim().to_string()
        } else {
            cleaned
        };
        if cleaned.is_empty() {
            // The model wanted to keep it but could not say what the trap is: delete.
            return Ok(Verdict::Delete);
        }
        let word_count = cleaned.split_whitespace().count();
        if word_count > 2 * max_words {
            bail!(
                "LLM reply too long: {word_count} words (limit {})",
                2 * max_words
            );
        }
        Ok(Verdict::Line(cleaned))
    }

    async fn post(&self, url: &str, body: &serde_json::Value) -> Result<String> {
        let mut req = self.client.post(url).json(body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await.context("LLM request failed")?;
        let resp = resp
            .error_for_status()
            .context("LLM returned an error status")?;
        let parsed: ChatResponse = resp.json().await.context("failed to parse LLM response")?;
        if let Some(u) = &parsed.usage {
            self.tokens.requests.fetch_add(1, Ordering::Relaxed);
            self.tokens
                .prompt
                .fetch_add(u.prompt_tokens, Ordering::Relaxed);
            self.tokens
                .completion
                .fetch_add(u.completion_tokens, Ordering::Relaxed);
            let cached = u
                .prompt_tokens_details
                .as_ref()
                .map_or(0, |d| d.cached_tokens);
            self.tokens.cached.fetch_add(cached, Ordering::Relaxed);
        }
        let content = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("LLM response had no choices"))?
            .message
            .content;
        Ok(content)
    }
}

/// Matches a leading conversational preamble ("Here is the summary: ...", "Sure, ...:") up to
/// and including its colon, so it can be stripped and the real content kept.
static PREAMBLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(here('s| is)\b|sure\b|certainly\b|the (one[- ]line )?summary( is)?\b)[^:]{0,40}:\s*")
        .unwrap()
});

/// Take the first non-empty, non-fence line, trim, strip a leading conversational preamble,
/// strip wrapping quotes/backticks, strip a leading "#", "//" or "*" run, collapse internal
/// whitespace, strip trailing "*/". Returns "" if nothing usable remains (e.g. the cleaned
/// text is empty, only punctuation, or a single very short token).
fn clean_reply(raw: &str) -> String {
    // Skip markdown code-fence marker lines (```lang / ```) entirely rather than treating them
    // as content.
    let Some(mut line) = raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("```"))
        .find(|l| !l.is_empty())
    else {
        return String::new();
    };

    if let Some(m) = PREAMBLE_RE.find(line) {
        let rest = line[m.end()..].trim();
        if !rest.is_empty() {
            line = rest;
        }
    }

    // Strip wrapping quotes/backticks.
    loop {
        let bytes = line.as_bytes();
        if bytes.len() >= 2 {
            let first = bytes[0];
            let last = bytes[bytes.len() - 1];
            let is_pair = matches!(first, b'"' | b'\'' | b'`') && first == last;
            if is_pair {
                line = &line[1..line.len() - 1];
                line = line.trim();
                continue;
            }
        }
        break;
    }

    // Strip a leading "#", "//" or "*" run.
    let line = line.trim_start_matches(['#', '/', '*']).trim_start();

    // Strip trailing "*/".
    let line = line.trim_end().trim_end_matches("*/").trim_end();

    // Collapse internal whitespace.
    let cleaned = line.split_whitespace().collect::<Vec<_>>().join(" ");

    // Reject junk that shouldn't be accepted as a summary: pure punctuation (e.g. a stray
    // backtick left over from a mangled fence), or a single token too short to be a real word.
    if cleaned.is_empty() || cleaned.chars().all(|c| !c.is_alphanumeric()) {
        return String::new();
    }
    if !cleaned.contains(' ') && cleaned.len() < 2 {
        return String::new();
    }

    cleaned
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_quoted_reply() {
        assert_eq!(
            clean_reply("\"Parses the config file.\"\n"),
            "Parses the config file."
        );
    }

    #[test]
    fn strips_comment_leader_and_collapses_whitespace() {
        assert_eq!(clean_reply("//  hello   world  "), "hello world");
    }

    #[test]
    fn empty_reply_is_empty() {
        assert_eq!(clean_reply("\n\n  \n"), "");
    }

    #[test]
    fn markdown_fence_is_skipped_not_mangled() {
        assert_eq!(
            clean_reply("```\nThis function computes epsilon for the solver.\n```"),
            "This function computes epsilon for the solver."
        );
    }

    #[test]
    fn conversational_preamble_is_stripped() {
        assert_eq!(
            clean_reply("Here is the one line summary: parses the config file"),
            "parses the config file"
        );
    }

    #[test]
    fn lone_punctuation_reply_is_rejected() {
        assert_eq!(clean_reply("```"), "");
        assert_eq!(clean_reply("`"), "");
    }
}
