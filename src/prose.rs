//! Lightweight NLP over comment text: strip delimiters, drop separators / commented-out code,
//! unwrap paragraphs, measure density.
use crate::types::{CommentBlock, CommentKind, Language};
use regex::Regex;
use std::sync::LazyLock;

#[derive(Debug, Clone)]
pub struct ProseAnalysis {
    /// Cleaned prose lines (delimiters and leaders stripped; separators and code-like lines dropped).
    pub lines: Vec<String>,
    /// Prose lines joined into flowing text.
    pub text: String,
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
                    Language::Python | Language::Yaml => {
                        c.text.strip_prefix('#').unwrap_or(&c.text)
                    }
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

/// A commented-out YAML key (`foo:` / `foo: bar`, no space before the colon) or list item (`- `).
static YAML_KEY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_./-]+:(\s|$)").unwrap());

/// Whether `rest` (the text after a YAML `key:` or `- ` prefix) reads as a short scalar/list
/// value rather than a full English clause: empty, or at most 6 words with no terminal sentence
/// punctuation. Ordinary prose that happens to start with a label ("Warning: this value must be
/// updated...") or a hand-written bullet ("- first check the connection...") fails this, so it
/// is not mistaken for commented-out YAML.
fn looks_like_short_yaml_value(rest: &str) -> bool {
    if rest.is_empty() {
        return true;
    }
    if rest.ends_with(['.', '!', '?']) {
        return false;
    }
    rest.split_whitespace().count() <= 6
}

/// Small heuristic: does this line look like code rather than English prose?
fn is_code_like(line: &str, lang: Language) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    // YAML-only: commented-out config (a list item or a `key:`/`key: value` line) reads as code,
    // even though it has none of the JS/Python code punctuation checked below -- but only when
    // the value/item itself is short, so a "Label: sentence." or "- bullet point." prose line
    // isn't misclassified just because it happens to share the same prefix shape.
    if lang == Language::Yaml {
        if let Some(m) = YAML_KEY_RE.find(t) {
            if looks_like_short_yaml_value(t[m.end()..].trim()) {
                return true;
            }
        } else if let Some(rest) = t.strip_prefix("- ")
            && looks_like_short_yaml_value(rest.trim())
        {
            return true;
        }
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
        if is_code_like(&line, lang) {
            code_line_count += 1;
        } else {
            prose_lines.push(line);
        }
    }
    let code_like = code_line_count > prose_lines.len();

    let text = prose_lines.join(" ");
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

    #[test]
    fn yaml_commented_out_config_is_code_like() {
        let block = line_block(&[
            "# name: myapp",
            "# image: nginx:latest",
            "# ports:",
            "#   - 8080:80",
        ]);
        assert!(analyze(&block, Language::Yaml).code_like);
    }

    #[test]
    fn yaml_english_prose_is_not_code_like() {
        let block = line_block(&[
            "# This service handles incoming requests and forwards them to the",
            "# appropriate backend based on the path prefix in the URL.",
        ]);
        assert!(!analyze(&block, Language::Yaml).code_like);
    }

    #[test]
    fn yaml_label_prose_is_not_code_like() {
        // A "Label: sentence." line shares its prefix shape with a commented-out `key: value`,
        // but the long, punctuated remainder marks it as English prose, not YAML.
        let block = line_block(&[
            "# Warning: this value must be updated whenever the schema changes upstream.",
            "# The downstream consumer reads this key directly at startup and caches it.",
            "# If it drifts from the real schema, requests will fail in a confusing way.",
            "# Always run the validation script after editing this file by hand please.",
        ]);
        let a = analyze(&block, Language::Yaml);
        assert!(!a.code_like);
        assert_eq!(a.lines.len(), 4, "no line should be dropped as code-like");
    }

    #[test]
    fn yaml_bullet_prose_is_not_code_like() {
        let block = line_block(&[
            "# - first check the connection before doing anything else in this handler",
            "# - then verify the credentials actually match the ones on file for the user",
        ]);
        assert!(!analyze(&block, Language::Yaml).code_like);
    }
}
