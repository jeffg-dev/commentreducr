//! OpenAI-compatible chat completions client used to abstractively summarize a comment.
use crate::types::Config;
use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use serde::Deserialize;
use serde_json::json;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// What the model decided about a comment block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The comment carries nothing the code does not already say.
    Delete,
    /// Replace the block with this single terse line.
    Line(String),
}

/// What the model is shown about one docstring.
pub struct DocRequest<'a> {
    pub kind: &'a str,
    pub name: &'a str,
    pub signature: &'a str,
    pub is_test: bool,
    pub in_test_file: bool,
    pub text: &'a str,
    pub body_preview: &'a [String],
    pub body_lines: usize,
}

/// What the model decided about a docstring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocVerdict {
    /// The docstring carries nothing a caller needs; remove it.
    Delete,
    /// Replace the docstring with this text (lines joined with '\n', trimmed).
    Text(String),
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

const DOC_SYSTEM_PROMPT: &str = "You rewrite or delete one source-code docstring. Reply with exactly one of two \
shapes and nothing else: the single word DELETE, or the word KEEP followed by the replacement \
docstring text on the lines after it. Plain prose only: no quotes, no triple-quotes, no markdown, \
no preamble.\n\n\
A docstring earns its place only when it states a contract for the CALLER: which parameters or \
fields matter and why, the return contract, a gotcha, a call-order or ordering hazard, an \
invariant not visible in the signature, or when to reach for this over an alternative. It must \
stay true after the body is rewritten -- it describes the contract, not the implementation.\n\n\
DELETE when the docstring: narrates what the code does step by step; restates the name or \
signature in words; reads like a spec written before the code (re-typing arguments or a request \
schema the signature or types already show); carries a ticket, PR number, author, date, or phase \
label; argues architecture or product rationale instead of usage; counts or lists today's \
callers; sits on a trivial one-liner, pass-through, dunder, or plain data class where the name \
and signature already say it; or is a maintenance note (\"TEMPORARY\", \"delete this file\").\n\n\
KEEP (rewritten) only the buried contract or hazard, terse, no story, when one is genuinely \
there.\n\n\
Test code is different: a test function or method docstring says what it guards or tests, at \
most 2 lines. A test module or test class docstring is at most one short paragraph -- 5 lines, \
about 80 words -- and is DELETE outright if the tests are self-explanatory. Non-test docstrings: \
a one-line summary first, then at most one short paragraph or a short Args/Returns list, 15 lines \
hard maximum. Most docstrings on ordinary code are DELETE.";

