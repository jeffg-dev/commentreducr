//! Python docstring extraction, the "always keep" structural rules for docstrings, and the
//! delete/replace edits used to remove or rewrite one. Pure library code: nothing here reads
//! files or wires into the CLI, that is a later agent's job.
//!
//! A docstring is `expression_statement > string` as the first non-comment statement of `module`,
//! or of the `block` child of `class_definition` / `function_definition` (an `async def` is still
//! a `function_definition`; a decorated def/class is wrapped in `decorated_definition`, whose
//! `decorator` children are the decorators and whose `definition` field is the real node). A
//! `string` node with an `f`/`F`/`b`/`B` anywhere in its prefix is a formatted or byte string, not
//! a docstring; only `r`/`R`/`u`/`U` (or no prefix) qualify.
use crate::rewrite::Edit;
use anyhow::{Result, anyhow};
use regex::Regex;
use std::path::{Component, Path};
use std::sync::LazyLock;
use tree_sitter::Node;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocKind {
    Module,
    Class,
    Function,
}

#[derive(Debug, Clone)]
pub struct Docstring {
    /// Byte range of the whole string node (prefix + quotes).
    pub start: usize,
    pub end: usize,
    /// 0-based.
    pub start_line: usize,
    pub end_line: usize,
    /// Leading whitespace of the line the string starts on.
    pub indent: String,
    pub kind: DocKind,
    /// def/class name; "" for Module.
    pub name: String,
    /// The def/class header line(s) collapsed to one line, trimmed, <= 120 chars; "" for Module.
    pub signature: String,
    /// Decorator texts (e.g. "@click.command()"), trimmed.
    pub decorators: Vec<String>,
    /// Function named test*/Test*, Class named Test*, or Module when in_test_file.
    pub is_test: bool,
    pub in_test_file: bool,
    /// The docstring is the entire body (deleting it would leave an empty block).
    pub only_statement: bool,
    /// Only whitespace precedes the string on its first line.
    pub own_line: bool,
    /// Non-whitespace follows the string on its last line (e.g. `"""x"""; y = 1`).
    pub code_after: bool,
    /// "", "r", "u", ...
    pub prefix: String,
    /// `"""`, `'''`, `"`, or `'`.
    pub quote: String,
    /// Cleaned docstring text: `inspect.cleandoc` semantics (first line stripped, common indent
    /// of the rest removed, leading/trailing blank lines dropped).
    pub text: String,
    /// For Function/Class: up to 3 trimmed non-blank source lines of the body after the
    /// docstring, each cut to 80 chars; empty for Module.
    pub body_preview: Vec<String>,
    /// Number of non-blank body lines after the docstring (0 when only_statement).
    pub body_lines: usize,
}

static LICENSE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(license|copyright|SPDX)\b").unwrap());

/// File name `test_*.py` / `*_test.py` / `conftest.py`, or any path component named "tests" or
/// "test".
pub fn is_test_file(path: &Path) -> bool {
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        if name == "conftest.py" {
            return true;
        }
        if let Some(stem) = name.strip_suffix(".py")
            && (stem.starts_with("test_") || stem.ends_with("_test"))
        {
            return true;
        }
    }
    path.components()
        .any(|c| matches!(c, Component::Normal(s) if s == "tests" || s == "test"))
}

/// Every docstring in `src`, in source order (module, then every class/function's docstring,
/// nested ones included). `Err` when the tree has parse errors, like `parse::extract_comments`.
pub fn extract_docstrings(src: &str, in_test_file: bool) -> Result<Vec<Docstring>> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&tree_sitter_python::LANGUAGE.into())?;
    let tree = parser
        .parse(src.as_bytes(), None)
        .ok_or_else(|| anyhow!("tree-sitter failed to parse"))?;
    if tree.root_node().has_error() {
        return Err(anyhow!("parse errors"));
    }

    let lines: Vec<&str> = src.lines().collect();
    let mut out = Vec::new();
    walk(tree.root_node(), src, &lines, in_test_file, &mut out);
    Ok(out)
}

