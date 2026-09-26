//! tree-sitter based comment extraction. Comments are `comment` nodes in the JS/TS, Python and
//! YAML grammars and `line_comment` / `block_comment` nodes in Rust's; strings, template literals,
//! regex literals, JSX text, Python docstrings, YAML block/quoted scalars and Rust raw strings are
//! never comments, so the grammar does the hard work for us.
use crate::rewrite::{line_end, line_start};
use crate::types::{Comment, CommentBlock, CommentKind, Language};
use anyhow::{Result, anyhow};
use regex::Regex;
use std::borrow::Cow;
use std::sync::LazyLock;

fn ts_language(lang: Language) -> tree_sitter::Language {
    match lang {
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Language::Yaml => tree_sitter_yaml::LANGUAGE.into(),
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
    }
}

/// The whitespace run at the start of the line containing `pos`, up to the first non-whitespace
/// character (which may be the comment itself, or code preceding a trailing comment).
fn leading_whitespace(src: &str, pos: usize) -> String {
    let ls = line_start(src, pos);
    let line = &src[ls..line_end(src, ls)];
    let non_ws = line
        .char_indices()
        .find(|(_, c)| !c.is_whitespace())
        .map(|(i, _)| i)
        .unwrap_or(line.len());
    line[..non_ws].to_string()
}

fn make_comment(src: &str, node: &tree_sitter::Node) -> Comment {
    let start = node.start_byte();
    let mut end = node.end_byte();
    let kind = if node.kind() == "html_comment" || src[start..end].starts_with("/*") {
        CommentKind::Block
    } else {
        CommentKind::Line
    };
    // A line comment ends before its line terminator, but the Python and Rust grammars include a
    // CRLF line's `\r` (Rust a doc comment's `\n` too), which a trailing-comment delete would eat.
    if kind == CommentKind::Line {
        end = start + src[start..end].trim_end_matches(['\r', '\n']).len();
    }
    let text = src[start..end].to_string();
    let start_line = node.start_position().row;
    let end_line = start_line + text.matches('\n').count();
    let ls = line_start(src, start);
    let own_line = src[ls..start].chars().all(|c| c.is_whitespace());
    let le = line_end(src, end);
    let code_after = !src[end..le].trim().is_empty();
    let mut cursor = node.walk();
    let doc = node.children(&mut cursor).any(|c| {
        matches!(
            c.kind(),
            "outer_doc_comment_marker" | "inner_doc_comment_marker"
        )
    });
    Comment {
        start,
        end,
        kind,
        text,
        start_line,
        end_line,
        own_line,
        code_after,
        doc,
    }
}

/// `import('m')` in a type: optionally preceded by `typeof`, or opening a type-argument list.
static IMPORT_TYPE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(typeof\s+|<\s*)import\(\s*(?:'[^'\n]*'|"[^"\n]*")\s*\)"#).unwrap()
});

/// The text tree-sitter actually sees: `src` with a couple of byte-length-preserving edits that
/// work around grammar limitations, so node offsets still index `src` directly.
///
/// - A raw NUL byte (legal inside a JS/TS string) is a hard lexer error; parse it as a space.
/// - `f<import('m')>()` and `f<typeof import('m')>()` (the Vitest `importOriginal` idiom) fail in
///   tree-sitter-typescript 0.23 (tree-sitter/tree-sitter-typescript#367): at `<` the parser
///   commits to a comparison and never reaches the type-argument reading. `typeof III…` of the
///   same length parses as a type query in every position, including with `.Bar` / `['k']`
///   suffixes, and in expression position it is still a plain unary expression.
fn parse_input(src: &str, lang: Language) -> Cow<'_, str> {
    let mut text = Cow::Borrowed(src);
    if text.contains('\0') {
        text = Cow::Owned(text.replace('\0', " "));
    }
    if matches!(lang, Language::TypeScript | Language::Tsx)
        && let Cow::Owned(s) = IMPORT_TYPE_RE.replace_all(&text, |c: &regex::Captures| {
            let prefix = &c[1];
            let rest = c[0].len() - prefix.len();
            if prefix.starts_with('<') {
                format!("{prefix}typeof {}", "I".repeat(rest - 7))
            } else {
                format!("{prefix}{}", "I".repeat(rest))
            }
        })
    {
        text = Cow::Owned(s);
    }
    text
}