/// Few-shot demos: (kind, name, signature, is_test, in_test_file, docstring text, body preview
/// lines, body line count, expected reply). Nine categories: bloated test module, bloated test
/// function, name-restating one-liner, implementation narration, history/ticket, a real
/// call-order hazard, a pass-through/dunder, a class with real invariants, a caller headcount.
#[allow(clippy::type_complexity)]
const DOC_DEMOS: &[(&str, &str, &str, bool, bool, &str, &[&str], usize, &str)] = &[
    (
        "module",
        "",
        "",
        true,
        true,
        "This test module covers the widget-export pipeline end to end.\n\n\
         It was added in the Q3 hardening pass (see TICKET-881) after a production incident \
         where a malformed template crashed the export worker. Below we test happy path export, \
         template validation, and the retry logic that handles a transient upload failure. \
         Please keep new tests in this file rather than creating widget_export_test_extra.py.",
        &[
            "def test_export_happy_path():",
            "def test_export_rejects_bad_template():",
            "def test_export_retries_transient_upload_failure():",
        ],
        42,
        "KEEP\nCovers the widget-export pipeline: happy-path export, template\nvalidation, and retry on a transient upload failure.",
    ),
    (
        "function",
        "test_retry_gives_up_after_three_attempts",
        "def test_retry_gives_up_after_three_attempts():",
        true,
        true,
        "This test checks that the retry helper stops calling the flaky upload function after \
         three attempts and raises the last error it saw. We set up a mock that fails every \
         time, call retry_upload three times, and assert that a fourth call never happens. This \
         guards against a past regression (JIRA-2290) where the loop ran forever.",
        &["with mock_always_fails():", "    retry_upload()"],
        8,
        "KEEP\nRetry gives up after 3 attempts and raises the last error, rather than\nlooping forever.",
    ),
    (
        "function",
        "mark_read",
        "def mark_read(self) -> None:",
        false,
        false,
        "Mark the notification as read.",
        &["self.is_read = True", "self.read_at = utcnow()"],
        3,
        "DELETE",
    ),
    (
        "function",
        "normalize_tags",
        "def normalize_tags(tags: list[str]) -> list[str]:",
        false,
        false,
        "Loop over each tag, strip whitespace, lowercase it, and skip any tag that becomes empty \
         after stripping. Then dedupe the list while preserving order and return it.",
        &["seen = set()", "out = []"],
        9,
        "DELETE",
    ),
    (
        "module",
        "",
        "",
        false,
        false,
        "Legacy export shim for the old CSV format.\n\n\
         TEMPORARY: kept only until the billing team finishes migrating off the v1 report \
         endpoint (BILL-1042). Delete this module once billing/reports.py no longer imports \
         from here -- check callers before removing.",
        &["def legacy_csv_export(rows):"],
        18,
        "DELETE",
    ),
    (
        "function",
        "_release_hold_before_charge",
        "def _release_hold_before_charge(order: Order) -> None:",
        false,
        false,
        "Releases the payment hold on an order before the final charge runs.\n\n\
         Call this before charge_order, never after: charge_order re-reads the order's \
         held_amount to compute the refundable difference, and once the hold is released that \
         field reads zero, so calling this after charge_order would make the refund calculation \
         silently think nothing was held. We found this the hard way in staging last month when \
         a retry path called these in the wrong order and issued a full-price charge with no \
         refund of the original hold.\n\n\
         Internally this just flips a boolean and appends a ledger row.",
        &["order.hold_released = True", "order.ledger.append(...)"],
        10,
        "KEEP\nMust be called before charge_order, never after: charge_order reads\nheld_amount to compute the refund, and it reads zero once the hold is\nreleased.",
    ),
    (
        "function",
        "__repr__",
        "def __repr__(self) -> str:",
        false,
        false,
        "String representation of the object for debugging.",
        &["return f\"<Invoice id={self.id} total={self.total}>\""],
        2,
        "DELETE",
    ),
    (
        "class",
        "SeatAllocation",
        "class SeatAllocation:",
        false,
        false,
        "Represents an allocation of seats to a subscription tier.\n\n\
         A SeatAllocation ties a tenant to a purchased seat count. This class was introduced \
         when we added tiered pricing (see PROJ-990) to replace the old flat per-user billing \
         model described in billing_v1.py, which is now deprecated. Among live rows, at most \
         one allocation may be active per tenant at a time; a tenant with zero active \
         allocations is treated as unlicensed and every gated feature is denied. Soft-deleted \
         rows do not count toward the one-active-per-tenant limit, so reactivating one after a \
         downgrade requires deleting the newer row first.",
        &["tenant_id: int", "seat_count: int", "active: bool"],
        16,
        "KEEP\nAt most one allocation may be active per tenant; zero active means\nunlicensed and every gated feature is denied. Soft-deleted rows do not\ncount toward the limit, so reactivating one requires deleting the newer\nrow first.",
    ),
    (
        "function",
        "canonical_currency_code",
        "def canonical_currency_code(code: str) -> str:",
        false,
        false,
        "Uppercases and validates a 3-letter ISO 4217 currency code.\n\n\
         This is called from roughly 40 call sites across billing, invoicing, and the public \
         API serializers (BillingForm, InvoiceLineItem, and six other DTOs at last count), so \
         changing the validation here affects all of them. Whenever a new currency needs \
         support, add it to SUPPORTED_CURRENCIES in currency_constants.py first.",
        &[
            "code = code.strip().upper()",
            "if code not in SUPPORTED_CURRENCIES:",
        ],
        8,
        "KEEP\nUppercases and validates a 3-letter ISO 4217 currency code.",
    ),
];

/// Docstrings start with the summary, so truncating the tail costs little.
const MAX_DOC_WORDS: usize = 250;

