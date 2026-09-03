//! Detection of comments that must never be touched: tool directives, shebangs, encoding cookies,
//! license headers, JSDoc, source maps, region markers, TODO/FIXME, etc.
use crate::types::{CommentBlock, Language};

/// True if the block (or any comment in it) is structural and must be preserved in both modes.
/// `src` is the full file source (needed e.g. to know the block is at the top of the file).
pub fn is_structural(block: &CommentBlock, lang: Language, src: &str) -> bool {
    let _ = (block, lang, src);
    todo!("structural::is_structural")
}
