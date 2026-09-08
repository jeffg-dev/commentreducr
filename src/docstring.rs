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
use crate::rewrite::{Edit, line_end, line_end_incl_terminator, line_start};
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
    /// Text from the start of the docstring's line up to its opening quote: pure whitespace when
    /// `own_line`, otherwise it also holds the preceding code (e.g. `def probe(self): ` for an
    /// inline docstring). Only reused as an indent by `replace_edit`'s multi-line branch, which
    /// refuses to run at all when `!own_line`.
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
    // A leading UTF-8 BOM is not whitespace by Unicode's White_Space property, but it is not
    // code either: strip it before judging what (if anything) precedes the docstring on its
    // line, so a BOM'd file-initial module docstring is still recognized as own_line.
    let before = src[ls..start].trim_start_matches('\u{feff}');
    let own_line = before.chars().all(char::is_whitespace);
    let le = line_end(src, end);
    // After a complete string statement a `#` can only start a comment, which goes with the
    // docstring (`"""  # noqa`), so it does not count as code.
    let rest = src[end..le].trim();
    let code_after = !rest.is_empty() && !rest.starts_with('#');
    let indent = before.to_string();

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

    let start = doc.start - doc.indent.len();
    let last_pos = doc.end.saturating_sub(1).max(doc.start);
    let mut end = line_end_incl_terminator(src, last_pos);

    // Unlike rewrite::delete_edit, there is no EOF-without-terminator case to handle here: an
    // only_statement=false docstring always has a later sibling statement in the same
    // module/class/function body, positioned after it in the source, so its own line is
    // guaranteed a real terminator before EOF -- the one shape that could put that sibling on
    // the docstring's own last line (`code_after`) is already refused above.
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

    Some(Edit {
        start,
        end,
        replacement: String::new(),
    })
}