fn doc_user_message(req: &DocRequest) -> String {
    let mut m = format!("Kind: {}", req.kind);
    if !req.name.is_empty() {
        m.push_str(&format!("\nName: {}", req.name));
    }
    if !req.signature.is_empty() {
        m.push_str(&format!("\nSignature: {}", req.signature));
    }
    m.push_str(&format!(
        "\nTest: {}",
        if req.is_test {
            "yes"
        } else if req.in_test_file {
            "no (in a test file)"
        } else {
            "no"
        }
    ));
    m.push_str(&format!("\nBody: {} lines", req.body_lines));
    if !req.body_preview.is_empty() {
        m.push('\n');
        m.push_str(&req.body_preview.join("\n"));
    }
    let words: Vec<&str> = req.text.split_whitespace().take(MAX_DOC_WORDS).collect();
    m.push_str(&format!("\n\nDocstring:\n{}", words.join(" ")));
    m
}

pub struct LlmClient {
    endpoint: String,
    model: String,
    api_key: Option<String>,
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

/// Plain copy of `TokenTotals` at one moment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub requests: u64,
    pub prompt: u64,
    pub completion: u64,
    pub cached: u64,
}

impl TokenTotals {
    pub fn snapshot(&self) -> TokenUsage {
        TokenUsage {
            requests: self.requests.load(Ordering::Relaxed),
            prompt: self.prompt.load(Ordering::Relaxed),
            completion: self.completion.load(Ordering::Relaxed),
            cached: self.cached.load(Ordering::Relaxed),
        }
    }
}