fn parse_tree(src: &str, lang: Language) -> Result<tree_sitter::Tree> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&ts_language(lang))?;
    parser
        .parse(parse_input(src, lang).as_bytes(), None)
        .ok_or_else(|| anyhow!("tree-sitter failed to parse"))
}

/// All comment tokens in `src`, in source order.
pub fn extract_comments(src: &str, lang: Language) -> Result<Vec<Comment>> {
    let tree = parse_tree(src, lang)?;
    // tree-sitter's error recovery can mislex strings/regexes when it can't make sense of the
    // input; never touch a file we didn't parse cleanly.
    if tree.root_node().has_error() {
        return Err(anyhow!("parse errors"));
    }

    let mut comments = Vec::new();
    let mut cursor = tree.root_node().walk();
    loop {
        let node = cursor.node();
        if matches!(
            node.kind(),
            "comment" | "html_comment" | "line_comment" | "block_comment"
        ) {
            comments.push(make_comment(src, &node));
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return Ok(comments);
            }
        }
    }
}

/// Redacted description of every parse error in `src`, or None if the file parses cleanly.
/// Only node kinds, positions and the shape of the offending lines (letters -> `a`, digits -> `0`,
/// control chars -> `^`, non-ASCII -> `~`) are included, so the output is safe to paste into a
/// bug report.
pub fn diagnose(src: &str, lang: Language) -> Result<Option<String>> {
    const MAX_ERRORS: usize = 20;
    const MAX_LINES: usize = 5;
    const MAX_COLS: usize = 200;
    let tree = parse_tree(src, lang)?;
    if !tree.root_node().has_error() {
        return Ok(None);
    }
    let mut errors = Vec::new();
    collect_errors(tree.root_node(), &mut errors);
    let lines: Vec<&str> = src.lines().collect();
    let mut out = format!(
        "{lang:?}, {} lines, {} parse errors\n",
        lines.len(),
        errors.len()
    );
    for node in errors.iter().take(MAX_ERRORS) {
        let (s, e) = (node.start_position(), node.end_position());
        let mut chain = Vec::new();
        let mut p = node.parent();
        while let Some(n) = p {
            chain.push(n.kind());
            p = n.parent();
        }
        chain.reverse();
        out.push_str(&format!(
            "{}:{}-{}:{} in {}\n  {}\n",
            s.row + 1,
            s.column + 1,
            e.row + 1,
            e.column + 1,
            chain.join(" > "),
            node.to_sexp()
        ));
        for line in lines
            .iter()
            .skip(s.row)
            .take((e.row - s.row + 1).min(MAX_LINES))
        {
            let masked: String = line
                .chars()
                .take(MAX_COLS)
                .map(|c| match c {
                    c if c.is_ascii_alphabetic() => 'a',
                    c if c.is_ascii_digit() => '0',
                    c if c.is_ascii_control() && c != '\t' => '^',
                    c if c.is_ascii() => c,
                    _ => '~',
                })
                .collect();
            out.push_str(&format!("  | {masked}\n"));
        }
    }
    if errors.len() > MAX_ERRORS {
        out.push_str(&format!("... {} more\n", errors.len() - MAX_ERRORS));
    }
    Ok(Some(out))
}

/// Outermost ERROR / MISSING nodes under `node`, in source order.
fn collect_errors<'a>(node: tree_sitter::Node<'a>, out: &mut Vec<tree_sitter::Node<'a>>) {
    if node.is_error() || node.is_missing() {
        out.push(node);
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.has_error() {
            collect_errors(child, out);
        }
    }
}

fn single_block(src: &str, c: Comment) -> CommentBlock {
    let indent = leading_whitespace(src, c.start);
    CommentBlock {
        start: c.start,
        end: c.end,
        start_line: c.start_line,
        end_line: c.end_line,
        indent,
        own_line: c.own_line,
        code_after: c.code_after,
        kind: c.kind,
        comments: vec![c],
    }
}

