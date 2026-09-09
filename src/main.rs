use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use commentreducr::{Config, Mode, Target, run};
use std::path::{Path, PathBuf};

/// Every default the CLI ships with, in the same flat `key = value` grammar `parse_config`
/// reads a user's config file with; see the README config section. `[default: N]` in the clap
/// help strings below are literal copies of these values, checked against them by
/// `embedded_defaults_match_help_text`.
const DEFAULT_CONFIG_TOML: &str = include_str!("default_config.toml");

const AFTER_HELP: &str =
    "Advanced options: --help. Settings file: ~/.config/commentreducr/config.toml";
const AFTER_LONG_HELP: &str = "endpoint, model, docstrings_model, api_key, workers, min_lines, \
                               min_density, max_words, ignore, keep_decorators and keep_bases \
                               can also be set in ~/.config/commentreducr/config.toml; flags \
                               win.";

/// Delete or reduce comments (Python, JS/TS, YAML) or Python docstrings in git-tracked files.
#[derive(Parser, Debug)]
#[command(name = "commentreducr", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Comments in Python, JS/TS and YAML files
    #[command(after_help = AFTER_HELP, after_long_help = AFTER_LONG_HELP)]
    Comments(Opts),
    /// Python docstrings (module, class, function)
    #[command(after_help = AFTER_HELP, after_long_help = AFTER_LONG_HELP)]
    Docstrings(Opts),
}

/// Field order is help order: common flags, then the "LLM" heading, then "Advanced" (long
/// help only). Numeric flags are `Option` so the config file can supply them when absent.
#[derive(clap::Args, Debug)]
struct Opts {
    /// Directory or file to process [default: .]
    path: Option<PathBuf>,

    /// Delete all non-structural comments/docstrings.
    #[arg(long)]
    delete: bool,

    /// Reduce large dense blocks to one line/short text via the LLM (default).
    #[arg(long, conflicts_with = "delete")]
    reduce: bool,

    /// With --delete only: report what would change without writing.
    #[arg(long, requires = "delete")]
    dry_run: bool,

    /// Worker threads (and max in-flight LLM requests) [default: 8]
    #[arg(short = 'n', long, alias = "concurrency", value_name = "N")]
    workers: Option<usize>,

    /// Print every changed block, not just per-file summaries.
    #[arg(short, long)]
    verbose: bool,

    /// Config file [default: ~/.config/commentreducr/config.toml]
    #[arg(
        long,
        value_name = "FILE",
        default_value_os_t = default_config_path(),
        hide_default_value = true
    )]
    config: PathBuf,

    /// OpenAI-compatible base URL [default: http://localhost:8000/v1]
    #[arg(long, help_heading = "LLM")]
    endpoint: Option<String>,

    /// Model name
    #[arg(
        long,
        help_heading = "LLM",
        long_help = "Model name [default: gemma-4-e2b-it-4bit for comments, \
                      gemma-4-26b-a4b-it-4bit for docstrings]"
    )]
    model: Option<String>,

    /// API key, if the endpoint needs one
    #[arg(long, help_heading = "LLM")]
    api_key: Option<String>,

    /// Minimum lines in a block before it is reduced [default: 4]
    #[arg(
        long,
        value_name = "N",
        help_heading = "Advanced",
        hide_short_help = true
    )]
    min_lines: Option<usize>,

    /// (comments only) Minimum average words per line [default: 5]
    #[arg(
        long,
        value_name = "N",
        help_heading = "Advanced",
        hide_short_help = true
    )]
    min_density: Option<f64>,

    /// (comments only) Target max words in a summary [default: 20]
    #[arg(
        long,
        value_name = "N",
        help_heading = "Advanced",
        hide_short_help = true
    )]
    max_words: Option<usize>,

    /// Parse only, no LLM, no writes: report files that fail to parse
    #[arg(
        long,
        conflicts_with = "eval",
        help_heading = "Advanced",
        hide_short_help = true,
        long_help = "Parse only, no LLM, no writes: print a redacted report of every file that \
                      fails to parse (node kinds and line shapes, no paths or code) for pasting \
                      into a bug report"
    )]
    diagnose: bool,

    /// Score the LLM prompt against a labeled JSONL dataset instead of processing files
    #[arg(
        long,
        value_name = "JSONL",
        help_heading = "Advanced",
        hide_short_help = true
    )]
    eval: Option<PathBuf>,
}

