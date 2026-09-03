//! tree-sitter based comment extraction. Comments are `comment` nodes in all four grammars;
//! strings, template literals, regex literals, JSX text and Python docstrings are never comments,
//! so the grammar does the hard work for us.
use crate::types::{Comment, CommentBlock, CommentKind, Language};
use anyhow::{Result, anyhow};

fn ts_language(lang: Language) -> tree_sitter::Language {
    match lang {
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
    }
}

/// Byte offset of the start of the line containing `pos`.
fn line_start(src: &str, pos: usize) -> usize {
    src.as_bytes()[..pos]
        .iter()
        .rposition(|&b| b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(0)
}

/// Byte offset of the end of the line containing `pos` (the newline itself, or EOF).
fn line_end(src: &str, pos: usize) -> usize {
    src.as_bytes()[pos..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|i| pos + i)
        .unwrap_or(src.len())
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
    let end = node.end_byte();
    let text = src[start..end].to_string();
    let kind = if node.kind() == "html_comment" || text.starts_with("/*") {
        CommentKind::Block
    } else {
        CommentKind::Line
    };
    let start_line = node.start_position().row;
    let end_line = node.end_position().row;
    let ls = line_start(src, start);
    let own_line = src[ls..start].chars().all(|c| c.is_whitespace());
    let le = line_end(src, end);
    let code_after = !src[end..le].trim().is_empty();
    Comment {
        start,
        end,
        kind,
        text,
        start_line,
        end_line,
        own_line,
        code_after,
    }
}

/// All comment tokens in `src`, in source order.
pub fn extract_comments(src: &str, lang: Language) -> Result<Vec<Comment>> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&ts_language(lang))?;
    let tree = parser
        .parse(src, None)
        .ok_or_else(|| anyhow!("tree-sitter failed to parse"))?;
    // tree-sitter's error recovery can mislex strings/regexes when it can't make sense of the
    // input; never touch a file we didn't parse cleanly.
    if tree.root_node().has_error() {
        return Err(anyhow!("parse errors"));
    }

    let mut comments = Vec::new();
    let mut cursor = tree.root_node().walk();
    loop {
        let node = cursor.node();
        if node.kind() == "comment" || node.kind() == "html_comment" {
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
/// adjacent lines with identical indentation merge; everything else is its own block.
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
}
