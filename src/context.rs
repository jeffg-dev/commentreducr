//! Source context for an item: kind, placement, attached code, enclosing scope and symbol. The
//! rules follow the extractor that built the v2 gold set (see tools/dataset/v2/SCHEMA.md), so
//! `tools/dataset/v2/parity.py` can check them; deviations are marked where they are made.
use crate::docstring::{self, DocKind};
use crate::types::{CommentKind, Language};
use crate::{Item, structural};
use regex::Regex;
use serde::Serialize;
use std::path::Path;
use std::sync::LazyLock;

/// A to-do marker for `comment_kind`. Narrower than the structural keep rule, which also keeps
/// NOTE and any lower-case mention of "todo" in prose. Deviation: `@todo` counts, which the
/// gold extractor (upper case only) missed.
static TODO_KIND_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(TODO|FIXME|XXX|HACK)\b|(?i:@todo\b)").unwrap());

/// Test-runner docblock pragmas: written as `/** */` but they configure the runner.
static PRAGMA_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"@(jest|vitest)-environment\b").unwrap());

/// Node kinds that count as an enclosing scope, and the field that holds the body.
pub(crate) struct ScopeKinds {
    pub kinds: &'static [&'static str],
    pub body_field: &'static str,
}

pub(crate) fn scope_kinds(lang: Language) -> ScopeKinds {
    let kinds: &'static [&'static str] = match lang {
        Language::Python => &["function_definition", "class_definition"],
        Language::JavaScript | Language::TypeScript | Language::Tsx => &[
            "function_declaration",
            "generator_function_declaration",
            "method_definition",
            "arrow_function",
            "function_expression",
            "class_declaration",
            "class",
        ],
        Language::Rust => &[
            "function_item",
            "impl_item",
            "trait_item",
            "struct_item",
            "enum_item",
            "mod_item",
        ],
        Language::Yaml => &[],
    };
    ScopeKinds {
        kinds,
        body_field: "body",
    }
}

/// The enclosing function/class (including its header), or a nearby module neighborhood. Large
/// scopes retain the header and forty lines on either side; the target itself is removed.
pub(crate) fn context_for_range(
    source: &str,
    tree: &tree_sitter::Tree,
    lang: Language,
    start: usize,
    end_byte: usize,
    start_line: usize,
    end_line: usize,
) -> String {
    let kinds = scope_kinds(lang);
    let root = tree.root_node();
    let mut scope = root;
    let mut node = root.descendant_for_byte_range(start, end_byte);
    while let Some(n) = node {
        if kinds.kinds.contains(&n.kind()) {
            scope = n;
            break;
        }
        node = n.parent();
    }
    let at_root = scope.id() == root.id();
    let offsets: Vec<usize> = std::iter::once(0)
        .chain(source.match_indices('\n').map(|(i, _)| i + 1))
        .chain(std::iter::once(source.len()))
        .collect();
    let first = scope.start_position().row;
    let last = scope.end_position().row.min(offsets.len() - 2);
    let large = at_root || last.saturating_sub(first) > 80;
    let from = if large {
        (start_line - 1).saturating_sub(40).max(first)
    } else {
        first
    };
    let to = if large {
        (end_line - 1 + 40).min(last)
    } else {
        last
    };
    let begin = offsets[from];
    let end = offsets[to + 1];
    let mut result = String::new();
    if from > first
        && !at_root
        && let Some(body) = scope.child_by_field_name(kinds.body_field)
    {
        result.push_str(source[scope.start_byte()..body.start_byte()].trim_end());
        result.push('\n');
    }
    result.push_str(&source[begin..start]);
    result.push_str(&source[end_byte..end]);
    result.trim().to_owned()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommentKindV2 {
    Inline,
    ApiDoc,
    Todo,
    Directive,
}

impl CommentKindV2 {
    pub fn as_str(self) -> &'static str {
        match self {
            CommentKindV2::Inline => "inline",
            CommentKindV2::ApiDoc => "api_doc",
            CommentKindV2::Todo => "todo",
            CommentKindV2::Directive => "directive",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    BeforeStatement,
    Trailing,
    FunctionHead,
    ClassHead,
    ModuleHead,
}

#[derive(Debug, Clone, Serialize)]
pub struct Symbol {
    pub name: Option<String>,
    pub public: Option<bool>,
    pub is_test: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ItemContext {
    pub language: &'static str,
    pub comment_kind: CommentKindV2,
    pub placement: Placement,
    /// Raw text with delimiters (docstrings keep prefix and quotes).
    pub comment: String,
    pub annotated_code: String,
    /// 0-based inclusive line range of the annotated code; may contain the comment's own lines.
    pub annotated_lines: Option<(usize, usize)>,
    /// 0-based inclusive line range of the innermost enclosing function-like scope; None at
    /// module level and when the innermost scope is a class, impl, trait, struct, enum or mod.
    pub scope_lines: Option<(usize, usize)>,
    pub enclosing_scope: String,
    pub symbol: Symbol,
}

pub(crate) fn language_name(lang: Language) -> &'static str {
    match lang {
        Language::Python => "python",
        Language::JavaScript => "javascript",
        Language::TypeScript => "typescript",
        Language::Tsx => "tsx",
        Language::Rust => "rust",
        Language::Yaml => "yaml",
    }
}

fn is_comment_node(n: tree_sitter::Node) -> bool {
    n.kind().contains("comment")
}

fn node_text<'a>(src: &'a str, n: tree_sitter::Node) -> &'a str {
    &src[n.start_byte()..n.end_byte()]
}

