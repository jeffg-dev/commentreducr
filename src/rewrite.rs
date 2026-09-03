//! Turn actions into byte-range edits and apply them. This is the only code that touches file
//! contents, so correctness here is what keeps the target codebase intact.
use crate::types::{CommentBlock, Language};

#[derive(Debug, Clone)]
pub struct Edit {
    pub start: usize,
    pub end: usize,
    pub replacement: String,
}

/// Edit that removes the block.
/// - own-line block with no code after: remove the whole lines including their line terminators.
/// - trailing comment (code before it): remove from the end of the code (trim trailing spaces) to
///   the end of the comment, keeping the line terminator.
/// - inline block comment with code after: remove just the span plus one adjacent space if both
///   neighbours are spaces.
pub fn delete_edit(src: &str, block: &CommentBlock) -> Edit {
    let _ = (src, block);
    todo!("rewrite::delete_edit")
}

/// Edit that replaces an own-line block with `{indent}{prefix} {summary}` plus the file's line
/// terminator (CRLF preserved).
pub fn reduce_edit(src: &str, block: &CommentBlock, lang: Language, summary: &str) -> Edit {
    let _ = (src, block, lang, summary);
    todo!("rewrite::reduce_edit")
}

/// Apply non-overlapping edits (any order) and return the new source.
pub fn apply(src: &str, edits: Vec<Edit>) -> String {
    let _ = (src, edits);
    todo!("rewrite::apply")
}