/// Edit that replaces `doc`'s text with `new_text` (lines already joined with "\n", already
/// trimmed), keeping the original prefix and quote. `None` if `new_text` is empty or would be
/// unsafe to splice in verbatim: it contains the quote sequence itself, a backslash, or ends in
/// the quote's delimiter character (which would combine with the closing quote we append and
/// close the string early, e.g. a reply ending `raw"` next to a `"""` docstring). `None` too for
/// a multi-line reply when the docstring is not `own_line`: there is no whitespace-only indent
/// to reuse for its continuation/closing lines (code shares the line instead, e.g.
/// `def probe(self): """x"""`), so splicing one in would duplicate that code into the text.
pub fn replace_edit(_src: &str, doc: &Docstring, new_text: &str) -> Option<Edit> {
    let quote_char = doc.quote.chars().next().unwrap_or_default();
    if new_text.is_empty()
        || new_text.contains(doc.quote.as_str())
        || new_text.contains('\\')
        || new_text.ends_with(quote_char)
    {
        return None;
    }

    let doc_lines: Vec<&str> = new_text.split('\n').collect();
    if doc_lines.len() > 1 && !doc.own_line {
        return None;
    }

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
    fn test_prefixed_class_docstring_is_test() {
        let src = "class TestFoo:\n    \"\"\"Holds a couple of test methods.\"\"\"\n\n    def test_one(self):\n        pass\n";
        let docs = extract_docstrings(src, false).unwrap();
        let foo = by_name(&docs, "TestFoo");
        assert_eq!(foo.kind, DocKind::Class);
        assert!(
            foo.is_test,
            "a Test-prefixed class docstring must be is_test"
        );
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
    fn bom_before_module_docstring_does_not_defeat_own_line() {
        let src = "\u{feff}\"\"\"Module doc with BOM.\"\"\"\n\nimport os\n";
        let docs = extract_docstrings(src, false).unwrap();
        assert_eq!(docs.len(), 1);
        assert!(
            docs[0].own_line,
            "a leading BOM must not read as code preceding the docstring"
        );
        assert!(!docs[0].only_statement, "import os follows it");
        let edit = delete_edit(src, &docs[0]).unwrap();
        let out = rewrite::apply(src, vec![edit]);
        // The BOM itself survives byte-for-byte; only the docstring (and the blank line after
        // it) is removed.
        assert_eq!(out, "\u{feff}import os\n");
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
    fn delete_edit_leaves_code_after_untouched() {
        let src = "def g(): \"\"\"x\"\"\"; y = 1\n";
        let docs = extract_docstrings(src, false).unwrap();
        assert_eq!(docs.len(), 1);
        assert!(!docs[0].only_statement);
        assert!(delete_edit(src, &docs[0]).is_none());

        // A trailing comment on the closing line is not code: the docstring goes, comment and all.
        let src = "\"\"\"Doc.\n\"\"\"  # noqa\nimport os\n";
        let docs = extract_docstrings(src, false).unwrap();
        assert!(!docs[0].code_after);
        let out = rewrite::apply(src, vec![delete_edit(src, &docs[0]).unwrap()]);
        assert_eq!(out, "import os\n");
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
    fn replace_edit_refuses_new_text_ending_in_the_quote_char() {
        // A reply ending in a single quote char would combine with the closing delimiter we
        // append right after it, forming a longer run than intended and closing the string
        // early -- leaving a dangling extra quote character the tokenizer never expects.
        let src = "def f():\n    \"\"\"old\"\"\"\n";
        let docs = extract_docstrings(src, false).unwrap();
        assert!(replace_edit(src, &docs[0], "Ends with quote\"").is_none());

        let src = "def f():\n    '''old'''\n";
        let docs = extract_docstrings(src, false).unwrap();
        assert!(replace_edit(src, &docs[0], "Uses the value 'raw'").is_none());
    }

    #[test]
    fn replace_edit_refuses_multiline_when_docstring_shares_its_line_with_code() {
        // No whitespace-only indent exists to reuse for continuation/closing lines when code
        // (`def probe(self): `) precedes the docstring on its own line; splicing a multi-line
        // reply in would duplicate that code into the docstring's text.
        let src = "class C:\n    def probe(self): \"\"\"old\"\"\"\n";
        let docs = extract_docstrings(src, false).unwrap();
        assert!(!docs[0].own_line);
        assert!(replace_edit(src, &docs[0], "First line.\nSecond line.").is_none());
        // A single-line reply is unaffected: nothing needs the indent.
        assert!(replace_edit(src, &docs[0], "One line only").is_some());
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

    // A small realistic pytest module (paraphrased, not copied verbatim, from a real one) rather
    // than a hand-rolled fixture. Everything about extraction's shape (decorators, source-order
    // interleaving, is_test from a name) is already covered by extraction_covers_every_shape;
    // the one property that isn't is a module docstring's is_test coming from in_test_file, so
    // that's the one this checks.
    const BAD_EXAMPLE: &str = r#""""A rejected bulk-close is a 400, not a traceback in the logs."""

import contextlib


@contextlib.contextmanager
def _service_rejecting(reason):
    """POSTs a bulk-close whose service call raises, yielding the response."""
    yield reason


def test_rejection_is_recorded_at_info():
    """Guards the log-level contract these assertions pin."""
    assert True
"#;

    #[test]
    fn extracts_from_a_real_test_module() {
        let docs = extract_docstrings(BAD_EXAMPLE, true).unwrap();
        let names: Vec<&str> = docs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "",
                "_service_rejecting",
                "test_rejection_is_recorded_at_info"
            ]
        );

        let module = &docs[0];
        assert_eq!(module.kind, DocKind::Module);
        assert!(module.is_test, "module doc in a test file must be is_test");

        let rejecting = docs
            .iter()
            .find(|d| d.name == "_service_rejecting")
            .unwrap();
        assert!(!rejecting.is_test, "helper, not a test function");
    }
}