/// Optional settings from the config file; flags override these.
#[derive(Default)]
struct FileConfig {
    endpoint: Option<String>,
    model: Option<String>,
    /// Model for the docstrings subcommand; falls back to `model`, then the built-in default.
    docstrings_model: Option<String>,
    api_key: Option<String>,
    workers: Option<usize>,
    min_lines: Option<usize>,
    min_density: Option<f64>,
    max_words: Option<usize>,
    /// gitignore-syntax patterns; merged with the shipped default by `merge_list`, not replacing
    /// it (see `assign_config_value`'s doc comment).
    ignore: Option<Vec<String>>,
    /// Decorator names whose docstrings are never touched; merged with the shipped default like
    /// `ignore`.
    keep_decorators: Option<Vec<String>>,
    /// Base-class names whose class docstrings are never touched; merged with the shipped
    /// default like `ignore`.
    keep_bases: Option<Vec<String>>,
}

/// One value parsed from a config line: a quoted string, a bare token kept as text so the caller
/// can parse it as the field's own type (`usize` vs `f64`), or a `["a", "b"]` string array.
enum ConfigValue {
    Str(String),
    Num(String),
    List(Vec<String>),
}

/// Parses the flat `key = value` subset of TOML documented in the README: one statement per
/// line, no sections, no multi-line values. A value is a quoted string, a bare number, or a
/// single-line `["a", "b"]` string array (no nested arrays, no non-string items). Unknown keys
/// are ignored, matching the old serde-based parser, so a config written for a newer version
/// still loads.
fn parse_config(text: &str) -> Result<FileConfig> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut cfg = FileConfig::default();
    let mut seen = std::collections::HashSet::new();
    for (i, raw_line) in text.lines().enumerate() {
        let lineno = i + 1;
        let line = raw_line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            anyhow::bail!("line {lineno}: sections are not supported");
        }
        let Some(eq) = line.find('=') else {
            anyhow::bail!("line {lineno}: expected key = value");
        };
        let key = line[..eq].trim_end();
        if key.is_empty()
            || !key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            anyhow::bail!("line {lineno}: expected key = value");
        }
        if !seen.insert(key.to_string()) {
            anyhow::bail!("line {lineno}: duplicate key {key}");
        }
        let (value, rest) = parse_config_value(line[eq + 1..].trim_start(), lineno)?;
        let rest = rest.trim_start();
        if !rest.is_empty() && !rest.starts_with('#') {
            anyhow::bail!("line {lineno}: unexpected trailing content");
        }
        assign_config_value(&mut cfg, key, value, lineno)?;
    }
    Ok(cfg)
}

/// Parses one value (a quoted string, a bare number, or a `[...]` string array) from the start
/// of `s`, returning it along with whatever text follows it on the line.
fn parse_config_value(s: &str, lineno: usize) -> Result<(ConfigValue, &str)> {
    if let Some(rest) = s.strip_prefix('[') {
        let (items, rest) = parse_config_list(rest, lineno)?;
        return Ok((ConfigValue::List(items), rest));
    }
    let Some(rest) = s.strip_prefix('"') else {
        // Bare token up to whitespace or a comment; the field's own type parses it.
        let end = s
            .find(|c: char| c.is_whitespace() || c == '#')
            .unwrap_or(s.len());
        if end == 0 {
            anyhow::bail!("line {lineno}: expected key = value");
        }
        return Ok((ConfigValue::Num(s[..end].to_string()), &s[end..]));
    };
    let (str_val, rest) = parse_quoted_string(rest, lineno)?;
    Ok((ConfigValue::Str(str_val), rest))
}

