//! Lightweight NLP over comment text: strip delimiters, drop separators / commented-out code,
//! unwrap paragraphs, split sentences, measure density, and produce an extractive one-liner.
use crate::types::{CommentBlock, CommentKind, Language};
use rust_stemmers::{Algorithm, Stemmer};
use std::collections::{HashMap, HashSet};
use unicode_segmentation::UnicodeSegmentation;

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

const STOPWORDS: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "if", "then", "else", "when", "at", "by", "for", "with",
    "about", "against", "between", "into", "through", "during", "before", "after", "above",
    "below", "to", "from", "up", "down", "in", "out", "on", "off", "over", "under", "again",
    "further", "once", "here", "there", "all", "any", "both", "each", "few", "more", "most",
    "other", "some", "such", "no", "nor", "not", "only", "own", "same", "so", "than", "too",
    "very", "can", "will", "just", "should", "now", "is", "are", "was", "were", "be", "been",
    "being", "have", "has", "had", "do", "does", "did", "this", "that", "these", "those", "of",
    "it", "its", "as", "which", "who", "what",
];

/// Strip comment delimiters/leaders from each raw line of the block: `#`, `//`, `/*`, `*/`,
/// leading `*` in block comments, and surrounding whitespace.
pub fn clean_lines(block: &CommentBlock, lang: Language) -> Vec<String> {
    let mut out = Vec::new();
    for c in &block.comments {
        match c.kind {
            CommentKind::Line => {
                let rest = match lang {
                    Language::Python => c.text.strip_prefix('#').unwrap_or(&c.text),
                    _ => c.text.strip_prefix("//").unwrap_or(&c.text),
                };
                let rest = rest.trim_start_matches(['/', '!']);
                out.push(rest.trim().to_string());
            }
            CommentKind::Block => {
                let s = c.text.trim();
                let s = s
                    .strip_prefix("/**")
                    .or_else(|| s.strip_prefix("/*"))
                    .unwrap_or(s);
                let s = s.strip_suffix("*/").unwrap_or(s);
                for line in s.lines() {
                    let line = line.trim();
                    let line = line.strip_prefix('*').unwrap_or(line);
                    out.push(line.trim().to_string());
                }
            }
        }
    }
    out
}

/// >= 3 chars, entirely made of separator punctuation (e.g. `----`, `====`, `****`).
fn is_separator(line: &str) -> bool {
    let t = line.trim();
    if t.chars().count() < 3 {
        return false;
    }
    t.chars().all(|c| "-=*#~_+/|.".contains(c))
}

/// Small heuristic: does this line look like code rather than English prose?
fn is_code_like(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    if t.ends_with(';') || t.ends_with('{') || t.ends_with('}') {
        return true;
    }
    const STARTERS: &[&str] = &[
        "def ",
        "class ",
        "import ",
        "from ",
        "return ",
        "if ",
        "for ",
        "while ",
        "const ",
        "let ",
        "var ",
        "function ",
        "export ",
        "}",
    ];
    if STARTERS.iter().any(|s| t.starts_with(s)) {
        return true;
    }
    if t.contains("=>") || t.contains("->") || t.contains(");") {
        return true;
    }
    // bare assignment like `foo = bar(`
    if let Some(idx) = t.find(" = ")
        && t[idx + 3..].contains('(')
    {
        return true;
    }
    let total = t.chars().count();
    let punct = t
        .chars()
        .filter(|c| !c.is_alphanumeric() && !c.is_whitespace())
        .count();
    total > 0 && (punct as f64 / total as f64) > 0.35
}

fn truncate_words(s: &str, max_words: usize) -> String {
    let words: Vec<&str> = s.split_whitespace().collect();
    if words.len() <= max_words {
        words.join(" ")
    } else {
        words[..max_words].join(" ")
    }
}

fn tokenize(sentence: &str, stemmer: &Stemmer) -> Vec<String> {
    sentence
        .split(|c: char| !c.is_alphabetic())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
        .filter(|w| !STOPWORDS.contains(&w.as_str()))
        .map(|w| stemmer.stem(&w).into_owned())
        .collect()
}

fn extractive_summary(sentences: &[String], prose_lines: &[String], max_words: usize) -> String {
    if sentences.is_empty() {
        return match prose_lines.first() {
            Some(l) => truncate_words(l, max_words),
            None => String::new(),
        };
    }
    if sentences[0].split_whitespace().count() <= max_words {
        return sentences[0].clone();
    }

    let stemmer = Stemmer::create(Algorithm::English);
    let tokenized: Vec<Vec<String>> = sentences.iter().map(|s| tokenize(s, &stemmer)).collect();

    let mut df: HashMap<&str, usize> = HashMap::new();
    for toks in &tokenized {
        let seen: HashSet<&str> = toks.iter().map(|t| t.as_str()).collect();
        for t in seen {
            *df.entry(t).or_insert(0) += 1;
        }
    }

    let mut best_idx = 0usize;
    let mut best_score = f64::MIN;
    for (i, toks) in tokenized.iter().enumerate() {
        if toks.is_empty() {
            continue;
        }
        let sum: usize = toks.iter().map(|t| df[t.as_str()]).sum();
        let score = sum as f64 / (toks.len() as f64).sqrt();
        if score > best_score {
            best_score = score;
            best_idx = i;
        }
    }
    truncate_words(&sentences[best_idx], max_words)
}

pub fn analyze(block: &CommentBlock, lang: Language, max_words: usize) -> ProseAnalysis {
    let raw = clean_lines(block, lang);

    let mut code_line_count = 0usize;
    let mut prose_lines: Vec<String> = Vec::new();
    for line in raw {
        if line.trim().is_empty() || is_separator(&line) {
            continue;
        }
        if is_code_like(&line) {
            code_line_count += 1;
        } else {
            prose_lines.push(line);
        }
    }
    let code_like = code_line_count > prose_lines.len();

    let text = prose_lines.join(" ");
    let sentences: Vec<String> = text
        .unicode_sentences()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let word_count: usize = prose_lines
        .iter()
        .map(|l| l.split_whitespace().count())
        .sum();
    let words_per_line = if prose_lines.is_empty() {
        0.0
    } else {
        word_count as f64 / prose_lines.len() as f64
    };
    let extractive = extractive_summary(&sentences, &prose_lines, max_words);

    ProseAnalysis {
        lines: prose_lines,
        text,
        sentences,
        word_count,
        words_per_line,
        code_like,
        extractive,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Comment;

    fn line_block(lines: &[&str]) -> CommentBlock {
        let comments: Vec<Comment> = lines
            .iter()
            .map(|l| Comment {
                start: 0,
                end: 0,
                kind: CommentKind::Line,
                text: l.to_string(),
                start_line: 0,
                end_line: 0,
                own_line: true,
                code_after: false,
            })
            .collect();
        CommentBlock {
            comments,
            start: 0,
            end: 0,
            start_line: 0,
            end_line: 0,
            indent: String::new(),
            own_line: true,
            code_after: false,
            kind: CommentKind::Line,
        }
    }

    #[test]
    fn cleans_line_comments() {
        let block = line_block(&["// hello world", "/// doc line"]);
        let lines = clean_lines(&block, Language::JavaScript);
        assert_eq!(
            lines,
            vec!["hello world".to_string(), "doc line".to_string()]
        );
    }

    #[test]
    fn short_first_sentence_used_verbatim() {
        let block =
            line_block(&["This explains the thing. And here is more detail to pad it out."]);
        let a = analyze(&block, Language::JavaScript, 10);
        assert_eq!(a.extractive, "This explains the thing.");
    }
}