/// A definition's name; an anonymous function takes the name it is assigned to (`const f = () =>`).
fn def_name(n: tree_sitter::Node, src: &str) -> Option<String> {
    let name = match n.kind() {
        "impl_item" => {
            let t = n.child_by_field_name("type")?;
            if t.kind() == "generic_type" {
                t.child_by_field_name("type")?
            } else {
                t
            }
        }
        "arrow_function" | "function_expression" => n
            .child_by_field_name("name")
            .or_else(|| n.parent()?.child_by_field_name("name"))?,
        _ => n.child_by_field_name("name")?,
    };
    Some(node_text(src, name).to_owned())
}

/// 0-based row and byte column of byte offset `b`.
fn point(src: &str, b: usize) -> (usize, usize) {
    let line_start = src[..b].rfind('\n').map_or(0, |i| i + 1);
    (src[..line_start].matches('\n').count(), b - line_start)
}

/// Rows `lo..hi` of `lines` with the target (from `s` to `e`, row and column) cut out. A row the
/// cut leaves blank is dropped; every other row, blank or not, is kept verbatim.
fn rows_without(
    lines: &[&str],
    lo: usize,
    hi: usize,
    s: (usize, usize),
    e: (usize, usize),
) -> String {
    let mut out = Vec::new();
    for (row, line) in lines.iter().enumerate().take(hi).skip(lo) {
        if (s.0..=e.0).contains(&row) {
            let prefix = if row == s.0 {
                line.get(..s.1).unwrap_or(line)
            } else {
                ""
            };
            let suffix = if row == e.0 {
                line.get(e.1..).unwrap_or("")
            } else {
                ""
            };
            if (prefix.to_owned() + suffix).trim().is_empty() {
                continue;
            }
            out.push(prefix.to_owned() + suffix);
        } else {
            out.push((*line).to_owned());
        }
    }
    out.join("\n")
}

/// The first named, non-comment node in tree order that starts at or after byte `after` and on a
/// row below `row`: the outermost statement or definition following a comment.
fn following(n: tree_sitter::Node, after: usize, row: usize) -> Option<tree_sitter::Node> {
    if n.is_named()
        && !is_comment_node(n)
        && n.start_byte() >= after
        && n.start_position().row > row
    {
        return Some(n);
    }
    if n.end_byte() <= after {
        return None;
    }
    let mut c = n.walk();
    n.children(&mut c).find_map(|k| following(k, after, row))
}

const JS_FUNCTIONS: &[&str] = &[
    "function_declaration",
    "generator_function_declaration",
    "method_definition",
    "arrow_function",
    "function_expression",
];
const JS_CLASSES: &[&str] = &[
    "class_declaration",
    "interface_declaration",
    "enum_declaration",
];

fn is_js_def(kind: &str) -> bool {
    JS_FUNCTIONS.contains(&kind) || JS_CLASSES.contains(&kind) || kind == "type_alias_declaration"
}

/// Named nodes starting on `row`, in tree order.
fn starting_on<'t>(n: tree_sitter::Node<'t>, row: usize, out: &mut Vec<tree_sitter::Node<'t>>) {
    if n.start_position().row > row || n.end_position().row < row {
        return;
    }
    if n.start_position().row == row {
        out.push(n);
    }
    let mut c = n.walk();
    for k in n.named_children(&mut c) {
        starting_on(k, row, out);
    }
}

