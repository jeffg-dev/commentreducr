//! Turn actions into byte-range edits and apply them. This is the only code that touches file
//! contents, so correctness here is what keeps the target codebase intact.
use crate::types::{CommentBlock, Language};

#[derive(Debug, Clone)]
pub struct Edit {
    pub start: usize,
    pub end: usize,
    pub replacement: String,
}

/// Line terminator used by `src`: "\r\n" if present anywhere, else "\n".
fn eol_of(src: &str) -> &'static str {
    if src.contains("\r\n") { "\r\n" } else { "\n" }
}

/// Byte offset of the start of the line containing `pos`. `pos` need not be a char boundary.
fn line_start(src: &str, pos: usize) -> usize {
    src.as_bytes()[..pos]
        .iter()
        .rposition(|&b| b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(0)
}

/// Byte offset just past the terminator of the line containing `pos` (i.e. the start of the next
/// line), or the end of the string if `pos`'s line has no terminator (EOF).
/// `pos` need not be a char boundary.
fn line_end_incl_terminator(src: &str, pos: usize) -> usize {
    match src.as_bytes()[pos..].iter().position(|&b| b == b'\n') {
        Some(i) => pos + i + 1,
        None => src.len(),
    }
}

/// Edit that removes the block.
/// - own-line block with no code after: remove the whole lines including their line terminators.
/// - trailing comment (code before it): remove from the end of the code (trim trailing spaces) to
///   the end of the comment, keeping the line terminator.
/// - inline block comment with code after: remove just the span plus one adjacent space if both
///   neighbours are spaces.
pub fn delete_edit(src: &str, block: &CommentBlock) -> Edit {
    if block.own_line && !block.code_after {
        let mut start = block.start - block.indent.len();
        let last_pos = block.end.saturating_sub(1).max(block.start);
        let mut end = line_end_incl_terminator(src, last_pos);
        let hit_eof = end == src.len() && !src[..end].ends_with('\n');

        if hit_eof {
            // No terminator on the last line; remove through EOF, and also strip the
            // terminator preceding our block so we don't leave a dangling blank line.
            if start > 0 {
                // start currently sits right after the previous line's terminator (or at 0).
                // Walk back over that terminator.
                if src.as_bytes()[start - 1] == b'\n' {
                    let mut new_start = start - 1;
                    if new_start > 0 && src.as_bytes()[new_start - 1] == b'\r' {
                        new_start -= 1;
                    }
                    start = new_start;
                }
            }
            end = src.len();
        } else {
            // Nice-to-have: collapse a blank line left after the block if both the line
            // before the block and the line after are blank.
            let before_blank = start == 0 || {
                let prev_line_start = line_start(src, start - 1);
                src[prev_line_start..start].trim().is_empty()
            };
            if before_blank {
                let after_line_end = line_end_incl_terminator(src, end);
                let after_line_text = &src[end..after_line_end];
                let stripped = after_line_text.trim_end_matches(['\r', '\n']);
                if stripped.trim().is_empty() && !after_line_text.is_empty() {
                    end = after_line_end;
                }
            }
        }

        Edit {
            start,
            end,
            replacement: String::new(),
        }
    } else if block.code_after {
        // Inline block comment with code after (e.g. `foo(/* x */ 1)`), or own-line block
        // comment with code after on the same line (e.g. `/* x */ let y;`). code_after can
        // only be true for Block comments, since a Line comment consumes to end of line.
        let start = block.start;
        let end = block.end;
        let bytes = src.as_bytes();
        let byte_after_is_space = end < src.len() && bytes[end] == b' ';
        let line_st = line_start(src, start);
        let byte_before_is_space = start > line_st && bytes[start - 1] == b' ';
        let mut removed_end = end;
        if byte_after_is_space && (start == line_st || byte_before_is_space) {
            removed_end += 1;
        }
        // If neither side of the comment is whitespace, deleting it verbatim would glue the
        // surrounding tokens together (e.g. `return/* c */undefined` -> `returnundefined`).
        // Insert a single space to keep them separate.
        let left_glue = start > 0 && !bytes[start - 1].is_ascii_whitespace();
        let right_glue = removed_end < src.len() && !bytes[removed_end].is_ascii_whitespace();
        let replacement = if left_glue && right_glue { " " } else { "" };
        Edit {
            start,
            end: removed_end,
            replacement: replacement.to_string(),
        }
    } else {
        // Trailing comment (code before, nothing after): walk back over spaces/tabs from
        // block.start to the end of the code, keep the line terminator.
        let mut start = block.start;
        let bytes = src.as_bytes();
        while start > 0 && (bytes[start - 1] == b' ' || bytes[start - 1] == b'\t') {
            start -= 1;
        }
        Edit {
            start,
            end: block.end,
            replacement: String::new(),
        }
    }
}

/// Edit that replaces an own-line block with `{indent}{prefix} {summary}` plus the file's line
/// terminator (CRLF preserved).
pub fn reduce_edit(src: &str, block: &CommentBlock, lang: Language, summary: &str) -> Edit {
    let start = block.start - block.indent.len();
    let last_pos = block.end.saturating_sub(1).max(block.start);
    let last_line_end_with_term = line_end_incl_terminator(src, last_pos);
    let had_terminator =
        last_line_end_with_term <= src.len() && src[..last_line_end_with_term].ends_with('\n');
    let end = last_line_end_with_term;

    let eol = if had_terminator { eol_of(src) } else { "" };
    let prefix = lang.line_prefix();
    let replacement = format!("{}{} {}{}", block.indent, prefix, summary, eol);

    Edit {
        start,
        end,
        replacement,
    }
}

/// Apply non-overlapping edits (any order) and return the new source.
pub fn apply(src: &str, edits: Vec<Edit>) -> String {
    let mut edits = edits;
    edits.sort_by_key(|e| std::cmp::Reverse(e.start));

    debug_assert!(
        edits.windows(2).all(|w| w[0].start >= w[1].end),
        "overlapping edits"
    );

    let mut out = src.to_string();
    for edit in &edits {
        out.replace_range(edit.start..edit.end, &edit.replacement);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Comment, CommentKind};

    fn block(
        src: &str,
        start: usize,
        end: usize,
        own_line: bool,
        code_after: bool,
    ) -> CommentBlock {
        let indent_start = line_start(src, start);
        let indent = src[indent_start..start].to_string();
        let start_line = src[..start].matches('\n').count();
        let end_line = src[..end].matches('\n').count();
        CommentBlock {
            comments: vec![Comment {
                start,
                end,
                kind: CommentKind::Line,
                text: src[start..end].to_string(),
                start_line,
                end_line,
                own_line,
                code_after,
            }],
            start,
            end,
            start_line,
            end_line,
            indent,
            own_line,
            code_after,
            kind: CommentKind::Line,
        }
    }

    #[test]
    fn multibyte_last_char_in_comment_does_not_panic() {
        // Comment ends in a 3-byte char, so block.end - 1 is not a char boundary (issue #6).
        let src = "let a = 1;\n// ─────\nlet b = 2;\n";
        let start = src.find("//").unwrap();
        let end = src.find("\nlet b").unwrap();
        let b = block(src, start, end, true, false);
        assert_eq!(
            apply(src, vec![delete_edit(src, &b)]),
            "let a = 1;\nlet b = 2;\n"
        );
        assert_eq!(
            apply(src, vec![reduce_edit(src, &b, Language::JavaScript, "x")]),
            "let a = 1;\n// x\nlet b = 2;\n"
        );
        // Same for a trailing comment and for a last line without a terminator.
        let src = "let a = 1; // ─\nlet b = 2; // ─";
        let s1 = src.find("//").unwrap();
        let e1 = src.find('\n').unwrap();
        let s2 = src.rfind("//").unwrap();
        let edits = vec![
            delete_edit(src, &block(src, s1, e1, false, false)),
            delete_edit(src, &block(src, s2, src.len(), false, false)),
        ];
        assert_eq!(apply(src, edits), "let a = 1;\nlet b = 2;");
    }

    #[test]
    fn own_line_run_deletion_crlf() {
        let src = "let a = 1;\r\n// foo\r\n// bar\r\nlet b = 2;\r\n";
        let start = src.find("// foo").unwrap();
        let end = src.find("// bar").unwrap() + "// bar".len();
        let b = block(src, start, end, true, false);
        let edit = delete_edit(src, &b);
        let out = apply(src, vec![edit]);
        assert_eq!(out, "let a = 1;\r\nlet b = 2;\r\n");
    }

    #[test]
    fn trailing_comment_deletion() {
        let src = "let a = 1; // set a\nlet b = 2;\n";
        let start = src.find("// set a").unwrap();
        let end = src.find('\n').unwrap();
        let b = block(src, start, end, false, false);
        let edit = delete_edit(src, &b);
        let out = apply(src, vec![edit]);
        assert_eq!(out, "let a = 1;\nlet b = 2;\n");
    }

    #[test]
    fn inline_block_comment_deletion_does_not_glue_adjacent_tokens() {
        let src = "function f() {\n  return/* explicit */undefined;\n}\n";
        let start = src.find("/* explicit */").unwrap();
        let end = start + "/* explicit */".len();
        let b = block(src, start, end, false, true);
        let edit = delete_edit(src, &b);
        let out = apply(src, vec![edit]);
        assert_eq!(out, "function f() {\n  return undefined;\n}\n");
    }

    #[test]
    fn reduce_preserves_indent() {
        let src = "fn f() {\n    // line one\n    // line two\n    do_thing();\n}\n";
        let start = src.find("// line one").unwrap();
        let end = start + "// line one\n    // line two".len();
        let b = block(src, start, end, true, false);
        let edit = reduce_edit(src, &b, Language::JavaScript, "does the thing");
        let out = apply(src, vec![edit]);
        assert_eq!(out, "fn f() {\n    // does the thing\n    do_thing();\n}\n");
    }
}
