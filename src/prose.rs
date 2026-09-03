//! Lightweight NLP over comment text: strip delimiters, drop separators / commented-out code,
//! unwrap paragraphs, split sentences, measure density.
use crate::types::{CommentBlock, CommentKind, Language};
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
}

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

pub fn analyze(block: &CommentBlock, lang: Language) -> ProseAnalysis {
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

    ProseAnalysis {
        lines: prose_lines,
        text,
        sentences,
        word_count,
        words_per_line,
        code_like,
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
}