/// The JS/TS definition a doc block documents: one starting on `row`, else one inside an
/// `export`/`const` wrapper starting on that row (at most one row further down).
fn js_documented(root: tree_sitter::Node, row: usize) -> Option<tree_sitter::Node> {
    let mut on_row = Vec::new();
    starting_on(root, row, &mut on_row);
    if let Some(d) = on_row.iter().find(|n| is_js_def(n.kind())) {
        return Some(*d);
    }
    on_row
        .iter()
        .filter(|n| {
            matches!(
                n.kind(),
                "export_statement" | "lexical_declaration" | "variable_declarator"
            )
        })
        .find_map(|w| {
            let mut inside = Vec::new();
            for r in row..=row + 1 {
                starting_on(*w, r, &mut inside);
            }
            inside.sort_by_key(|n| n.start_byte());
            inside.into_iter().find(|n| is_js_def(n.kind()))
        })
}

/// Exported, not `#private`/`private`/`protected`, and not inside a function body. Deviation: the
/// gold extractor meant to exclude `private`/`protected` too but its check never matched.
fn js_public(n: tree_sitter::Node, src: &str) -> bool {
    let mut c = n.walk();
    if def_name(n, src).is_some_and(|s| s.starts_with('#'))
        || n.children(&mut c)
            .any(|k| k.kind() == "accessibility_modifier" && node_text(src, k) != "public")
    {
        return false;
    }
    std::iter::successors(n.parent(), |p| p.parent())
        .take_while(|a| !JS_FUNCTIONS.contains(&a.kind()))
        .any(|a| a.kind() == "export_statement")
}

/// Head placement, symbol name and visibility for a JS/TS doc block, from what it documents.
fn js_doc_target(
    root: tree_sitter::Node,
    src: &str,
    row: usize,
) -> Option<(Placement, String, bool)> {
    let n = js_documented(root, row)?;
    def_name(n, src)?;
    let mut names: Vec<String> = std::iter::successors(Some(n), |p| p.parent())
        .filter(|a| is_js_def(a.kind()))
        .filter_map(|a| def_name(a, src))
        .collect();
    names.reverse();
    let placement = if JS_FUNCTIONS.contains(&n.kind()) {
        Placement::FunctionHead
    } else if JS_CLASSES.contains(&n.kind()) {
        Placement::ClassHead
    } else {
        Placement::BeforeStatement
    };
    Some((placement, names.join("."), js_public(n, src)))
}

/// Paths that hold tests: a `test`/`tests`/`__tests__`/`playwright` directory, a Python test module,
/// or a `.test.`/`.spec.` (or `_test.`) file name.
fn is_test_path(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    docstring::is_test_file(path)
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::Normal(s) if s == "__tests__" || s == "playwright"))
        || [".test.", ".spec.", "_test.", "_spec."]
            .iter()
            .any(|m| name.contains(m))
}