fn walk(node: Node, src: &str, lines: &[&str], in_test_file: bool, out: &mut Vec<Docstring>) {
    match node.kind() {
        "module" => {
            if let Some(string_node) = first_stmt_string(node)
                && let Some(doc) = make_docstring(
                    string_node,
                    node,
                    0,
                    DocKind::Module,
                    String::new(),
                    String::new(),
                    Vec::new(),
                    src,
                    lines,
                    in_test_file,
                )
            {
                out.push(doc);
            }
        }
        "function_definition" | "class_definition" => {
            let kind = if node.kind() == "function_definition" {
                DocKind::Function
            } else {
                DocKind::Class
            };
            if let Some(body) = node.child_by_field_name("body")
                && let Some(string_node) = first_stmt_string(body)
            {
                let name = node
                    .child_by_field_name("name")
                    .map(|n| src[n.start_byte()..n.end_byte()].to_string())
                    .unwrap_or_default();
                let decorators = decorators_of(node, src);
                let signature = header_signature(node, src);
                if let Some(doc) = make_docstring(
                    string_node,
                    body,
                    node.end_position().row,
                    kind,
                    name,
                    signature,
                    decorators,
                    src,
                    lines,
                    in_test_file,
                ) {
                    out.push(doc);
                }
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, src, lines, in_test_file, out);
    }
}

/// The first non-comment child of `parent` (a `module` or `block` node), if it is a bare `string`
/// expression statement. Everything else about the block is irrelevant: only the first statement
/// can ever be a docstring.
fn first_stmt_string<'a>(parent: Node<'a>) -> Option<Node<'a>> {
    let mut cursor = parent.walk();
    for child in parent.children(&mut cursor) {
        if child.kind() == "comment" {
            continue;
        }
        if child.kind() == "expression_statement" {
            let mut c2 = child.walk();
            let mut kids = child.children(&mut c2);
            let first = kids.next();
            let has_second = kids.next().is_some();
            if !has_second
                && let Some(s) = first
                && s.kind() == "string"
            {
                return Some(s);
            }
        }
        return None;
    }
    None
}

/// True if `parent`'s only non-comment child is the docstring statement itself.
fn only_stmt_of(parent: Node) -> bool {
    let mut cursor = parent.walk();
    parent
        .children(&mut cursor)
        .filter(|c| c.kind() != "comment")
        .count()
        == 1
}

/// Decorator texts (trimmed) of a `function_definition`/`class_definition` wrapped in a
/// `decorated_definition`; empty if it is not decorated.
fn decorators_of(node: Node, src: &str) -> Vec<String> {
    let Some(parent) = node.parent() else {
        return Vec::new();
    };
    if parent.kind() != "decorated_definition" {
        return Vec::new();
    }
    let mut cursor = parent.walk();
    parent
        .children(&mut cursor)
        .filter(|c| c.kind() == "decorator")
        .map(|c| src[c.start_byte()..c.end_byte()].trim().to_string())
        .collect()
}

/// The def/class header, collapsed to one line and trimmed, up to and including the `:` that is a
/// direct child of the definition node (so a `:` nested in a type annotation or default value is
/// never mistaken for it).
fn header_signature(node: Node, src: &str) -> String {
    let mut end = node.start_byte();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() && child.kind() == ":" {
            end = child.end_byte();
            break;
        }
    }
    let collapsed = src[node.start_byte()..end]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if collapsed.chars().count() > 120 {
        collapsed.chars().take(120).collect()
    } else {
        collapsed
    }
}

/// Prefix (chars before the first quote char) and quote sequence of a `string_start` node's text.
fn split_prefix_quote(text: &str) -> (String, String) {
    let idx = text.find(['\'', '"']).unwrap_or(text.len());
    (text[..idx].to_string(), text[idx..].to_string())
}

