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
    if structural {
        return Action::Keep;
    }
    match cfg.mode {
        Mode::Delete => Action::Delete,
        Mode::Reduce => {
            if !block.own_line || block.code_after {
                return Action::Keep;
            }
            if analysis.lines.len() < cfg.min_lines {
                return Action::Keep;
            }
            if analysis.words_per_line < cfg.min_density {
                return Action::Keep;
            }
            if analysis.code_like {
                return Action::Keep;
            }
            Action::Reduce {
                prose: analysis.text.clone(),
                fallback: analysis.extractive.clone(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(mode: Mode) -> Config {
        Config {
            mode,
            min_lines: 2,
            min_density: 3.0,
            max_summary_words: 12,
            endpoint: None,
            model: String::new(),
            api_key: None,
            llm_concurrency: 1,
            dry_run: false,
            verbose: false,
        }
    }

    fn block(own_line: bool, code_after: bool) -> CommentBlock {
        CommentBlock {
            comments: vec![],
            start: 0,
            end: 0,
            start_line: 0,
            end_line: 0,
            indent: String::new(),
            own_line,
            code_after,
            kind: crate::types::CommentKind::Line,
        }
    }

    fn analysis(lines: usize, words_per_line: f64, code_like: bool) -> ProseAnalysis {
        ProseAnalysis {
            lines: vec![String::new(); lines],
            text: "some prose text here".into(),
            sentences: vec![],
            word_count: 0,
            words_per_line,
            code_like,
            extractive: "some prose".into(),
        }
    }

    #[test]
    fn structural_always_kept() {
        let b = block(true, false);
        let a = analysis(5, 10.0, false);
        assert!(matches!(decide(&b, &a, true, &cfg(Mode::Delete)), Action::Keep));
        assert!(matches!(decide(&b, &a, true, &cfg(Mode::Reduce)), Action::Keep));
    }

    #[test]
    fn delete_mode_deletes_non_structural() {
        let b = block(true, false);
        let a = analysis(5, 10.0, false);
        assert!(matches!(decide(&b, &a, false, &cfg(Mode::Delete)), Action::Delete));
    }

    #[test]
    fn reduce_mode_reduces_dense_prose() {
        let b = block(true, false);
        let a = analysis(5, 10.0, false);
        assert!(matches!(decide(&b, &a, false, &cfg(Mode::Reduce)), Action::Reduce { .. }));
    }

    #[test]
    fn reduce_mode_keeps_short_and_low_density_and_trailing_and_code_like() {
        let c = cfg(Mode::Reduce);
        assert!(matches!(
            decide(&block(true, false), &analysis(1, 10.0, false), false, &c),
            Action::Keep
        ));
        assert!(matches!(
            decide(&block(true, false), &analysis(5, 1.0, false), false, &c),
            Action::Keep
        ));
        assert!(matches!(
            decide(&block(false, false), &analysis(5, 10.0, false), false, &c),
            Action::Keep
        ));
        assert!(matches!(
            decide(&block(true, true), &analysis(5, 10.0, false), false, &c),
            Action::Keep
        ));
        assert!(matches!(
            decide(&block(true, false), &analysis(5, 10.0, true), false, &c),
            Action::Keep
        ));
    }
}