impl std::fmt::Display for TokenUsage {
    /// `N requests, P prompt (X/req, C cached), Q completion (Y/req)`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let rq = self.requests.max(1) as f64;
        write!(
            f,
            "{} requests, {} prompt ({:.0}/req, {} cached), {} completion ({:.1}/req)",
            self.requests,
            self.prompt,
            self.prompt as f64 / rq,
            self.cached,
            self.completion,
            self.completion as f64 / rq
        )
    }
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
        let endpoint = cfg.endpoint.trim_end_matches('/').to_string();
        LlmClient {
            endpoint,
            model: cfg.model.clone(),
            api_key: cfg.api_key.clone(),
            tokens: TokenTotals::default(),
        }
    }

    /// Preflight: one tiny completion to prove the endpoint is reachable and the model loads.
    pub fn check(&self) -> Result<()> {
        let body = json!({
            "model": self.model,
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 1,
        });
        self.post(&format!("{}/chat/completions", self.endpoint), &body)
            .with_context(|| {
                format!(
                    "cannot reach LLM at {} with model {} (--reduce needs it)",
                    self.endpoint, self.model
                )
            })?;
        Ok(())
    }

    /// Ask the model what to do with a comment block. `prose` is the cleaned comment text and
    /// `context` the first non-blank code line after it (may be empty). Returns Err on transport
    /// failure or unusable output.
    pub fn summarize(&self, prose: &str, context: &str, max_words: usize) -> Result<Verdict> {
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

        let raw = match self.post(&url, &body) {
            Ok(r) => r,
            Err(_) => self.post(&url, &body)?, // one retry on transport error
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

    /// Ask the model what to do with one docstring: delete it, or replace it with a shorter
    /// contract-only version. Returns Err on transport failure or an unusable reply.
    pub fn rewrite_docstring(&self, req: &DocRequest) -> Result<DocVerdict> {
        let mut messages = vec![json!({
            "role": "system",
            "content": DOC_SYSTEM_PROMPT,
        })];
        for &(
            kind,
            name,
            signature,
            is_test,
            in_test_file,
            text,
            body_preview,
            body_lines,
            reply,
        ) in DOC_DEMOS
        {
            let preview: Vec<String> = body_preview.iter().map(|s| s.to_string()).collect();
            let demo_req = DocRequest {
                kind,
                name,
                signature,
                is_test,
                in_test_file,
                text,
                body_preview: &preview,
                body_lines,
            };
            messages.push(json!({"role": "user", "content": doc_user_message(&demo_req)}));
            messages.push(json!({"role": "assistant", "content": reply}));
        }
        messages.push(json!({"role": "user", "content": doc_user_message(req)}));

        let body = json!({
            "model": self.model,
            "messages": messages,
            "max_tokens": 220,
            "temperature": 0,
        });

        let url = format!("{}/chat/completions", self.endpoint);
        let raw = match self.post(&url, &body) {
            Ok(r) => r,
            Err(_) => self.post(&url, &body)?, // one retry on transport error
        };

        if std::env::var_os("COMMENTREDUCR_DEBUG").is_some() {
            eprintln!("llm raw (docstring): {raw:?}");
        }

        parse_doc_reply(&raw, req)
    }

    fn post(&self, url: &str, body: &serde_json::Value) -> Result<String> {
        let mut req = minreq::post(url)
            .with_timeout(60)
            .with_json(body)
            .context("encoding LLM request")?;
        if let Some(key) = &self.api_key {
            req = req.with_header("Authorization", format!("Bearer {key}"));
        }
        let resp = req.send().context("LLM request failed")?;
        if !(200..300).contains(&resp.status_code) {
            bail!(
                "LLM returned HTTP {} {}",
                resp.status_code,
                resp.reason_phrase
            );
        }
        let parsed: ChatResponse = resp.json().context("failed to parse LLM response")?;
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

/// Matches a leading conversational preamble line before the DELETE/KEEP marker, e.g.
/// "Docstring:", "Here is the rewritten docstring:", "Sure:".
static DOC_PREAMBLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(docstring|here('s| is)\b.*|sure|certainly)\s*:?\s*$").unwrap()
});

/// Strip quotes wrapping the whole (trimmed) string: a matching `"""`/`'''` pair, or a matching
/// single `"`/`'`/`` ` `` pair. Repeats until no more wrapping is found.
fn strip_wrapping_quotes(s: &str) -> String {
    let mut t = s.trim();
    loop {
        if t.len() >= 6
            && ((t.starts_with("\"\"\"") && t.ends_with("\"\"\""))
                || (t.starts_with("'''") && t.ends_with("'''")))
        {
            t = t[3..t.len() - 3].trim();
            continue;
        }
        if t.len() >= 2 {
            let bytes = t.as_bytes();
            let first = bytes[0];
            let last = bytes[bytes.len() - 1];
            if matches!(first, b'"' | b'\'' | b'`') && first == last {
                t = t[1..t.len() - 1].trim();
                continue;
            }
        }
        break;
    }
    t.to_string()
}

/// Drop leading/trailing blank lines and collapse runs of blank lines to one.
fn trim_blank_edges_and_collapse(mut lines: Vec<String>) -> Vec<String> {
    while lines.first().is_some_and(|l| l.is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let mut out = Vec::with_capacity(lines.len());
    let mut prev_blank = false;
    for l in lines {
        let blank = l.is_empty();
        if blank && prev_blank {
            continue;
        }
        prev_blank = blank;
        out.push(l);
    }
    out
}

/// Trim trailing whitespace per line, then drop/collapse blank lines.
fn normalize_lines(text: &str) -> Vec<String> {
    let lines: Vec<String> = text.lines().map(|l| l.trim_end().to_string()).collect();
    trim_blank_edges_and_collapse(lines)
}

/// Line cap for the kind of docstring being rewritten: 2 for a test function/method, 5 for a
/// test module/class, 15 for everything else.
fn max_lines_for(req: &DocRequest) -> usize {
    if req.is_test {
        match req.kind {
            "function" => 2,
            _ => 5,
        }
    } else {
        15
    }
}

/// Parse one reply to the docstring-rewrite prompt. Skips code fences, strips a leading
/// preamble line and wrapping quotes, then reads the first remaining line as the verdict marker:
/// `DELETE` (case-insensitive, trailing punctuation tolerated) deletes regardless of anything
/// else in the reply; `KEEP` keeps the rest of the reply as text (empty rest -> Delete); neither
/// marker keeps the whole (cleaned) reply as text. The kept text is then normalized and capped
/// to the line limit for `req`'s kind; an empty result after capping is also Delete. Errors only
/// on a wholly empty reply.
fn parse_doc_reply(raw: &str, req: &DocRequest) -> Result<DocVerdict> {
    if raw.trim().is_empty() {
        bail!("empty reply from LLM");
    }

    let no_fence: Vec<&str> = raw
        .lines()
        .filter(|l| !l.trim_start().starts_with("```"))
        .collect();

    let mut lines: Vec<&str> = no_fence;
    while let Some(first) = lines.first() {
        let t = first.trim();
        if t.is_empty() || DOC_PREAMBLE_RE.is_match(t) {
            lines.remove(0);
            continue;
        }
        break;
    }
    if lines.is_empty() {
        bail!("empty reply from LLM after cleanup");
    }

    let joined = strip_wrapping_quotes(&lines.join("\n"));
    let lines: Vec<&str> = joined.lines().collect();
    if lines.is_empty() {
        bail!("empty reply from LLM after cleanup");
    }

    let first = lines[0].trim();
    let (marker, rest_of_first) = first.split_once(char::is_whitespace).unwrap_or((first, ""));
    let marker_core = marker.trim_end_matches(['.', ':', '!', '-']);

    let text = if marker_core.eq_ignore_ascii_case("delete") {
        None
    } else if marker_core.eq_ignore_ascii_case("keep") {
        let mut rest_lines = vec![rest_of_first.trim_start()];
        rest_lines.extend_from_slice(&lines[1..]);
        Some(rest_lines.join("\n"))
    } else {
        Some(joined.clone())
    };

    let Some(text) = text else {
        return Ok(DocVerdict::Delete);
    };

    // The kept text itself may still be wrapped in quotes (e.g. `KEEP` alone on the first line,
    // followed by a triple-quoted docstring).
    let text = strip_wrapping_quotes(&text);

    let normalized = normalize_lines(&text);
    if normalized.is_empty() {
        return Ok(DocVerdict::Delete);
    }

    let cap = max_lines_for(req);
    let capped = trim_blank_edges_and_collapse(normalized.into_iter().take(cap).collect());
    if capped.is_empty() {
        return Ok(DocVerdict::Delete);
    }

    Ok(DocVerdict::Text(capped.join("\n").trim().to_string()))
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

    fn doc_req<'a>(kind: &'a str, is_test: bool, preview: &'a [String]) -> DocRequest<'a> {
        DocRequest {
            kind,
            name: "",
            signature: "",
            is_test,
            in_test_file: is_test,
            text: "",
            body_preview: preview,
            body_lines: 4,
        }
    }

    #[test]
    fn parse_doc_reply_bare_delete_is_delete() {
        let r = doc_req("function", false, &[]);
        assert_eq!(parse_doc_reply("DELETE", &r).unwrap(), DocVerdict::Delete);
    }

    #[test]
    fn parse_doc_reply_delete_tolerates_case_and_trailing_punctuation() {
        let r = doc_req("function", false, &[]);
        assert_eq!(parse_doc_reply("delete.", &r).unwrap(), DocVerdict::Delete);
        assert_eq!(parse_doc_reply("Delete:", &r).unwrap(), DocVerdict::Delete);
    }

    #[test]
    fn parse_doc_reply_delete_wins_regardless_of_trailing_lines() {
        let r = doc_req("function", false, &[]);
        assert_eq!(
            parse_doc_reply("DELETE\nThis text should be ignored.", &r).unwrap(),
            DocVerdict::Delete
        );
    }

    #[test]
    fn parse_doc_reply_keep_returns_the_rest_as_text() {
        let r = doc_req("function", false, &[]);
        assert_eq!(
            parse_doc_reply("KEEP\nStates the real contract.", &r).unwrap(),
            DocVerdict::Text("States the real contract.".to_string())
        );
    }

    #[test]
    fn parse_doc_reply_keep_with_empty_rest_is_delete() {
        let r = doc_req("function", false, &[]);
        assert_eq!(parse_doc_reply("KEEP", &r).unwrap(), DocVerdict::Delete);
        assert_eq!(parse_doc_reply("KEEP\n\n", &r).unwrap(), DocVerdict::Delete);
    }

    #[test]
    fn parse_doc_reply_with_neither_keyword_keeps_the_whole_reply() {
        let r = doc_req("function", false, &[]);
        assert_eq!(
            parse_doc_reply("Falls back to 'en' when unset.", &r).unwrap(),
            DocVerdict::Text("Falls back to 'en' when unset.".to_string())
        );
    }

    #[test]
    fn parse_doc_reply_skips_code_fences() {
        let r = doc_req("function", false, &[]);
        assert_eq!(
            parse_doc_reply("```\nKEEP\nThe real contract.\n```", &r).unwrap(),
            DocVerdict::Text("The real contract.".to_string())
        );
    }

    #[test]
    fn parse_doc_reply_strips_a_leading_preamble_line() {
        let r = doc_req("function", false, &[]);
        assert_eq!(
            parse_doc_reply(
                "Here is the rewritten docstring:\nKEEP\nThe real contract.",
                &r
            )
            .unwrap(),
            DocVerdict::Text("The real contract.".to_string())
        );
        assert_eq!(
            parse_doc_reply("Docstring:\nDELETE", &r).unwrap(),
            DocVerdict::Delete
        );
    }

    #[test]
    fn parse_doc_reply_strips_wrapping_triple_quotes_around_the_text() {
        let r = doc_req("function", false, &[]);
        assert_eq!(
            parse_doc_reply("KEEP\n\"\"\"\nThe real contract.\n\"\"\"", &r).unwrap(),
            DocVerdict::Text("The real contract.".to_string())
        );
    }

    #[test]
    fn parse_doc_reply_empty_is_err() {
        let r = doc_req("function", false, &[]);
        assert!(parse_doc_reply("", &r).is_err());
        assert!(parse_doc_reply("   \n  \n", &r).is_err());
    }

    #[test]
    fn parse_doc_reply_caps_test_function_at_two_lines() {
        let r = doc_req("function", true, &[]);
        let reply = "KEEP\nline one\nline two\nline three\nline four";
        assert_eq!(
            parse_doc_reply(reply, &r).unwrap(),
            DocVerdict::Text("line one\nline two".to_string())
        );
    }

    #[test]
    fn parse_doc_reply_caps_test_module_at_five_lines() {
        let r = doc_req("module", true, &[]);
        let reply = "KEEP\n1\n2\n3\n4\n5\n6\n7";
        assert_eq!(
            parse_doc_reply(reply, &r).unwrap(),
            DocVerdict::Text("1\n2\n3\n4\n5".to_string())
        );
    }

    #[test]
    fn parse_doc_reply_caps_non_test_at_fifteen_lines() {
        let r = doc_req("class", false, &[]);
        let body: Vec<String> = (1..=20).map(|n| n.to_string()).collect();
        let reply = format!("KEEP\n{}", body.join("\n"));
        let expected = (1..=15)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            parse_doc_reply(&reply, &r).unwrap(),
            DocVerdict::Text(expected)
        );
    }

    #[test]
    fn parse_doc_reply_collapses_blank_runs_and_trims_edges() {
        let r = doc_req("class", false, &[]);
        let reply = "KEEP\n\n\nFirst.\n\n\n\nSecond.\n\n\n";
        assert_eq!(
            parse_doc_reply(reply, &r).unwrap(),
            DocVerdict::Text("First.\n\nSecond.".to_string())
        );
    }

    #[test]
    fn doc_demos_round_trip_through_parse_doc_reply() {
        for &(
            kind,
            name,
            signature,
            is_test,
            in_test_file,
            text,
            body_preview,
            body_lines,
            reply,
        ) in DOC_DEMOS
        {
            let preview: Vec<String> = body_preview.iter().map(|s| s.to_string()).collect();
            let req = DocRequest {
                kind,
                name,
                signature,
                is_test,
                in_test_file,
                text,
                body_preview: &preview,
                body_lines,
            };
            let verdict = parse_doc_reply(reply, &req)
                .unwrap_or_else(|e| panic!("demo {name:?} failed to parse: {e}"));
            let expected = match reply.strip_prefix("KEEP\n") {
                Some(rest) => DocVerdict::Text(rest.to_string()),
                None => DocVerdict::Delete,
            };
            assert_eq!(verdict, expected, "demo {name:?} did not round-trip");
        }
    }

    #[test]
    fn doc_user_message_mentions_kind_and_test_flag() {
        let req = DocRequest {
            kind: "function",
            name: "test_foo",
            signature: "def test_foo():",
            is_test: true,
            in_test_file: true,
            text: "Tests foo.",
            body_preview: &[],
            body_lines: 2,
        };
        let msg = doc_user_message(&req);
        assert!(msg.contains("Kind: function"), "message was: {msg}");
        assert!(msg.contains("Test: yes"), "message was: {msg}");
    }
}