/// Parses a `"..."` string starting just after the opening quote (already stripped by the
/// caller), returning the unescaped text and whatever follows the closing quote.
fn parse_quoted_string(rest: &str, lineno: usize) -> Result<(String, &str)> {
    let mut out = String::new();
    let mut chars = rest.char_indices();
    while let Some((idx, c)) = chars.next() {
        match c {
            '"' => return Ok((out, &rest[idx + 1..])),
            '\\' => match chars.next() {
                Some((_, '\\')) => out.push('\\'),
                Some((_, '"')) => out.push('"'),
                Some((_, 'n')) => out.push('\n'),
                Some((_, 't')) => out.push('\t'),
                Some((_, 'r')) => out.push('\r'),
                Some((_, other)) => anyhow::bail!("line {lineno}: invalid escape \\{other}"),
                None => anyhow::bail!("line {lineno}: unterminated string"),
            },
            other => out.push(other),
        }
    }
    anyhow::bail!("line {lineno}: unterminated string")
}

/// Parses a single-line `"a", "b"]` item list (the `[` already stripped by the caller) up to and
/// including its closing `]`, returning the items and whatever follows on the line. Whitespace
/// around items and commas is ignored. Every item must be a quoted string; a nested `[` or any
/// other token is rejected, and running off the end of the line (a multi-line array) surfaces as
/// the same "expected ',' or ']'" error.
fn parse_config_list(s: &str, lineno: usize) -> Result<(Vec<String>, &str)> {
    let mut items = Vec::new();
    let mut s = s.trim_start();
    if let Some(rest) = s.strip_prefix(']') {
        return Ok((items, rest));
    }
    loop {
        s = s.trim_start();
        let Some(rest) = s.strip_prefix('"') else {
            if s.starts_with('[') {
                anyhow::bail!("line {lineno}: nested arrays are not supported");
            }
            anyhow::bail!("line {lineno}: array items must be strings");
        };
        let (item, rest) = parse_quoted_string(rest, lineno)?;
        items.push(item);
        s = rest.trim_start();
        match s.chars().next() {
            Some(',') => {
                s = &s[1..];
            }
            Some(']') => return Ok((items, &s[1..])),
            _ => anyhow::bail!("line {lineno}: expected ',' or ']' in array"),
        }
    }
}

/// Applies one parsed value to the matching known field; unknown keys are ignored.
fn assign_config_value(
    cfg: &mut FileConfig,
    key: &str,
    value: ConfigValue,
    lineno: usize,
) -> Result<()> {
    fn as_str(key: &str, value: ConfigValue, lineno: usize) -> Result<String> {
        match value {
            ConfigValue::Str(s) => Ok(s),
            _ => anyhow::bail!("line {lineno}: {key} expects a string"),
        }
    }
    fn as_usize(key: &str, value: ConfigValue, lineno: usize) -> Result<usize> {
        match value {
            ConfigValue::Num(n) => n
                .parse()
                .map_err(|_| anyhow::anyhow!("line {lineno}: {key} expects a whole number")),
            _ => anyhow::bail!("line {lineno}: {key} expects a number"),
        }
    }
    fn as_f64(key: &str, value: ConfigValue, lineno: usize) -> Result<f64> {
        match value {
            ConfigValue::Num(n) => n
                .parse()
                .map_err(|_| anyhow::anyhow!("line {lineno}: {key} expects a number")),
            _ => anyhow::bail!("line {lineno}: {key} expects a number"),
        }
    }
    fn as_list(key: &str, value: ConfigValue, lineno: usize) -> Result<Vec<String>> {
        match value {
            ConfigValue::List(items) => Ok(items),
            _ => anyhow::bail!("line {lineno}: {key} expects an array"),
        }
    }
    match key {
        "endpoint" => cfg.endpoint = Some(as_str(key, value, lineno)?),
        "model" => cfg.model = Some(as_str(key, value, lineno)?),
        "docstrings_model" => cfg.docstrings_model = Some(as_str(key, value, lineno)?),
        "api_key" => cfg.api_key = Some(as_str(key, value, lineno)?),
        "workers" => cfg.workers = Some(as_usize(key, value, lineno)?),
        "min_lines" => cfg.min_lines = Some(as_usize(key, value, lineno)?),
        "max_words" => cfg.max_words = Some(as_usize(key, value, lineno)?),
        "min_density" => cfg.min_density = Some(as_f64(key, value, lineno)?),
        "ignore" => cfg.ignore = Some(as_list(key, value, lineno)?),
        "keep_decorators" => cfg.keep_decorators = Some(as_list(key, value, lineno)?),
        "keep_bases" => cfg.keep_bases = Some(as_list(key, value, lineno)?),
        _ => {}
    }
    Ok(())
}

