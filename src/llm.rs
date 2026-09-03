//! OpenAI-compatible chat completions client used to abstractively summarize a comment.
use crate::types::Config;
use anyhow::Result;

pub struct LlmClient {
    // reqwest::Client, endpoint (".../v1"), model, api_key, tokio Semaphore(llm_concurrency)
}

impl LlmClient {
    pub fn new(cfg: &Config) -> LlmClient {
        let _ = cfg;
        todo!("llm::LlmClient::new")
    }

    /// Summarize `prose` to a single line of at most ~`max_words` words. `context` is the first
    /// non-blank code line following the comment (may be empty) and is passed to the model as a hint.
    /// Returns Err on transport failure or if the model output is unusable (empty / multi-line after
    /// cleanup). Caller falls back to the extractive summary.
    pub async fn summarize(&self, prose: &str, context: &str, max_words: usize) -> Result<String> {
        let _ = (prose, context, max_words);
        todo!("llm::LlmClient::summarize")
    }
}
