//! OpenAI-compatible chat completions client used to abstractively summarize a comment.
use crate::types::Config;
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;
use tokio::sync::Semaphore;

pub struct LlmClient {
    client: reqwest::Client,
    endpoint: String,
    model: String,
    api_key: Option<String>,
    semaphore: Semaphore,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
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
        }
    }

    /// Summarize `prose` to a single line of at most ~`max_words` words. `context` is the first
    /// non-blank code line following the comment (may be empty) and is passed to the model as a hint.
    /// Returns Err on transport failure or if the model output is unusable (empty / multi-line after
    /// cleanup). Caller falls back to the extractive summary.
    pub async fn summarize(&self, prose: &str, context: &str, max_words: usize) -> Result<String> {
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|e| anyhow!("semaphore closed: {e}"))?;

        let system = format!(
            "You condense multi-line source code comments into a single line. Reply with exactly \
             one line of plain text, no more than {max_words} words, no quotes, no markdown, no \
             preamble. Preserve identifiers and technical terms verbatim."
        );
        let mut user = prose.to_string();
        if !context.is_empty() {
            user.push_str(&format!("\n\nThe comment precedes this code: {context}"));
        }

        let body = json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
            "max_tokens": 64,
            "temperature": 0,
        });

        let url = format!("{}/chat/completions", self.endpoint);

        let raw = match self.post(&url, &body).await {
            Ok(r) => r,
            Err(_) => self.post(&url, &body).await?, // one retry on transport error
        };

        let cleaned = clean_reply(&raw);
        if cleaned.is_empty() {
            bail!("empty reply from LLM after cleanup");
        }
        let word_count = cleaned.split_whitespace().count();
        if word_count > 3 * max_words {
            bail!(
                "LLM reply too long: {word_count} words (limit {})",
                3 * max_words
            );
        }
        Ok(cleaned)
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

/// Take the first non-empty line, trim, strip wrapping quotes/backticks, strip a leading
/// "#", "//" or "*" run, collapse internal whitespace, strip trailing "*/".
fn clean_reply(raw: &str) -> String {
    let Some(mut line) = raw.lines().map(str::trim).find(|l| !l.is_empty()) else {
        return String::new();
    };

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
    line.split_whitespace().collect::<Vec<_>>().join(" ")
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
}