/// Concatenates the default list with the user's, so a config file's list adds to the shipped
/// default rather than replacing it. General on purpose: any future `ConfigValue::List` key
/// (e.g. docstring keep-rules) can reuse this same merge.
fn merge_list(default: Vec<String>, user: Option<Vec<String>>) -> Vec<String> {
    let mut merged = default;
    if let Some(user) = user {
        merged.extend(user);
    }
    merged
}

/// Every default the CLI ships with, parsed once from the embedded `default_config.toml` (see
/// `DEFAULT_CONFIG_TOML`). A parse failure here is a programmer error -- a bad edit to that file
/// -- not a user error, hence `expect`.
struct Defaults {
    endpoint: String,
    model: String,
    docstrings_model: String,
    workers: usize,
    min_lines: usize,
    min_density: f64,
    max_words: usize,
    ignore: Vec<String>,
    keep_decorators: Vec<String>,
    keep_bases: Vec<String>,
}

fn embedded_defaults() -> Defaults {
    let cfg = parse_config(DEFAULT_CONFIG_TOML).expect("src/default_config.toml failed to parse");
    Defaults {
        endpoint: cfg
            .endpoint
            .expect("src/default_config.toml: missing `endpoint`"),
        model: cfg.model.expect("src/default_config.toml: missing `model`"),
        docstrings_model: cfg
            .docstrings_model
            .expect("src/default_config.toml: missing `docstrings_model`"),
        workers: cfg
            .workers
            .expect("src/default_config.toml: missing `workers`"),
        min_lines: cfg
            .min_lines
            .expect("src/default_config.toml: missing `min_lines`"),
        min_density: cfg
            .min_density
            .expect("src/default_config.toml: missing `min_density`"),
        max_words: cfg
            .max_words
            .expect("src/default_config.toml: missing `max_words`"),
        ignore: cfg
            .ignore
            .expect("src/default_config.toml: missing `ignore`"),
        keep_decorators: cfg
            .keep_decorators
            .expect("src/default_config.toml: missing `keep_decorators`"),
        keep_bases: cfg
            .keep_bases
            .expect("src/default_config.toml: missing `keep_bases`"),
    }
}

fn default_config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_default();
    base.join("commentreducr").join("config.toml")
}

