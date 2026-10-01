use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Python,
    /// Also covers .jsx, .mjs, .cjs (tree-sitter-javascript parses JSX).
    JavaScript,
    TypeScript,
    Tsx,
    Yaml,
    Rust,
}

impl Language {
    pub fn from_path(path: &Path) -> Option<Language> {
        match path.extension()?.to_str()? {
            "py" | "pyi" => Some(Language::Python),
            "js" | "jsx" | "mjs" | "cjs" => Some(Language::JavaScript),
            "ts" | "mts" | "cts" => Some(Language::TypeScript),
            "tsx" => Some(Language::Tsx),
            "yml" | "yaml" => Some(Language::Yaml),
            "rs" => Some(Language::Rust),
            _ => None,
        }
    }

    /// Prefix used when emitting a single-line comment.
    pub fn line_prefix(self) -> &'static str {
        match self {
            Language::Python | Language::Yaml => "#",
            _ => "//",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentKind {
    /// `# ...` or `// ...`
    Line,
    /// `/* ... */` (JS/TS and Rust)
    Block,
}

/// One comment token as found by the parser. Byte offsets are into the file source.
#[derive(Debug, Clone)]
pub struct Comment {
    pub start: usize,
    pub end: usize,
    pub kind: CommentKind,
    /// Raw text including delimiters.
    pub text: String,
    /// 0-based line numbers.
    pub start_line: usize,
    pub end_line: usize,
    /// Only whitespace precedes the comment on its first line.
    pub own_line: bool,
    /// Non-whitespace code follows the comment on its last line (e.g. `foo(/* x */ 1)` or `/* x */ let y;`).
    pub code_after: bool,
    /// A Rust doc comment (`///`, `//!`, `/** */`, `/*! */`): a `#[doc]` attribute, not a comment.
    pub doc: bool,
}

/// A group of comments treated as one unit: either a single Block comment, a single trailing/inline
/// comment, or a run of own-line Line comments on consecutive lines with identical indentation.
#[derive(Debug, Clone)]
pub struct CommentBlock {
    pub comments: Vec<Comment>,
    /// Byte range covering the first comment start to the last comment end.
    pub start: usize,
    pub end: usize,
    pub start_line: usize,
    pub end_line: usize,
    /// Leading whitespace of the first comment's line (used when emitting a replacement).
    pub indent: String,
    pub own_line: bool,
    pub code_after: bool,
    pub kind: CommentKind,
}

impl CommentBlock {
    pub fn line_count(&self) -> usize {
        self.end_line - self.start_line + 1
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Reduce,
    Delete,
}

/// What a run processes: comments (all supported languages) or Python docstrings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Target {
    Comments,
    Docstrings,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Languages {
    Python,
    #[value(name = "typescript")]
    TypeScript,
    Rust,
    All,
}

impl Languages {
    pub fn matches(self, language: Language) -> bool {
        match self {
            Self::Python => language == Language::Python,
            Self::TypeScript => matches!(language, Language::TypeScript | Language::Tsx),
            Self::Rust => language == Language::Rust,
            Self::All => true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub target: Target,
    pub language: Languages,
    pub mode: Mode,
    /// Target maximum words for the one-line summary.
    pub max_summary_words: usize,
    /// OpenAI-compatible base URL. Required in reduce mode; unused in delete mode.
    pub endpoint: String,
    pub model: String,
    pub docstrings_model: String,
    pub api_key: Option<String>,
    pub llm_concurrency: usize,
    pub dry_run: bool,
    pub verbose: bool,
    pub database: Option<PathBuf>,
    /// gitignore-syntax patterns for files to skip, on top of `git ls-files`'s own tracked set
    /// and the repo's gitignore rules (see `files::tracked_source_files`). Shipped default plus
    /// whatever the user's config file appends.
    pub ignore: Vec<String>,
    /// Decorator names (see `docstring::name_matches`) whose docstrings are never touched
    /// (Strands `@tool`, click/typer commands). Shipped default plus whatever the user's config
    /// file appends.
    pub keep_decorators: Vec<String>,
    /// Base-class names (see `docstring::name_matches`) whose class docstrings, and same-file
    /// subclasses' docstrings, are never touched (dspy.Signature, pydantic.BaseModel). Shipped
    /// default plus whatever the user's config file appends.
    pub keep_bases: Vec<String>,
}
