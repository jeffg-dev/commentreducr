//! Detection of comments that must never be touched: tool directives, shebangs, encoding cookies,
//! license headers, JSDoc, source maps, region markers, TODO/FIXME, etc.
use crate::types::{CommentBlock, CommentKind, Language};
use regex::Regex;
use std::sync::LazyLock;

#[derive(Clone, Copy)]
enum Scope {
    Python,
    Js,
    Both,
}

impl Scope {
    fn matches(self, lang: Language) -> bool {
        match self {
            Scope::Both => true,
            Scope::Python => lang == Language::Python,
            Scope::Js => lang != Language::Python,
        }
    }
}

/// Table of directive patterns. Each row is checked against every comment's raw text in a block;
/// a match anywhere makes the block structural. Kept as a handful of alternations rather than one
/// regex per directive.
static RULES: LazyLock<Vec<(Scope, Regex)>> = LazyLock::new(|| {
    let specs: &[(Scope, &str)] = &[
        // Python: noqa, type:, pyright:, pylint:, mypy:, ruff:, isort:, fmt:, pragma, nosec,
        // noinspection, cython:, distutils:, %% cell markers.
        (
            Scope::Python,
            r"(?i)^#\s*(noqa\b|type:|pyright:|pylint:|mypy:|ruff:|isort:|fmt:|pragma\b|nosec\b|noinspection\b|cython:|distutils:|%%)",
        ),
        // Both: TODO/FIXME/XXX/HACK/NOTE, anywhere in the comment.
        (Scope::Both, r"(?i)\b(TODO|FIXME|XXX|HACK|NOTE)\b"),
        // JS/TS: eslint-*, @ts-*, prettier/biome ignore, coverage ignores, @flow, @jsx*, @license,
        // @preserve, @generated, #region/#endregion, webpack/vite magic comments.
        (
            Scope::Js,
            r"(?i)(eslint-(disable|enable)(-next-line|-line)?\b|@ts-(ignore|expect-error|nocheck|check)\b|prettier-ignore\b|biome-ignore\b|istanbul ignore\b|c8 ignore\b|v8 ignore\b|@flow\b|@jsx[-\w]*|@license\b|@preserve\b|@generated\b|#region\b|#endregion\b|\bwebpack\w*\s*:|\bvite\b\s*:?\s*ignore)",
        ),
        // JS/TS: reference/sourcemap directives anchored at the start of the comment.
        (
            Scope::Js,
            r"^(///\s*<reference|//#\s*source(MappingURL|URL))",
        ),
    ];
    specs
        .iter()
        .map(|(scope, pat)| (*scope, Regex::new(pat).unwrap()))
        .collect()
});

static CODING_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)coding[:=]\s*[-\w.]+").unwrap());

static LICENSE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(license|copyright|SPDX)\b").unwrap());

/// True if the block (or any comment in it) is structural and must be preserved in both modes.
/// `src` is unused now that license/copyright/SPDX detection no longer depends on file position,
/// but is kept in the signature per the module contract.
pub fn is_structural(block: &CommentBlock, lang: Language, _src: &str) -> bool {
    // Python shebang: the very first byte of the file.
    if lang == Language::Python && block.start == 0 {
        if let Some(first) = block.comments.first() {
            if first.start == 0 && first.text.starts_with("#!") {
                return true;
            }
        }
    }

    // PEP 263 coding cookie: must be on line 0 or 1 of the file.
    if lang == Language::Python
        && block
            .comments
            .iter()
            .any(|c| c.start_line <= 1 && CODING_RE.is_match(&c.text))
    {
        return true;
    }

    // JS/TS: any Block comment starting with `/**` or `/*!` (JSDoc, `@license`/`@preserve` banners).
    if lang != Language::Python
        && block.comments.iter().any(|c| {
            c.kind == CommentKind::Block && (c.text.starts_with("/**") || c.text.starts_with("/*!"))
        })
    {
        return true;
    }

    // License/copyright/SPDX text anywhere in the block is always structural (per DESIGN.md),
    // regardless of where in the file it appears.
    if block.comments.iter().any(|c| LICENSE_RE.is_match(&c.text)) {
        return true;
    }

    RULES.iter().any(|(scope, re)| {
        scope.matches(lang) && block.comments.iter().any(|c| re.is_match(&c.text))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Comment;

    fn block_for(text: &str, start: usize, start_line: usize) -> CommentBlock {
        let kind = if text.starts_with("/*") {
            CommentKind::Block
        } else {
            CommentKind::Line
        };
        let comment = Comment {
            start,
            end: start + text.len(),
            kind,
            text: text.to_string(),
            start_line,
            end_line: start_line,
            own_line: true,
            code_after: false,
        };
        CommentBlock {
            comments: vec![comment],
            start,
            end: start + text.len(),
            start_line,
            end_line: start_line,
            indent: String::new(),
            own_line: true,
            code_after: false,
            kind,
        }
    }

    #[test]
    fn table_of_directives() {
        let cases: &[(&str, Language, usize, usize, bool)] = &[
            ("#!/usr/bin/env python3", Language::Python, 0, 0, true),
            ("# -*- coding: utf-8 -*-", Language::Python, 0, 0, true),
            ("# noqa: E501", Language::Python, 100, 5, true),
            ("# just a plain comment", Language::Python, 100, 5, false),
            ("// eslint-disable-next-line no-unused-vars", Language::JavaScript, 50, 3, true),
            ("/** JSDoc comment */", Language::JavaScript, 0, 0, true),
            ("// TODO: fix this later", Language::JavaScript, 200, 20, true),
            ("// Copyright 2024 Acme Corp", Language::JavaScript, 0, 0, true),
        ];
        for (text, lang, start, start_line, expected) in cases {
            let block = block_for(text, *start, *start_line);
            let src = " ".repeat(*start);
            assert_eq!(
                is_structural(&block, *lang, &src),
                *expected,
                "text = {text:?}"
            );
        }
    }
}