/// A missing file is fine (all defaults); a present but malformed one is an error.
fn load_file_config(path: &Path) -> Result<FileConfig> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_config(&text).with_context(|| format!("bad config {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FileConfig::default()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // Panics inside a worker are caught and reported as warnings; keep the default hook quiet.
    std::panic::set_hook(Box::new(|_| {}));
    let (target, opts) = match &cli.command {
        Command::Comments(o) => (Target::Comments, o),
        Command::Docstrings(o) => (Target::Docstrings, o),
    };
    // clap's `requires = "delete"` on --dry-run only fires against the arguments actually typed,
    // so `--reduce --dry-run` (an explicit --reduce, rather than --reduce's absence) sails past
    // it and would otherwise run the full reduce pipeline -- including live LLM calls -- with
    // only the final write suppressed. Reduce mode does not support dry-run at all; check the
    // resolved flag instead of trusting clap to have rejected every shape of this combination.
    if opts.dry_run && !opts.delete {
        anyhow::bail!("--dry-run only applies to --delete");
    }
    let defaults = embedded_defaults();
    let file = load_file_config(&opts.config)?;
    let cfg = Config {
        target,
        mode: if opts.delete {
            Mode::Delete
        } else {
            Mode::Reduce
        },
        min_lines: opts
            .min_lines
            .or(file.min_lines)
            .unwrap_or(defaults.min_lines),
        min_density: opts
            .min_density
            .or(file.min_density)
            .unwrap_or(defaults.min_density),
        max_summary_words: opts
            .max_words
            .or(file.max_words)
            .unwrap_or(defaults.max_words),
        endpoint: opts
            .endpoint
            .clone()
            .or(file.endpoint)
            .unwrap_or(defaults.endpoint),
        model: opts
            .model
            .clone()
            .or(match target {
                Target::Comments => file.model,
                Target::Docstrings => file.docstrings_model.or(file.model),
            })
            .unwrap_or(match target {
                Target::Comments => defaults.model,
                Target::Docstrings => defaults.docstrings_model,
            }),
        api_key: opts.api_key.clone().or(file.api_key),
        llm_concurrency: opts.workers.or(file.workers).unwrap_or(defaults.workers),
        dry_run: opts.dry_run,
        verbose: opts.verbose,
        ignore: merge_list(defaults.ignore, file.ignore),
        keep_decorators: merge_list(defaults.keep_decorators, file.keep_decorators),
        keep_bases: merge_list(defaults.keep_bases, file.keep_bases),
    };
    if let Some(dataset) = &opts.eval {
        return match target {
            Target::Comments => commentreducr::eval::run(dataset, &cfg),
            Target::Docstrings => commentreducr::eval::run_docstrings(dataset, &cfg),
        };
    }
    let default_path = PathBuf::from(".");
    let path = opts.path.as_deref().unwrap_or(&default_path);
    if opts.diagnose {
        println!("commentreducr {}", env!("CARGO_PKG_VERSION"));
        let bad = commentreducr::diagnose(path, target, &cfg.ignore)?;
        eprintln!("{bad} files with parse errors");
        if bad > 0 {
            std::process::exit(1);
        }
        return Ok(());
    }
    let stats = run(path, &cfg)?;
    let noun = match target {
        Target::Comments => "comments",
        Target::Docstrings => "docstrings",
    };
    eprintln!(
        "{} files scanned, {} changed, {} skipped; {noun}: {} kept, {} deleted ({} lines), {} reduced ({} lines saved), {} LLM failures",
        stats.files_scanned,
        stats.files_changed,
        stats.files_skipped,
        stats.comments_kept,
        stats.comments_deleted,
        stats.lines_deleted,
        stats.comments_reduced,
        stats.lines_reduced,
        stats.llm_errors,
    );
    if let Some(t) = stats.tokens {
        eprintln!(
            "tokens: {t}; {:.1}s, {}",
            stats.elapsed.as_secs_f64(),
            commentreducr::progress::token_rates(t, stats.elapsed)
        );
    }
    if stats.has_errors() {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_config_is_default_and_bad_config_errors() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "commentreducr-maintest-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        assert!(load_file_config(&path).unwrap().model.is_none());
        std::fs::write(&path, "model = \"m\"\napi_key = \"k\"\nmin_lines = 2\n").unwrap();
        let c = load_file_config(&path).unwrap();
        assert_eq!(
            (
                c.model.as_deref(),
                c.api_key.as_deref(),
                c.endpoint,
                c.docstrings_model,
                c.min_lines
            ),
            (Some("m"), Some("k"), None, None, Some(2))
        );
        std::fs::write(&path, "model = ").unwrap();
        assert!(load_file_config(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_config_readme_example_and_errors() {
        // The README's example config, verbatim (mirrors src/default_config.toml; keep both in
        // sync by hand if either changes).
        let readme = r#"
# Every default commentreducr ships with. Copy this into ~/.config/commentreducr/config.toml
# (or --config FILE) and override only what you need; flags win over that file, which wins over
# these defaults. List-valued keys like `ignore` are the exception: a config file's list is
# appended to the shipped default, not swapped in for it.
endpoint = "http://localhost:8000/v1"        # OpenAI-compatible base URL
model = "gemma-4-e2b-it-4bit"                # used for comments (and docstrings if docstrings_model is unset)
docstrings_model = "gemma-4-26b-a4b-it-4bit" # used for docstrings
workers = 8                                   # worker threads / max in-flight LLM requests
min_lines = 4                                 # minimum lines in a block before it's reduced
min_density = 5.0                             # comments only: minimum average words per line
max_words = 20                                # comments only: target max words in a summary
ignore = ["migrations/"]   # gitignore-syntax patterns; a user config's list is appended to this one
keep_decorators = ["tool", "command", "group"]   # docstrings under these decorators are never touched (Strands @tool, click/typer commands)
keep_bases = ["Signature", "BaseModel"]          # class docstrings with these bases are never touched (dspy.Signature, pydantic.BaseModel)
"#;
        let c = parse_config(readme).unwrap();
        assert_eq!(c.endpoint.as_deref(), Some("http://localhost:8000/v1"));
        assert_eq!(c.model.as_deref(), Some("gemma-4-e2b-it-4bit"));
        assert_eq!(
            c.docstrings_model.as_deref(),
            Some("gemma-4-26b-a4b-it-4bit")
        );
        assert_eq!(c.workers, Some(8));
        assert_eq!(c.min_lines, Some(4));
        assert_eq!(c.min_density, Some(5.0));
        assert_eq!(c.max_words, Some(20));
        assert_eq!(c.ignore, Some(vec!["migrations/".to_string()]));
        assert_eq!(
            c.keep_decorators,
            Some(vec![
                "tool".to_string(),
                "command".to_string(),
                "group".to_string()
            ])
        );
        assert_eq!(
            c.keep_bases,
            Some(vec!["Signature".to_string(), "BaseModel".to_string()])
        );

        // escaped quote inside a string
        let c = parse_config(r#"model = "a \"quoted\" name""#).unwrap();
        assert_eq!(c.model.as_deref(), Some("a \"quoted\" name"));

        // unknown keys are ignored, like the old serde-based parser
        assert!(parse_config("nonsense = \"x\"\n").unwrap().model.is_none());

        // errors: duplicate key, a [section] header, a string for a numeric key
        assert!(parse_config("model = \"a\"\nmodel = \"b\"\n").is_err());
        assert!(parse_config("[section]\n").is_err());
        assert!(parse_config("min_lines = \"two\"\n").is_err());
    }

    #[test]
    fn parse_config_arrays() {
        let c = parse_config("ignore = [\"a\", \"b\"]\n").unwrap();
        assert_eq!(c.ignore, Some(vec!["a".to_string(), "b".to_string()]));

        let c = parse_config("ignore = []\n").unwrap();
        assert_eq!(c.ignore, Some(vec![]));

        // whitespace around items/commas, and a trailing comment
        let c = parse_config("ignore = [ \"a\" , \"b\" ]   # patterns\n").unwrap();
        assert_eq!(c.ignore, Some(vec!["a".to_string(), "b".to_string()]));

        // errors: non-string item, nested array, unterminated (also covers a multi-line array,
        // since the parser never looks past the current line), an array for a scalar key
        assert!(parse_config("ignore = [1, \"b\"]\n").is_err());
        assert!(parse_config("ignore = [[\"a\"]]\n").is_err());
        assert!(parse_config("ignore = [\"a\"\n").is_err());
        assert!(parse_config("min_lines = [\"a\"]\n").is_err());
    }

    #[test]
    fn merge_list_appends_user_to_default() {
        assert_eq!(
            merge_list(vec!["a".to_string()], Some(vec!["b".to_string()])),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(
            merge_list(vec!["a".to_string()], None),
            vec!["a".to_string()]
        );
        assert_eq!(
            merge_list(vec![], Some(vec!["b".to_string()])),
            vec!["b".to_string()]
        );
    }

    #[test]
    fn embedded_default_config_parses_and_has_every_key() {
        let c = parse_config(DEFAULT_CONFIG_TOML).expect("src/default_config.toml must parse");
        assert!(c.endpoint.is_some());
        assert!(c.model.is_some());
        assert!(c.docstrings_model.is_some());
        assert!(c.workers.is_some());
        assert!(c.min_lines.is_some());
        assert!(c.min_density.is_some());
        assert!(c.max_words.is_some());
        assert!(c.ignore.is_some());
        assert!(c.keep_decorators.is_some());
        assert!(c.keep_bases.is_some());
    }

    #[test]
    fn embedded_defaults_match_help_text() {
        // These literals mirror the `[default: ...]` text hardcoded in Opts' clap help strings
        // above; keep both in sync by hand if src/default_config.toml's values ever change.
        let d = embedded_defaults();
        assert_eq!(d.endpoint, "http://localhost:8000/v1");
        assert_eq!(d.model, "gemma-4-e2b-it-4bit");
        assert_eq!(d.docstrings_model, "gemma-4-26b-a4b-it-4bit");
        assert_eq!(d.workers, 8);
        assert_eq!(d.min_lines, 4);
        assert_eq!(d.min_density, 5.0);
        assert_eq!(d.max_words, 20);
    }
}
