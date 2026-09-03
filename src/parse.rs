//! tree-sitter based comment extraction. Comments are `comment` nodes in all four grammars;
//! strings, template literals, regex literals, JSX text and Python docstrings are never comments,
//! so the grammar does the hard work for us.
use crate::types::{Comment, CommentBlock, Language};
use anyhow::Result;

/// All comment tokens in `src`, in source order.
pub fn extract_comments(src: &str, lang: Language) -> Result<Vec<Comment>> {
    let _ = (src, lang);
    todo!("parse::extract_comments")
}

/// Groups comments into blocks (see `CommentBlock` doc). Consecutive own-line Line comments on
/// adjacent lines with identical indentation merge; everything else is its own block.
pub fn group_blocks(src: &str, comments: Vec<Comment>) -> Vec<CommentBlock> {
    let _ = (src, comments);
    todo!("parse::group_blocks")
}
