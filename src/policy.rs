//! Decide what to do with each block.
use crate::prose::ProseAnalysis;
use crate::types::{Action, CommentBlock, Config, Mode};

/// Rules:
/// - structural => Keep (both modes)
/// - Delete mode => Delete
/// - Reduce mode: keep trailing/inline comments (not own_line or code_after), keep blocks with
///   fewer than `min_lines` prose lines, keep low-density blocks (words_per_line < min_density),
///   keep code_like blocks; otherwise Reduce { prose: analysis.text, fallback: analysis.extractive }.
pub fn decide(block: &CommentBlock, analysis: &ProseAnalysis, structural: bool, cfg: &Config) -> Action {
    let _ = (block, analysis, structural, cfg, Mode::Reduce);
    todo!("policy::decide")
}