/// Groups comments into blocks (see `CommentBlock` doc). Consecutive own-line Line comments on
/// adjacent lines with identical indentation merge, unless one is a Rust doc comment and the
/// other is not; everything else is its own block.
pub fn group_blocks(src: &str, comments: Vec<Comment>) -> Vec<CommentBlock> {
    let mut blocks: Vec<CommentBlock> = Vec::new();
    for c in comments {
        let can_merge = c.kind == CommentKind::Line
            && c.own_line
            && blocks.last().is_some_and(|last| {
                last.own_line
                    && !last.code_after
                    && c.start_line == last.end_line + 1
                    && leading_whitespace(src, c.start) == last.indent
                    && last.comments.last().is_some_and(|prev| prev.doc == c.doc)
            });
        if can_merge {
            let last = blocks.last_mut().unwrap();
            last.end = c.end;
            last.end_line = c.end_line;
            last.code_after = c.code_after;
            last.comments.push(c);
        } else {
            blocks.push(single_block(src, c));
        }
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_string_and_docstring_are_not_comments() {
        let src = r##"def f():
    """A docstring, not a comment."""
    s = "# not a comment"
    # real comment one
    # real comment two
    return s  # trailing
"##;
        let comments = extract_comments(src, Language::Python).unwrap();
        let texts: Vec<&str> = comments.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["# real comment one", "# real comment two", "# trailing"]
        );

        let blocks = group_blocks(src, comments);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].comments.len(), 2);
        assert_eq!(blocks[0].indent, "    ");
        assert!(blocks[0].own_line);
        assert_eq!(blocks[1].comments.len(), 1);
        assert!(!blocks[1].own_line);
        assert!(!blocks[1].code_after);
    }

    #[test]
    fn trailing_comment_does_not_merge_with_following_own_line_comment() {
        // A trailing comment (own_line=false) followed by an own-line comment at the same
        // indent must NOT merge into one block, even though their line-level indentation
        // (leading_whitespace of the whole line) happens to coincide.
        let src = "def f():\n    x = 1  # trailing comment\n    # unrelated standalone comment\n    y = 2\n";
        let comments = extract_comments(src, Language::Python).unwrap();
        let blocks = group_blocks(src, comments);
        assert_eq!(blocks.len(), 2);
        assert!(!blocks[0].own_line);
        assert_eq!(blocks[0].comments.len(), 1);
        assert!(blocks[1].own_line);
        assert_eq!(blocks[1].comments.len(), 1);
    }

    #[test]
    fn diagnose_redacts_source() {
        assert!(diagnose("x = 1\n", Language::Python).unwrap().is_none());
        let src = "def f(:\n    secret = \"hunter2\"\n";
        let report = diagnose(src, Language::Python).unwrap().unwrap();
        assert!(
            report.contains("ERROR") || report.contains("MISSING"),
            "{report}"
        );
        assert!(
            !report.contains("secret") && !report.contains("hunter2"),
            "{report}"
        );
    }

    #[test]
    fn grammar_workarounds_keep_offsets() {
        // tree-sitter/tree-sitter-typescript#367 plus a raw NUL inside a string; both must parse,
        // and comment text must come from the original source.
        let src = "vi.mock('./m', async (importOriginal) => {\n  // keep: typeof import('x')\n  const actual = await importOriginal<typeof import('./m')>();\n  const bar = f<import(\"./m\").Bar>();\n  return [actual, bar].join(\"\0\"); // trailing\n});\n";
        assert!(diagnose(src, Language::TypeScript).unwrap().is_none());
        let comments = extract_comments(src, Language::TypeScript).unwrap();
        let texts: Vec<&str> = comments.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["// keep: typeof import('x')", "// trailing"]);
        assert_eq!(&src[comments[1].start..comments[1].end], "// trailing");

        let report = diagnose("const x = \"\0\";\nlet = ;\n", Language::TypeScript)
            .unwrap()
            .unwrap();
        assert!(!report.contains('\0'), "{report}");
    }

    #[test]
    fn tsx_non_comments_are_ignored() {
        let src = r#"const t = `template with // text and $ not a comment`;
const r = /\/\//;
/**
 * JSDoc block.
 */
function App() {
    return <div>// text</div>; // trailing
}
"#;
        let comments = extract_comments(src, Language::Tsx).unwrap();
        let texts: Vec<&str> = comments.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["/**\n * JSDoc block.\n */", "// trailing"]);

        let blocks = group_blocks(src, comments);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].kind, CommentKind::Block);
        assert_eq!(blocks[0].comments.len(), 1);
        assert_eq!(blocks[1].kind, CommentKind::Line);
        assert!(!blocks[1].own_line);
        assert!(!blocks[1].code_after);
    }

    #[test]
    fn rust_strings_are_not_comments_and_doc_comments_end_on_their_line() {
        let src = r##"//! Crate docs.
/// Item docs.
// regular
fn f<'a>(s: &'a str) -> &'a str {
    let r = r#"// not "a" comment"#; // trailing
    let c = '/'; /* nested /* inner */ outer */
    s
}
"##;
        assert!(diagnose(src, Language::Rust).unwrap().is_none());
        let comments = extract_comments(src, Language::Rust).unwrap();
        let texts: Vec<(&str, bool)> = comments.iter().map(|c| (c.text.as_str(), c.doc)).collect();
        assert_eq!(
            texts,
            vec![
                ("//! Crate docs.", true),
                ("/// Item docs.", true),
                ("// regular", false),
                ("// trailing", false),
                ("/* nested /* inner */ outer */", false),
            ]
        );
        for c in &comments {
            assert_eq!(&src[c.start..c.end], c.text, "offsets must index src");
            assert_eq!(c.start_line, c.end_line);
            assert!(!c.code_after || c.text.starts_with("/*"));
        }

        // A doc comment never merges with a plain comment on the next line.
        let blocks = group_blocks(src, comments);
        let sizes: Vec<usize> = blocks.iter().map(|b| b.comments.len()).collect();
        assert_eq!(sizes, vec![2, 1, 1, 1]);
    }

    #[test]
    fn deleting_a_trailing_comment_keeps_crlf() {
        // tree-sitter-python and tree-sitter-rust put a CRLF line's `\r` inside the comment.
        for (src, lang, want) in [
            (
                "x = 1  # c\r\ny = 2\r\n",
                Language::Python,
                "x = 1\r\ny = 2\r\n",
            ),
            (
                "let x = 1; // c\r\n/// d\r\nfn f() {}\r\n",
                Language::Rust,
                "let x = 1;\r\n/// d\r\nfn f() {}\r\n",
            ),
        ] {
            let blocks = group_blocks(src, extract_comments(src, lang).unwrap());
            let edit = crate::rewrite::delete_edit(src, &blocks[0]);
            assert_eq!(crate::rewrite::apply(src, vec![edit]), want);
        }
    }

    #[test]
    fn yaml_scalars_are_not_comments() {
        let src = r##"# top comment line one
# top comment line two
key: value  # trailing comment
block: |
  echo hi
  # not a comment
quoted: "# not a comment"
plain: a#b
items:
  - one
  # comment inside sequence
  - two
---
doc2: true
"##;
        assert!(diagnose(src, Language::Yaml).unwrap().is_none());
        let comments = extract_comments(src, Language::Yaml).unwrap();
        let texts: Vec<&str> = comments.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "# top comment line one",
                "# top comment line two",
                "# trailing comment",
                "# comment inside sequence",
            ]
        );
        for c in &comments {
            assert_eq!(&src[c.start..c.end], c.text, "offsets must index src");
        }

        let blocks = group_blocks(src, comments);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].comments.len(), 2);
        assert!(blocks[0].own_line);
        assert_eq!(blocks[1].comments.len(), 1);
        assert!(!blocks[1].own_line);
        assert_eq!(blocks[2].comments.len(), 1);
        assert!(blocks[2].own_line);
    }
}
