//! Lightweight NLP over comment text: strip delimiters, drop separators / commented-out code,
//! unwrap paragraphs, split sentences, measure density, and produce an extractive one-liner.
use crate::types::{CommentBlock, Language};

#[derive(Debug, Clone)]
pub struct ProseAnalysis {
    /// Cleaned prose lines (delimiters and leaders stripped; separators and code-like lines dropped).
    pub lines: Vec<String>,
    /// Prose lines joined into flowing text.
    pub text: String,
    pub sentences: Vec<String>,
    pub word_count: usize,
    /// word_count / lines.len() (0.0 if no lines).
    pub words_per_line: f64,
    /// Most of the raw lines look like commented-out code rather than English.
    pub code_like: bool,
    /// Best single-sentence extractive summary, trimmed to roughly `max_words`.
    pub extractive: String,
}

/// Strip comment delimiters/leaders from each raw line of the block: `#`, `//`, `/*`, `*/`,
/// leading `*` in block comments, and surrounding whitespace.
pub fn clean_lines(block: &CommentBlock, lang: Language) -> Vec<String> {
    let _ = (block, lang);
    todo!("prose::clean_lines")
}

pub fn analyze(block: &CommentBlock, lang: Language, max_words: usize) -> ProseAnalysis {
    let _ = (block, lang, max_words);
    todo!("prose::analyze")
}