pub(crate) fn item_context(
    source: &str,
    tree: &tree_sitter::Tree,
    lang: Language,
    path: &Path,
    item: &Item,
) -> ItemContext {
    let (comment, mut placement, comment_kind, own_line) = match item {
        Item::Comment(b) => {
            let text = item.text();
            let kind = if PRAGMA_RE.is_match(&text) {
                CommentKindV2::Directive
            } else if b
                .comments
                .iter()
                .any(|c| c.doc || (c.kind == CommentKind::Block && c.text.starts_with("/**")))
            {
                CommentKindV2::ApiDoc
            } else if TODO_KIND_RE.is_match(&text) {
                CommentKindV2::Todo
            } else if structural::is_directive(b, lang, source) {
                CommentKindV2::Directive
            } else {
                CommentKindV2::Inline
            };
            let placement = if b.own_line {
                Placement::BeforeStatement
            } else {
                Placement::Trailing
            };
            (text, placement, kind, b.own_line)
        }
        Item::Docstring(d) => (
            source[d.start..d.end].to_owned(),
            match d.kind {
                DocKind::Module => Placement::ModuleHead,
                DocKind::Class => Placement::ClassHead,
                DocKind::Function => Placement::FunctionHead,
            },
            CommentKindV2::ApiDoc,
            true,
        ),
    };

    // The target's first and last token: its start and end points, and the node to search from.
    let ((first_start, first_end), (last_end, _)) = match item {
        Item::Comment(b) => {
            let f = b
                .comments
                .first()
                .map_or((b.start, b.end), |c| (c.start, c.end));
            let l = b.comments.last().map_or((b.end, b.end), |c| (c.end, c.end));
            (f, l)
        }
        Item::Docstring(d) => ((d.start, d.end), (d.end, d.end)),
    };
    let root = tree.root_node();
    let lines: Vec<&str> = source.lines().collect();
    let s = point(source, first_start);
    let mut e = point(source, last_end);
    // A token that swallows its newline ends at column 0 of the next row.
    if e.1 == 0 && e.0 > s.0 {
        e.0 -= 1;
        e.1 = lines.get(e.0).map_or(0, |l| l.len());
    }

    let kinds = scope_kinds(lang).kinds;
    let node = root.descendant_for_byte_range(first_start, first_end);
    let scope = std::iter::successors(node.and_then(|n| n.parent()), |p| p.parent())
        .find(|n| kinds.contains(&n.kind()));
    let mut names: Vec<String> = std::iter::successors(scope, |p| p.parent())
        .filter(|n| kinds.contains(&n.kind()))
        .filter_map(|n| def_name(n, source))
        .collect();
    names.reverse();
    let mut name = (!names.is_empty()).then(|| names.join("."));
    let mut public = match lang {
        Language::Python | Language::Yaml => names.last().is_some_and(|n| !n.starts_with('_')),
        Language::JavaScript | Language::TypeScript | Language::Tsx => scope.is_some_and(|s| {
            std::iter::successors(Some(s), |p| p.parent()).any(|a| a.kind() == "export_statement")
        }),
        Language::Rust => scope.is_some_and(|n| {
            let mut c = n.walk();
            n.children(&mut c)
                .any(|k| k.kind() == "visibility_modifier")
        }),
    };

    // The attached code, as a half-open row range [alo, ahi).
    let n_lines = lines.len();
    let (alo, mut ahi) = if matches!(item, Item::Docstring(_)) {
        // Signature (or anything above a module docstring) plus up to ten rows after the docstring.
        // Deviation: stop at the definition's end; the gold extractor ran on into the next one.
        (
            scope.map_or(0, |n| n.start_position().row),
            (e.0 + 11)
                .min(n_lines)
                .min(scope.map_or(n_lines, |n| n.end_position().row + 1)),
        )
    } else if !own_line {
        (s.0, e.0 + 1)
    } else {
        match following(root, last_end, e.0) {
            Some(f) => {
                let alo = f.start_position().row;
                let mut ahi = f.end_position().row + 1;
                if comment_kind == CommentKindV2::ApiDoc {
                    ahi = ahi.min(alo + 11);
                }
                (alo, ahi)
            }
            None => (e.0 + 1, e.0 + 1),
        }
    };
    if own_line && matches!(item, Item::Comment(_)) && scope.is_none() && s.0 < 5 {
        placement = Placement::ModuleHead;
        ahi = (alo + 20).min(n_lines);
    }
    // shortcut: very long statements are cut to 60 rows (the dataset drops them), so the scope
    // window below still holds the comment.
    ahi = ahi.min(alo + 60);
    if comment_kind == CommentKindV2::ApiDoc
        && matches!(item, Item::Comment(_))
        && matches!(
            lang,
            Language::JavaScript | Language::TypeScript | Language::Tsx
        )
        && let Some((p, n, public_)) = js_doc_target(root, source, alo)
    {
        placement = p;
        name = Some(n);
        public = public_;
    }

    // The enclosing scope (or forty rows either side at top level), widened to hold the attached
    // code and cut to 80 rows around the comment.
    let mut slo = scope.map_or(s.0.saturating_sub(40), |n| n.start_position().row);
    let mut shi = scope.map_or((e.0 + 41).min(n_lines), |n| n.end_position().row + 1);
    slo = slo.min(alo);
    shi = shi.max(ahi);
    if shi - slo > 80 {
        slo = slo.max((s.0.saturating_sub(39)).min(alo));
        shi = shi.min(slo + 80);
        if ahi > shi {
            shi = ahi;
            slo = shi.saturating_sub(80);
        }
    }

    ItemContext {
        language: language_name(lang),
        comment_kind,
        placement,
        comment,
        annotated_code: rows_without(&lines, alo, ahi, s, e),
        annotated_lines: (ahi > alo).then(|| (alo, ahi - 1)),
        // Class, impl and mod scopes span most of a file; their comments reach tier 2 through
        // `annotated_lines` instead.
        scope_lines: scope
            .filter(|n| {
                JS_FUNCTIONS.contains(&n.kind())
                    || n.kind() == "function_definition"
                    || n.kind() == "function_item"
            })
            .map(|n| (n.start_position().row, n.end_position().row)),
        enclosing_scope: rows_without(&lines, slo, shi, s, e),
        symbol: Symbol {
            name,
            public: Some(public),
            // Beyond the gold rule (path only): a test*/Test* definition is a test anywhere.
            is_test: is_test_path(path)
                || names
                    .last()
                    .map(String::as_str)
                    .is_some_and(|l| l.starts_with("test") || l.starts_with("Test")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{docstring, parse};

    // The pre-move implementation, kept to prove Python output is byte-identical.
    fn old_context_for_range(
        source: &str,
        tree: &tree_sitter::Tree,
        start: usize,
        end_byte: usize,
        start_line: usize,
        end_line: usize,
    ) -> String {
        let root = tree.root_node();
        let mut scope = root;
        let mut node = root.descendant_for_byte_range(start, end_byte);
        while let Some(n) = node {
            if matches!(n.kind(), "function_definition" | "class_definition") {
                scope = n;
                break;
            }
            node = n.parent();
        }
        let offsets: Vec<usize> = std::iter::once(0)
            .chain(source.match_indices('\n').map(|(i, _)| i + 1))
            .chain(std::iter::once(source.len()))
            .collect();
        let first = scope.start_position().row;
        let last = scope.end_position().row.min(offsets.len() - 2);
        let large = scope.kind() == "module" || last.saturating_sub(first) > 80;
        let from = if large {
            (start_line - 1).saturating_sub(40).max(first)
        } else {
            first
        };
        let to = if large {
            (end_line - 1 + 40).min(last)
        } else {
            last
        };
        let begin = offsets[from];
        let end = offsets[to + 1];
        let mut result = String::new();
        if from > first
            && scope.kind() != "module"
            && let Some(body) = scope.child_by_field_name("body")
        {
            result.push_str(source[scope.start_byte()..body.start_byte()].trim_end());
            result.push('\n');
        }
        result.push_str(&source[begin..start]);
        result.push_str(&source[end_byte..end]);
        result.trim().to_owned()
    }

    #[test]
    fn python_context_unchanged() {
        for name in ["sample.py", "test_sample.py"] {
            let path = std::path::Path::new("tests/fixtures").join(name);
            let src = std::fs::read_to_string(&path).unwrap();
            let tree = parse::parse_tree(&src, Language::Python).unwrap();
            let mut ranges: Vec<_> = parse::group_blocks(
                &src,
                parse::extract_comments(&src, Language::Python).unwrap(),
            )
            .iter()
            .map(|b| (b.start, b.end, b.start_line + 1, b.end_line + 1))
            .collect();
            ranges.extend(
                docstring::extract_docstrings(&src, docstring::is_test_file(&path), &[])
                    .unwrap()
                    .iter()
                    .map(|d| (d.start, d.end, d.start_line + 1, d.end_line + 1)),
            );
            assert!(!ranges.is_empty());
            for (s, e, sl, el) in ranges {
                assert_eq!(
                    old_context_for_range(&src, &tree, s, e, sl, el),
                    context_for_range(&src, &tree, Language::Python, s, e, sl, el),
                    "{name} {s}..{e}"
                );
            }
        }
    }

    #[test]
    fn fixtures_match() {
        for name in [
            "sample.py",
            "scopes.py",
            "sample.js",
            "sample.ts",
            "sample.tsx",
            "sample.rs",
            "sample.yaml",
        ] {
            let dir = std::path::Path::new("tests/fixtures/context");
            let src = std::fs::read_to_string(dir.join(name)).unwrap();
            let lang = Language::from_path(Path::new(name)).unwrap();
            let tree = parse::parse_tree(&src, lang).unwrap();
            let mut items: Vec<Item> =
                parse::group_blocks(&src, parse::extract_comments(&src, lang).unwrap())
                    .into_iter()
                    .map(Item::Comment)
                    .collect();
            if lang == Language::Python {
                items.extend(
                    docstring::extract_docstrings(&src, false, &[])
                        .unwrap()
                        .into_iter()
                        .map(Item::Docstring),
                );
            }
            items.sort_by_key(|i| i.range().0);
            // A synthetic path: the fixture's real path sits under `tests/`, which would mark every item as a test.
            let got: Vec<_> = items
                .iter()
                .map(|i| item_context(&src, &tree, lang, Path::new(name), i))
                .collect();
            let got = serde_json::to_string_pretty(&got).unwrap() + "\n";
            let json = dir.join(format!("{name}.json"));
            if std::env::var_os("BLESS").is_some() {
                std::fs::write(&json, &got).unwrap();
            }
            assert_eq!(got, std::fs::read_to_string(&json).unwrap(), "{name}");
        }
    }
}