fn is_docstring_prefix(prefix: &str) -> bool {
    prefix.chars().all(|c| matches!(c, 'r' | 'R' | 'u' | 'U'))
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

/// Byte offset just past the terminator of the line containing `pos`, or EOF if that line has no
/// terminator.
fn line_end_incl_terminator(src: &str, pos: usize) -> usize {
    match src.as_bytes()[pos..].iter().position(|&b| b == b'\n') {
        Some(i) => pos + i + 1,
        None => src.len(),
    }
}

#[allow(clippy::too_many_arguments)]
fn make_docstring(
    string_node: Node,
    stmt_parent: Node,
    enclosing_end_line: usize,
    kind: DocKind,
    name: String,
    signature: String,
    decorators: Vec<String>,
    src: &str,
    lines: &[&str],
    in_test_file: bool,
) -> Option<Docstring> {
    let string_start = string_node.child(0)?;
    if string_start.kind() != "string_start" {
        return None;
    }
    let (prefix, quote) =
        split_prefix_quote(&src[string_start.start_byte()..string_start.end_byte()]);
    if !is_docstring_prefix(&prefix) {
        return None;
    }

    let start = string_node.start_byte();
    let end = string_node.end_byte();
    let ls = line_start(src, start);
    let own_line = src[ls..start].chars().all(char::is_whitespace);
    let le = line_end(src, end);
    let code_after = !src[end..le].trim().is_empty();
    let indent = src[ls..start].to_string();

    let mut raw_content = String::new();
    let mut cursor = string_node.walk();
    for child in string_node.children(&mut cursor) {
        if child.kind() == "string_content" {
            raw_content = src[child.start_byte()..child.end_byte()].to_string();
            break;
        }
    }

    let only_statement = only_stmt_of(stmt_parent);
    let end_line = string_node.end_position().row;
    let (body_preview, body_lines) =
        if matches!(kind, DocKind::Function | DocKind::Class) && !only_statement {
            body_preview_and_lines(lines, end_line, enclosing_end_line)
        } else {
            (Vec::new(), 0)
        };

    let is_test = match kind {
        DocKind::Module => in_test_file,
        DocKind::Function => name.starts_with("test") || name.starts_with("Test"),
        DocKind::Class => name.starts_with("Test"),
    };

    Some(Docstring {
        start,
        end,
        start_line: string_node.start_position().row,
        end_line,
        indent,
        kind,
        name,
        signature,
        decorators,
        is_test,
        in_test_file,
        only_statement,
        own_line,
        code_after,
        prefix,
        quote,
        text: cleandoc(&raw_content),
        body_preview,
        body_lines,
    })
}

/// Up to 3 trimmed, 80-char-capped non-blank source lines after the docstring's last line through
/// `block_end_line` (both inclusive of range, 0-based), plus a count of all non-blank lines in
/// that range.
fn body_preview_and_lines(
    lines: &[&str],
    doc_end_line: usize,
    block_end_line: usize,
) -> (Vec<String>, usize) {
    let mut preview = Vec::new();
    let mut count = 0;
    if doc_end_line >= block_end_line {
        return (preview, count);
    }
    for line_no in (doc_end_line + 1)..=block_end_line {
        let Some(line) = lines.get(line_no) else {
            continue;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        count += 1;
        if preview.len() < 3 {
            preview.push(trimmed.chars().take(80).collect());
        }
    }
    (preview, count)
}

/// `str.expandtabs()` with the given tab size: each `\t` advances to the next stop, columns reset
/// after `\n`.
fn expandtabs(s: &str, tabsize: usize) -> String {
    let mut out = String::with_capacity(s.len());
    let mut col = 0;
    for ch in s.chars() {
        match ch {
            '\t' => {
                let spaces = tabsize - (col % tabsize);
                out.push_str(&" ".repeat(spaces));
                col += spaces;
            }
            '\n' => {
                out.push(ch);
                col = 0;
            }
            _ => {
                out.push(ch);
                col += 1;
            }
        }
    }
    out
}

/// `inspect.cleandoc` semantics: expand tabs, strip the first line, dedent every other line by the
/// smallest indentation among the non-blank ones, then drop leading/trailing blank lines.
fn cleandoc(raw: &str) -> String {
    let expanded = expandtabs(raw, 8);
    let mut lines: Vec<String> = expanded
        .split('\n')
        .map(|l| l.trim_end_matches('\r').to_string())
        .collect();

    let mut margin = usize::MAX;
    for line in lines.iter().skip(1) {
        let stripped = line.trim_start();
        if !stripped.is_empty() {
            margin = margin.min(line.len() - stripped.len());
        }
    }

    if let Some(first) = lines.first_mut() {
        *first = first.trim_start().to_string();
    }
    if margin < usize::MAX {
        for line in lines.iter_mut().skip(1) {
            *line = if line.len() >= margin {
                line[margin..].to_string()
            } else {
                String::new()
            };
        }
    }

    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    while lines.first().is_some_and(|l| l.is_empty()) {
        lines.remove(0);
    }
    lines.join("\n")
}

/// Always kept, in every mode: doctest prompts, a Module docstring in a file that reads
/// `__doc__` (argparse/click render it as help text), license/copyright/SPDX text, or any
/// decorator that looks like a click/typer command (its docstring becomes the command's help).
pub fn is_structural(doc: &Docstring, src: &str) -> bool {
    if doc.text.contains(">>>") {
        return true;
    }
    if doc.kind == DocKind::Module && src.contains("__doc__") {
        return true;
    }
    if LICENSE_RE.is_match(&doc.text) {
        return true;
    }
    doc.decorators.iter().any(|d| {
        let lower = d.to_lowercase();
        lower.contains("command") || lower.contains("group")
    })
}

/// Edit that removes `doc`, or `None` when it is too risky to touch (code shares its first or
/// last line with something else besides its own `def`/`class` header).
/// - `only_statement`: the whole string node becomes `pass` (handles both an own-line docstring
///   and an inline `def g(self): """x"""`).
/// - own-line, nothing after it on the line: remove its whole lines (indent through terminator)
///   plus any immediately-following blank lines, so the body/module never starts on a blank line.
/// - anything else (code after it on the same line, or sharing a line with other statements):
///   left untouched.
pub fn delete_edit(src: &str, doc: &Docstring) -> Option<Edit> {
    if doc.only_statement {
        return Some(Edit {
            start: doc.start,
            end: doc.end,
            replacement: "pass".to_string(),
        });
    }
    if !doc.own_line || doc.code_after {
        return None;
    }

    let mut start = doc.start - doc.indent.len();
    let last_pos = doc.end.saturating_sub(1).max(doc.start);
    let mut end = line_end_incl_terminator(src, last_pos);
    let hit_eof = end == src.len() && !src[..end].ends_with('\n');

    if hit_eof {
        // No terminator on the last line; remove through EOF, and also strip the terminator
        // preceding our span so we don't leave a dangling blank line.
        if start > 0 && src.as_bytes()[start - 1] == b'\n' {
            let mut new_start = start - 1;
            if new_start > 0 && src.as_bytes()[new_start - 1] == b'\r' {
                new_start -= 1;
            }
            start = new_start;
        }
        end = src.len();
    } else {
        // Consume every immediately-following blank line so the body/module doesn't start blank.
        loop {
            let next_end = line_end_incl_terminator(src, end);
            if next_end == end {
                break;
            }
            let line_text = &src[end..next_end];
            if line_text.trim().is_empty() {
                end = next_end;
            } else {
                break;
            }
        }
    }

    Some(Edit {
        start,
        end,
        replacement: String::new(),
    })
}

/// Edit that replaces `doc`'s text with `new_text` (lines already joined with "\n", already
/// trimmed), keeping the original prefix and quote. `None` if `new_text` is empty or would be
/// unsafe to splice in verbatim (it contains the quote sequence itself, or a backslash).
pub fn replace_edit(_src: &str, doc: &Docstring, new_text: &str) -> Option<Edit> {
    if new_text.is_empty() || new_text.contains(doc.quote.as_str()) || new_text.contains('\\') {
        return None;
    }

    let doc_lines: Vec<&str> = new_text.split('\n').collect();
    let mut replacement = String::new();
    replacement.push_str(&doc.prefix);
    replacement.push_str(&doc.quote);

    if doc_lines.len() == 1 {
        replacement.push_str(new_text);
        replacement.push_str(&doc.quote);
    } else {
        replacement.push_str(doc_lines[0]);
        replacement.push('\n');
        for line in &doc_lines[1..] {
            if line.trim().is_empty() {
                replacement.push('\n');
            } else {
                replacement.push_str(&doc.indent);
                replacement.push_str(line);
                replacement.push('\n');
            }
        }
        replacement.push_str(&doc.indent);
        replacement.push_str(&doc.quote);
    }

    Some(Edit {
        start: doc.start,
        end: doc.end,
        replacement,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rewrite;
    use std::path::Path;

    fn by_name<'a>(docs: &'a [Docstring], name: &str) -> &'a Docstring {
        docs.iter()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("no docstring named {name:?} in {docs:#?}"))
    }

    #[test]
    fn extraction_covers_every_shape() {
        let src = r#""""Module doc.

More text.
"""

import os


class Foo:
    r'''Class doc.'''

    def method(self):
        """Only stmt."""

    def g(self): """inline"""


def not_a_docstring_fstring():
    f"""fstring {1}"""
    return 1


def not_a_docstring_bytes():
    b"""bytes"""
    return 1


def not_a_docstring_second_stmt():
    x = 1
    """second statement, not a docstring"""


@decorator_one
@decorator_two(arg=1)
def decorated():
    """Decorated doc."""


async def test_async_thing():
    """Async test doc."""


def outer():
    def inner():
        """Nested doc."""
"#;
        let docs = extract_docstrings(src, false).unwrap();
        let names: Vec<&str> = docs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "",
                "Foo",
                "method",
                "g",
                "decorated",
                "test_async_thing",
                "inner"
            ],
            "f-string/bytes/second-statement bodies must not produce a docstring"
        );

        let module = &docs[0];
        assert_eq!(module.kind, DocKind::Module);
        assert_eq!(module.prefix, "");
        assert_eq!(module.quote, "\"\"\"");
        assert_eq!(module.text, "Module doc.\n\nMore text.");
        assert!(module.own_line);
        assert!(!module.only_statement, "import os follows it");
        assert!(!module.is_test);
        assert_eq!(module.signature, "");
        assert!(module.decorators.is_empty());

        let foo = by_name(&docs, "Foo");
        assert_eq!(foo.kind, DocKind::Class);
        assert_eq!(foo.prefix, "r");
        assert_eq!(foo.quote, "'''");
        assert_eq!(foo.text, "Class doc.");
        assert_eq!(foo.signature, "class Foo:");
        assert!(!foo.only_statement, "method and g follow it");
        assert!(!foo.is_test);

        let method = by_name(&docs, "method");
        assert_eq!(method.kind, DocKind::Function);
        assert_eq!(method.signature, "def method(self):");
        assert!(method.only_statement);
        assert!(method.own_line);
        assert_eq!(method.text, "Only stmt.");

        let g = by_name(&docs, "g");
        assert_eq!(g.signature, "def g(self):");
        assert!(g.only_statement);
        assert!(
            !g.own_line,
            "code (`def g(self): `) precedes it on its line"
        );
        assert_eq!(g.text, "inline");

        let decorated = by_name(&docs, "decorated");
        assert_eq!(
            decorated.decorators,
            vec!["@decorator_one", "@decorator_two(arg=1)"]
        );
        assert_eq!(decorated.signature, "def decorated():");
        assert!(!decorated.is_test);

        let test_async = by_name(&docs, "test_async_thing");
        assert_eq!(test_async.kind, DocKind::Function);
        assert_eq!(test_async.signature, "async def test_async_thing():");
        assert!(test_async.is_test);
        assert_eq!(test_async.text, "Async test doc.");

        let inner = by_name(&docs, "inner");
        assert_eq!(inner.kind, DocKind::Function);
        assert_eq!(inner.signature, "def inner():");
        assert!(inner.only_statement);
        assert_eq!(inner.text, "Nested doc.");
    }

    #[test]
    fn cleandoc_dedents_and_trims_blank_lines() {
        let src = "def f():\n    \"\"\"First line.\n\n        Indented para.\n        More indented.\n\n    \"\"\"\n";
        let docs = extract_docstrings(src, false).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(
            docs[0].text,
            "First line.\n\nIndented para.\nMore indented."
        );
    }

    #[test]
    fn delete_edit_on_a_realistic_file() {
        let src = "\"\"\"Module doc.\"\"\"\n\nimport os\n\n\ndef f():\n    \"\"\"Function doc.\"\"\"\n    return 1\n\n\nclass C:\n    \"\"\"Only stmt.\"\"\"\n";
        let docs = extract_docstrings(src, false).unwrap();
        assert_eq!(docs.len(), 3);
        let edits: Vec<Edit> = docs.iter().map(|d| delete_edit(src, d).unwrap()).collect();
        let out = rewrite::apply(src, edits);
        assert_eq!(
            out,
            "import os\n\n\ndef f():\n    return 1\n\n\nclass C:\n    pass\n"
        );
        // Must still parse cleanly.
        assert!(extract_docstrings(&out, false).is_ok());
    }

    #[test]
    fn delete_edit_preserves_crlf() {
        let src = "\"\"\"Doc.\"\"\"\r\n\r\nimport os\r\n";
        let docs = extract_docstrings(src, false).unwrap();
        assert_eq!(docs.len(), 1);
        let edit = delete_edit(src, &docs[0]).unwrap();
        let out = rewrite::apply(src, vec![edit]);
        assert_eq!(out, "import os\r\n");
    }

    #[test]
    fn delete_edit_at_eof_without_terminator() {
        // Not a real docstring (it's the block's second statement) -- built by hand to exercise
        // delete_edit's own-line-at-EOF branch, which mirrors rewrite::delete_edit's hit_eof
        // handling: strip the terminator before the span too, so no dangling blank line is left.
        let src = "class C:\n    x = 1\n    \"\"\"doc\"\"\"";
        let start = src.rfind("\"\"\"doc\"\"\"").unwrap();
        let doc = Docstring {
            start,
            end: src.len(),
            start_line: 2,
            end_line: 2,
            indent: "    ".to_string(),
            kind: DocKind::Class,
            name: String::new(),
            signature: String::new(),
            decorators: Vec::new(),
            is_test: false,
            in_test_file: false,
            only_statement: false,
            own_line: true,
            code_after: false,
            prefix: String::new(),
            quote: "\"\"\"".to_string(),
            text: "doc".to_string(),
            body_preview: Vec::new(),
            body_lines: 0,
        };
        let edit = delete_edit(src, &doc).unwrap();
        let out = rewrite::apply(src, vec![edit]);
        assert_eq!(out, "class C:\n    x = 1");
    }

    #[test]
    fn delete_edit_leaves_code_after_untouched() {
        let src = "def g(): \"\"\"x\"\"\"; y = 1\n";
        let docs = extract_docstrings(src, false).unwrap();
        assert_eq!(docs.len(), 1);
        assert!(!docs[0].only_statement);
        assert!(delete_edit(src, &docs[0]).is_none());
    }

    #[test]
    fn replace_edit_one_line_at_four_space_indent() {
        let src = "def f():\n    \"\"\"old\"\"\"\n";
        let docs = extract_docstrings(src, false).unwrap();
        let edit = replace_edit(src, &docs[0], "New one line").unwrap();
        let out = rewrite::apply(src, vec![edit]);
        assert_eq!(out, "def f():\n    \"\"\"New one line\"\"\"\n");
    }

    #[test]
    fn replace_edit_multi_line_at_eight_space_indent_preserves_quote() {
        let src = "class C:\n    def m(self):\n        '''old'''\n";
        let docs = extract_docstrings(src, false).unwrap();
        assert_eq!(docs[0].indent, "        ");
        assert_eq!(docs[0].quote, "'''");
        let edit = replace_edit(src, &docs[0], "Line one\n\nLine two").unwrap();
        let out = rewrite::apply(src, vec![edit]);
        assert_eq!(
            out,
            "class C:\n    def m(self):\n        '''Line one\n\n        Line two\n        '''\n"
        );
    }

    #[test]
    fn replace_edit_refuses_unsafe_new_text() {
        let src = "def f():\n    \"\"\"old\"\"\"\n";
        let docs = extract_docstrings(src, false).unwrap();
        assert!(replace_edit(src, &docs[0], "").is_none());
        assert!(replace_edit(src, &docs[0], "has \"\"\" inside").is_none());
        assert!(replace_edit(src, &docs[0], "has \\ inside").is_none());
    }

    #[test]
    fn is_test_file_table() {
        let cases: &[(&str, bool)] = &[
            ("test_foo.py", true),
            ("foo_test.py", true),
            ("conftest.py", true),
            ("tests/foo.py", true),
            ("pkg/test/foo.py", true),
            ("foo.py", false),
            ("mytest_helpers.py", false),
            ("testing/foo.py", false),
        ];
        for (path, expected) in cases {
            assert_eq!(is_test_file(Path::new(path)), *expected, "path = {path:?}");
        }
    }

    #[test]
    fn is_structural_table() {
        let base = |kind, text: &str, decorators: Vec<&str>| Docstring {
            start: 0,
            end: 0,
            start_line: 0,
            end_line: 0,
            indent: String::new(),
            kind,
            name: String::new(),
            signature: String::new(),
            decorators: decorators.into_iter().map(str::to_string).collect(),
            is_test: false,
            in_test_file: false,
            only_statement: false,
            own_line: true,
            code_after: false,
            prefix: String::new(),
            quote: "\"\"\"".to_string(),
            text: text.to_string(),
            body_preview: Vec::new(),
            body_lines: 0,
        };

        assert!(is_structural(
            &base(DocKind::Function, "usage:\n    >>> f()\n    1", vec![]),
            ""
        ));
        assert!(is_structural(
            &base(DocKind::Module, "plain module doc", vec![]),
            "print(__doc__)"
        ));
        assert!(
            !is_structural(
                &base(DocKind::Class, "plain class doc", vec![]),
                "print(__doc__)"
            ),
            "only a Module docstring is structural for __doc__"
        );
        assert!(is_structural(
            &base(DocKind::Module, "License: MIT", vec![]),
            ""
        ));
        assert!(is_structural(
            &base(DocKind::Function, "run the thing", vec!["@app.command()"]),
            ""
        ));
        assert!(is_structural(
            &base(DocKind::Function, "run the thing", vec!["@cli.group()"]),
            ""
        ));
        assert!(!is_structural(
            &base(
                DocKind::Function,
                "just a normal docstring",
                vec!["@property"]
            ),
            ""
        ));
    }
}

#[cfg(test)]
mod bad_example_tests {
    use super::*;

    // A real test module (not committed elsewhere), embedded verbatim so this test has no
    // dependency on an external path. Exercises extraction against realistic, heavily-docstringed
    // test code rather than hand-rolled fixtures.
    const BAD_EXAMPLE: &str = r#"
"""A rejected bulk-close is a 400 with its reason, not a traceback in the logs.

``OrderService.bulk_close_orders`` raises ``ValidationError`` for the ordinary
business-rule refusals -- the account does not resolve, an order in the selection has
unpaid invoices or open line items, draft orders are still awaiting review. The route
catches that, answers 400 with the service's own message, and records one INFO line at
``logger.info``.

``exc_info`` is what these assertions exist to keep off that line: the handler's
formatter renders it regardless of level, so an INFO call carrying it prints a header
line plus a frame per stack level for a request that has already succeeded at telling
someone "no".

``test_form_submit_rejection_noise.py`` pins the same disposal for the web form's
submission rejections; these assertions are deliberately its shape, so the two
rejection surfaces stay consistent.

Not every `ValidationError` reaching this arm is a refusal, and the two fault sources
dispose of their diagnostics differently:

* `legacy_services.py` catches `Exception` around the write, rolls back, and re-raises
  `ValidationError("There was an error trying to bulk close orders.")`. Raised inside
  an except block, it carries the DB fault as `__context__`, so a formatter given
  `exc_info` renders that fault's frames too. This arm may record it without frames
  because the service reports the original at ERROR with `exc_info` one frame earlier;
  `test_the_converting_arm_reports_the_original_fault` is what holds that true.
* `AccountService.refresh_account_summary`, called for the post-commit account rollup
  once the orders are already persisted, reaches this arm with "Account does not
  exist." and "Account summary does not exist." (see `account/services.py`; its
  suspended-account refusal cannot arrive from this route, which pre-checks
  `account_is_suspended`). Neither carries an upstream ERROR log, and the second is a
  tenant-configuration fault rather than a refusal, so this line is its only record.

The frame stack is otherwise what is given up, and that is the intent: for a refusal
the frames describe the ordinary call path into the service and identify no fault. The
exception's message is preserved in the line, which for a validation rejection is the
whole diagnostic payload -- and is the same text the caller already receives in the
response body.
"""

import ast
import contextlib
import inspect
import textwrap
from types import SimpleNamespace
from unittest.mock import patch

import pytest
from flask import Flask

from myapp.api.order.routes import bulk_close_orders
from myapp.api.order.services.draft_guard import DRAFT_REVIEW_REQUIRED_MESSAGE
from myapp.api.order.legacy_services import OrderService
from myapp.api.utils.exceptions import ValidationError
from myapp.api.utils.tests.error_log_ast import looks_like_logger

# Refusal messages this arm receives. The two `validate_order_can_be_closed` ones are
# built inline in the service, so they are illustrative renderings rather than pinned
# literals -- the assertions hold for any string, since the exception is mock-injected.
_REJECTIONS = pytest.mark.parametrize(
    "reason",
    [
        "Account not found.",
        "Order #1001: Cannot close order. Resolve the following first: Unpaid invoice; Missing approval",
        "Order #1001: Cannot close order. 2 line items must be fulfilled first.",
        DRAFT_REVIEW_REQUIRED_MESSAGE,
    ],
)


def _make_app():
    app = Flask(__name__)
    app.config["SECRET_KEY"] = "test-secret"
    app.config["TESTING"] = True
    app.add_url_rule("/orders/bulk_close", view_func=bulk_close_orders, methods=["POST"])
    return app


@contextlib.contextmanager
def _service_rejecting(reason):
    """POST a bulk-close whose service refuses, yielding (response, logger).

    The route's own gates are stubbed so the ``ValidationError`` under test is the
    service's: an unstubbed ``get_account_by_id`` would answer 404 first and the arm
    would never run.
    """
    with (
        patch("flask_login.utils._get_user", return_value=SimpleNamespace(is_authenticated=True, id=7, tenant=42)),
        patch("myapp.api.authz.route_policy._decide_resource"),
        patch("myapp.api.order.routes.decode_id_or_none", return_value=1),
        patch("myapp.api.order.routes.AccountHelper.get_account_by_id", return_value=SimpleNamespace(id=1)),
        patch("myapp.api.order.routes.OrderService.bulk_close_orders", side_effect=ValidationError(reason)),
        patch("myapp.api.order.routes.logger") as logger_mock,
    ):
        with _make_app().test_client() as client:
            yield client.post("/orders/bulk_close", json={"accountId": "abc", "orderIds": ["o1", "o2"]}), logger_mock


@_REJECTIONS
def test_rejection_returns_the_services_own_message(reason):
    with _service_rejecting(reason) as (response, _):
        assert response.status_code == 400
        assert response.get_json()["message"] == reason


@_REJECTIONS
def test_rejection_is_not_logged_as_a_fault(reason):
    """ERROR would mint an error-tracker event for a correct 4xx, on top of printing the stack."""
    with _service_rejecting(reason) as (_, logger_mock):
        logger_mock.exception.assert_not_called()
        logger_mock.error.assert_not_called()
        logger_mock.critical.assert_not_called()


@_REJECTIONS
def test_rejection_is_recorded_at_info_without_a_traceback(reason):
    """One greppable line, carrying the reason as a `%s` argument rather than the frames."""
    with _service_rejecting(reason) as (_, logger_mock):
        logger_mock.info.assert_called_once()
        args, kwargs = logger_mock.info.call_args

    assert "exc_info" not in kwargs
    assert "exc_info" not in kwargs.get("extra", {})
    assert "%s" in args[0]
    assert reason in args[0] % args[1:]


def test_an_unexpected_failure_still_reports_a_traceback():
    """Only the rejection arm was quietened -- the blanket arm must still report."""
    with (
        patch("flask_login.utils._get_user", return_value=SimpleNamespace(is_authenticated=True, id=7, tenant=42)),
        patch("myapp.api.authz.route_policy._decide_resource"),
        patch("myapp.api.order.routes.decode_id_or_none", return_value=1),
        patch("myapp.api.order.routes.AccountHelper.get_account_by_id", return_value=SimpleNamespace(id=1)),
        patch(
            "myapp.api.order.routes.OrderService.bulk_close_orders",
            side_effect=RuntimeError("database connection closed"),
        ) as service_mock,
        patch("myapp.api.order.routes.logger") as logger_mock,
    ):
        with _make_app().test_client() as client:
            response = client.post("/orders/bulk_close", json={"accountId": "abc", "orderIds": ["o1"]})

    service_mock.assert_called_once()
    assert response.status_code == 400
    logger_mock.exception.assert_called_once()
    assert "database connection closed" not in response.get_json()["message"]


def _emits_a_traceback(call: ast.Call, caught_name: str | None) -> bool:
    """Whether this logging call actually renders frames.

    `exc_info`'s presence is not the question -- `logging` reads a falsy value as
    absent, so `exception(..., exc_info=None)` prints no traceback, while a bare
    `exception(...)` prints one because the method defaults the argument to True.
    """
    if not (isinstance(call.func, ast.Attribute) and looks_like_logger(call.func.value)):
        return False
    exc_info = next((keyword.value for keyword in call.keywords if keyword.arg == "exc_info"), None)
    if exc_info is None:
        return call.func.attr == "exception"
    if isinstance(exc_info, ast.Constant):
        return bool(exc_info.value)
    return isinstance(exc_info, ast.Name) and exc_info.id == caught_name


def _converting_arm(source: str) -> ast.ExceptHandler:
    """The arm turning a write failure into a rejection, found by that conversion.

    Located by what it raises rather than by position, so neither a later arm's own
    reporting nor a reordering can stand in for it.
    """
    arms = [
        handler
        for handler in ast.walk(ast.parse(textwrap.dedent(source)))
        if isinstance(handler, ast.ExceptHandler)
        and any(
            isinstance(node, ast.Raise) and isinstance(node.exc, ast.Call) and getattr(node.exc.func, "id", None) == "ValidationError"
            for node in ast.walk(handler)
        )
    ]
    assert len(arms) == 1, "expected exactly one arm converting a write failure into a rejection"
    return arms[0]


def _reports_the_fault(source: str) -> bool:
    arm = _converting_arm(source)
    return any(isinstance(node, ast.Call) and _emits_a_traceback(node, arm.name) for node in ast.walk(arm))


_ARM = """
def f():
    try:
        pass
    except Exception as e:
        %s
        raise ValidationError("converted")
"""
_REPORTS = {
    "exception with the caught name": 'logger.exception(msg="x", exc_info=e)',
    "bare exception defaults exc_info to True": 'logger.exception("x")',
    "error with the caught name": 'logger.error("x", exc_info=e)',
    "exc_info=True": 'logger.error("x", exc_info=True)',
}
_SILENT = {
    "exc_info=None": 'logger.exception(msg="x", exc_info=None)',
    "exc_info=False": 'logger.exception(msg="x", exc_info=False)',
    "info naming the exception but attaching nothing": 'logger.info("x: %s", e)',
    "nothing logged": "pass",
}


@pytest.mark.parametrize("shape", sorted(_REPORTS))
def test_the_scanner_sees_every_reporting_shape(shape):
    assert _reports_the_fault(_ARM % _REPORTS[shape]), f"{shape} does render frames and must count as reporting"


@pytest.mark.parametrize("shape", sorted(_SILENT))
def test_the_scanner_rejects_every_silent_shape(shape):
    assert not _reports_the_fault(_ARM % _SILENT[shape]), f"{shape} renders no frames and must not count as reporting"


def test_the_converting_arm_reports_the_original_fault():
    """The frames this route arm leaves to the service must actually be printed there.

    `bulk_close_orders` converts a write failure into `ValidationError`, so the route
    receives a rejection type for a genuine fault. Recording it without frames at the
    route is sound only while the converting arm reports the original -- drop that
    report and the route's one-line rejection becomes the sole record of a DB error.
    """
    assert _reports_the_fault(inspect.getsource(OrderService.bulk_close_orders.__func__)), (
        "The arm converting a write failure into ValidationError must report the "
        "original with a traceback. A falsy exc_info counts as no report: logging "
        "ignores it, so the frames are lost while the keyword is still present."
    )
"#;

    #[test]
    fn extracts_from_a_real_test_module() {
        let docs = extract_docstrings(BAD_EXAMPLE, true).unwrap();
        let report: Vec<(DocKind, String, bool, bool, usize)> = docs
            .iter()
            .map(|d| {
                (
                    d.kind,
                    d.name.clone(),
                    d.is_test,
                    d.only_statement,
                    d.text.split('\n').count(),
                )
            })
            .collect();
        // Sanity check the shape the fixture produces: module doc, then every docstringed
        // function in source order (helpers and pytest-style tests interleaved).
        let names: Vec<&str> = docs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "",
                "_service_rejecting",
                "test_rejection_is_not_logged_as_a_fault",
                "test_rejection_is_recorded_at_info_without_a_traceback",
                "test_an_unexpected_failure_still_reports_a_traceback",
                "_emits_a_traceback",
                "_converting_arm",
                "test_the_converting_arm_reports_the_original_fault",
            ],
            "full report: {report:#?}"
        );

        let module = &docs[0];
        assert_eq!(module.kind, DocKind::Module);
        assert!(module.is_test, "module doc in a test file must be is_test");

        let rejecting = docs
            .iter()
            .find(|d| d.name == "_service_rejecting")
            .unwrap();
        assert_eq!(rejecting.kind, DocKind::Function);
        assert!(!rejecting.is_test, "helper, not a test function");
        assert_eq!(rejecting.decorators, vec!["@contextlib.contextmanager"]);
    }
}
