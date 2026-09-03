use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Python,
    /// Also covers .jsx, .mjs, .cjs (tree-sitter-javascript parses JSX).
    JavaScript,
    TypeScript,
    Tsx,
}

impl Language {
    pub fn from_path(path: &Path) -> Option<Language> {
        match path.extension()?.to_str()? {
            "py" | "pyi" => Some(Language::Python),
            "js" | "jsx" | "mjs" | "cjs" => Some(Language::JavaScript),
            "ts" | "mts" | "cts" => Some(Language::TypeScript),
            "tsx" => Some(Language::Tsx),
            _ => None,
        }
    }

    /// Prefix used when emitting a single-line comment.
    pub fn line_prefix(self) -> &'static str {
        match self {
            Language::Python => "#",
            _ => "//",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentKind {
    /// `# ...` or `// ...`
    Line,
    /// `/* ... */` (JS/TS only)
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

/// Decision for one block.
#[derive(Debug, Clone)]
pub enum Action {
    Keep,
    Delete,
    /// Replace the block with a single-line comment. `prose` is the cleaned comment text to summarize.
    Reduce {
        prose: String,
    },
}

#[derive(Debug, Clone)]
pub struct Config {
    pub mode: Mode,
    /// Blocks with fewer cleaned prose lines than this are kept in reduce mode.
    pub min_lines: usize,
    /// Blocks averaging fewer words per prose line than this are kept in reduce mode (low density).
    pub min_density: f64,
    /// Target maximum words for the one-line summary.
    pub max_summary_words: usize,
    /// OpenAI-compatible base URL. Required in reduce mode; unused in delete mode.
    pub endpoint: String,
    pub model: String,
    pub api_key: Option<String>,
    pub llm_concurrency: usize,
    pub dry_run: bool,
    pub verbose: bool,
}
