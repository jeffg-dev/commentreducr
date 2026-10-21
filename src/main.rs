use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use commentreducr::{Config, Finding, JudgeKind, Languages, Mode, Target, findings, run};
use std::path::{Path, PathBuf};

/// Shipped defaults use the same configuration parser as the user's file.
const DEFAULT_CONFIG_TOML: &str = include_str!("default_config.toml");

const AFTER_HELP: &str =
    "Advanced options: --help. Settings file: ~/.config/commentreducr/config.toml";
const AFTER_LONG_HELP: &str = "endpoint, model, docstrings_model, api_key, workers, scope, language, database, \
                               max_words, ignore, keep_decorators, keep_bases, judge, judge_model, \
                               judge_endpoint, escalation_model, generator_model and judge_thresholds \
                               can also be set in ~/.config/commentreducr/config.toml; flags \
                               win.";

/// Delete or reduce comments (Python, JS/TS, YAML, Rust) or Python docstrings in git-tracked files.
#[derive(Parser, Debug)]
#[command(name = "commentreducr", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Screen all non-structural items, then reduce or delete flagged items with the LLM
    #[command(after_help = AFTER_HELP, after_long_help = AFTER_LONG_HELP)]
    Reduce(Opts),
    /// Delete all non-structural items without a classifier or LLM
    #[command(after_help = AFTER_HELP, after_long_help = AFTER_LONG_HELP)]
    Delete(Opts),
    /// Install a warning-only pre-push hook and prepare the local classifier
    InstallGitHook,
    /// Check changed comments and docstrings in commits being pushed (or in --changed)
    Check(HookOpts),
}

#[derive(clap::Args, Debug)]
struct HookOpts {
    /// Print findings and check errors to stdout, and always exit successfully
    #[arg(long)]
    warn: bool,
    /// Config file for ignore patterns and structural docstring exemptions
    #[arg(long, value_name = "FILE", default_value_os_t = default_config_path(), hide_default_value = true)]
    config: PathBuf,
    /// SQLite classifier cache [default: Git metadata directory/commentreducr/state.sqlite]
    #[arg(long, value_name = "FILE")]
    database: Option<PathBuf>,
    /// Check this commit range instead of the push (or upstream..HEAD)
    #[arg(long, value_name = "BASE..HEAD", conflicts_with = "remote_name")]
    changed: Option<String>,
    /// Output format [default: text]; json and sarif print the summary on stderr
    #[arg(long, value_enum, default_value_t = Format::Text, hide_default_value = true)]
    format: Format,
    /// Write the findings to FILE instead of stdout
    #[arg(long, value_name = "FILE")]
    output: Option<PathBuf>,
    #[arg(hide = true)]
    remote_name: Option<String>,
    #[arg(hide = true, requires = "remote_name")]
    remote_location: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum Format {
    Text,
    Json,
    Sarif,
}

fn install_hook() -> Result<()> {
    let directory = std::env::current_dir()?;
    let status = std::process::Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(&directory)
        .output()?;
    anyhow::ensure!(
        status.status.success(),
        "run install-git-hook inside a Git repository"
    );
    commentreducr::hook_model::install_model()?;
    let path = commentreducr::hook_git::install(&directory)?;
    println!("Installed warning-only pre-push hook: {}", path.display());
    let judge = load_file_config(&default_config_path())?
        .judge
        .unwrap_or(embedded_defaults().judge);
    if judge == JudgeKind::Legacy {
        println!(
            "judge = legacy: Python only, classifier. For all languages, set judge = \"systemone\" in {}",
            default_config_path().display()
        );
    } else {
        let name = if judge == JudgeKind::SystemOne {
            "systemone"
        } else {
            "generative"
        };
        println!("judge = {name}: all languages");
    }
    Ok(())
}

fn prepush_check(opts: &HookOpts) -> Result<()> {
    let text = opts.format == Format::Text;
    // Under json and sarif, stdout carries only the document.
    let say = |line: &str| {
        if text {
            println!("{line}")
        } else {
            eprintln!("{line}")
        }
    };
    let result = (|| -> Result<bool> {
        let file_opts = Opts {
            config: opts.config.clone(),
            database: opts.database.clone(),
            ..Opts::default()
        };
        let cfg = build_config(&file_opts, Mode::Reduce)?;
        let mut input = String::new();
        if opts.remote_name.is_some() {
            use std::io::Read;
            std::io::stdin().read_to_string(&mut input)?;
        }
        let repo = std::env::current_dir()?;
        let range = opts.changed.as_deref();
        let input = opts.remote_name.as_ref().map(|_| input.as_str());
        let remote = opts.remote_name.as_deref();
        let (findings, lines, summary, failed) = if cfg.judge == JudgeKind::Legacy {
            legacy_check(&repo, &cfg, range, input, remote, text)?
        } else {
            let stats = commentreducr::check_changed(&repo, &cfg, range, input, remote)?;
            let errors = stats.files_skipped + stats.llm_errors + stats.classifier_errors;
            let summary = format!(
                "commentreducr: {} changed blocks checked; {} findings; {errors} check errors; {} stale; {} untouched.",
                stats.changed_items,
                stats.findings.len(),
                stats.comments_stale,
                stats.untouched
            );
            let failed = !stats.findings.is_empty() || errors > 0;
            let lines = stats.finding_lines().collect();
            (stats.findings, lines, summary, failed)
        };
        let version = env!("CARGO_PKG_VERSION");
        let mut document = match opts.format {
            Format::Text => lines.join("\n"),
            Format::Json => serde_json::to_string_pretty(&findings::json(&findings))?,
            Format::Sarif => serde_json::to_string_pretty(&findings::sarif(&findings, version))?,
        };
        if !document.is_empty() {
            document.push('\n');
        }
        match &opts.output {
            Some(path) => std::fs::write(path, document)
                .with_context(|| format!("cannot write {}", path.display()))?,
            None => print!("{document}"),
        }
        say(&summary);
        Ok(failed)
    })();
    let failed = match result {
        Ok(failed) => failed,
        Err(error) => {
            say(&format!(
                "commentreducr: could not complete pre-push check: {error:#}"
            ));
            true
        }
    };
    if failed && !opts.warn {
        std::process::exit(1);
    }
    Ok(())
}

/// The classifier path: Python blocks only. Returns findings, the text lines (today's format),
/// the summary line and whether anything was found or failed. Under json and sarif, per-block
/// errors go to stderr so stdout stays a valid document.
fn legacy_check(
    repo: &Path,
    cfg: &Config,
    range: Option<&str>,
    input: Option<&str>,
    remote: Option<&str>,
    text: bool,
) -> Result<(Vec<Finding>, Vec<String>, String, bool)> {
    let (blocks, other) = commentreducr::hook_git::changed_blocks(
        repo,
        range,
        input,
        remote,
        &cfg.ignore,
        &cfg.keep_decorators,
        &cfg.keep_bases,
    )?;
    let note = if other > 0 {
        format!(" {other} non-Python files not checked (set judge = systemone to check them).")
    } else {
        String::new()
    };
    if blocks.is_empty() {
        let summary = format!("commentreducr: no changed Python documentation blocks.{note}");
        return Ok((Vec::new(), Vec::new(), summary, false));
    }
    let database = commentreducr::state::Database::open(repo, cfg.database.as_deref())?;
    let threshold = commentreducr::hook_model::Classifier::default_threshold()?;
    let mut classifier = None;
    let mut findings = Vec::new();
    let mut lines = Vec::new();
    let mut errors = 0;
    for block in &blocks {
        let verdict = (|| -> Result<(bool, f64)> {
            let key =
                commentreducr::state::classifier_key(&block.kind, &block.text, &block.context);
            if let Some(cached) = database.classification(&key)? {
                return Ok(cached);
            }
            if classifier.is_none() {
                classifier = Some(commentreducr::hook_model::Classifier::load()?);
            }
            let classifier = classifier.as_mut().unwrap();
            let probability = classifier.probability(&block.kind, &block.text, &block.context)?;
            let flag =
                database.remember_classification(&key, probability, classifier.threshold())?;
            Ok((flag, probability))
        })();
        match verdict {
            Ok((true, probability)) => {
                let advice = if block.kind == "comment" {
                    "Keep only an unexpected reason, a non-obvious trap, or a shortcut through complex code."
                } else {
                    "Keep the consumer contract: purpose, usage, results, and exceptions."
                };
                let message = format!("possible {} bloat. {advice}", block.kind.replace('_', " "));
                lines.push(format!(
                    "{}:{}-{}: {message}",
                    block.path.display(),
                    block.start_line,
                    block.end_line,
                ));
                findings.push(Finding {
                    path: block.path.clone(),
                    start_line: block.start_line,
                    end_line: block.end_line,
                    tier: block.tier,
                    kind: block.kind.clone(),
                    verdict: "bloat".into(),
                    checks: vec![("classifier".into(), probability, threshold)],
                    evidence: String::new(),
                    message,
                    comment: String::new(),
                });
            }
            Ok(_) => {}
            Err(error) => {
                errors += 1;
                let line = format!(
                    "{}:{}: could not check this block: {error:#}",
                    block.path.display(),
                    block.start_line
                );
                if text {
                    lines.push(line);
                } else {
                    eprintln!("{line}");
                }
            }
        }
    }
    let summary = format!(
        "commentreducr: {} changed blocks checked; {} findings; {errors} check errors.{note}",
        blocks.len(),
        findings.len()
    );
    let failed = !findings.is_empty() || errors > 0;
    Ok((findings, lines, summary, failed))
}

/// Field order is help order: common flags, then the "Inference" heading, then "Advanced" (long
/// help only). Numeric flags are `Option` so the config file can supply them when absent.
#[derive(clap::Args, Debug, Default)]
struct Opts {
    /// Directory or file to process [default: .]
    path: Option<PathBuf>,

    /// Items to process [default: all]
    #[arg(long, value_enum)]
    scope: Option<Target>,

    /// Languages to process, repeatable [default: all]; typescript includes JavaScript and TSX
    #[arg(short, long, value_enum)]
    language: Vec<Languages>,

    /// Judge only comments added or changed in this commit range, and report comments whose code changed
    #[arg(long, value_name = "BASE..HEAD", conflicts_with_all = ["eval", "diagnose", "dump_items"])]
    changed: Option<String>,

    /// With delete only: report changes without writing
    #[arg(long)]
    dry_run: bool,

    /// For AI agents: delete only the items judged safe by the decision engine; print advice for
    /// the rest as JSONL on stdout
    #[arg(long, conflicts_with_all = ["eval", "diagnose", "dump_items"])]
    ai: bool,

    /// SQLite state file [default: Git metadata directory/commentreducr/state.sqlite]
    #[arg(long, value_name = "FILE")]
    database: Option<PathBuf>,

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
    #[arg(long, help_heading = "Inference")]
    endpoint: Option<String>,

    /// Model name
    #[arg(
        long,
        help_heading = "Inference",
        long_help = "Model name [default: gemma-4-e2b-it-4bit for comments, \
                      gemma-4-26b-a4b-it-4bit for docstrings]"
    )]
    model: Option<String>,

    /// API key, if the endpoint needs one
    #[arg(long, help_heading = "Inference")]
    api_key: Option<String>,

    /// Verdict path [default: legacy]
    #[arg(long, value_enum, help_heading = "Inference")]
    judge: Option<JudgeKind>,

    /// Judge model name [default: clef-flash-4bit]
    #[arg(long, help_heading = "Inference")]
    judge_model: Option<String>,

    /// Full judge URL [default: derived from the endpoint]
    #[arg(long, help_heading = "Inference")]
    judge_endpoint: Option<String>,

    /// JSON file of policy thresholds merged over the shipped ones (e.g. for a fine-tuned judge)
    #[arg(long, value_name = "FILE", help_heading = "Inference")]
    judge_thresholds: Option<PathBuf>,

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

    /// (v2 datasets) Write one run file per row into DIR, for tools/bakeoff/score.py
    #[arg(
        long,
        value_name = "DIR",
        requires = "eval",
        help_heading = "Advanced",
        hide_short_help = true
    )]
    eval_runs: Option<PathBuf>,

    /// Print every item of FILE as JSON (byte range, lines, kind, text, context, structural); no LLM
    #[arg(
        long,
        value_name = "FILE",
        conflicts_with_all = ["eval", "diagnose"],
        help_heading = "Advanced",
        hide_short_help = true
    )]
    dump_items: Option<PathBuf>,
}

/// Optional settings from the config file; flags override these.
#[derive(Default)]
struct FileConfig {
    endpoint: Option<String>,
    model: Option<String>,
    /// Docstring LLM model; falls back to `model`, then the built-in default.
    docstrings_model: Option<String>,
    api_key: Option<String>,
    workers: Option<usize>,
    scope: Option<Target>,
    database: Option<PathBuf>,
    language: Option<Languages>,
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
    judge: Option<JudgeKind>,
    judge_endpoint: Option<String>,
    judge_model: Option<String>,
    escalation_model: Option<String>,
    generator_model: Option<String>,
    judge_thresholds: Option<PathBuf>,
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
        "scope" => {
            let s = as_str(key, value, lineno)?;
            cfg.scope = Some(match s.as_str() {
                "comments" => Target::Comments,
                "docstrings" => Target::Docstrings,
                "all" => Target::All,
                _ => anyhow::bail!("line {lineno}: invalid scope"),
            });
        }
        "database" => cfg.database = Some(PathBuf::from(as_str(key, value, lineno)?)),
        "language" => {
            let s = as_str(key, value, lineno)?;
            cfg.language = Some(match s.as_str() {
                "python" => Languages::Python,
                "typescript" => Languages::TypeScript,
                "rust" => Languages::Rust,
                "yaml" => Languages::Yaml,
                "all" => Languages::All,
                _ => anyhow::bail!("line {lineno}: invalid language"),
            });
        }
        "max_words" => cfg.max_words = Some(as_usize(key, value, lineno)?),
        "ignore" => cfg.ignore = Some(as_list(key, value, lineno)?),
        "keep_decorators" => cfg.keep_decorators = Some(as_list(key, value, lineno)?),
        "keep_bases" => cfg.keep_bases = Some(as_list(key, value, lineno)?),
        "judge" => {
            let s = as_str(key, value, lineno)?;
            cfg.judge = Some(match s.as_str() {
                "legacy" => JudgeKind::Legacy,
                "systemone" => JudgeKind::SystemOne,
                "generative" => JudgeKind::Generative,
                _ => anyhow::bail!("line {lineno}: invalid judge"),
            });
        }
        "judge_endpoint" => cfg.judge_endpoint = Some(as_str(key, value, lineno)?),
        "judge_model" => cfg.judge_model = Some(as_str(key, value, lineno)?),
        "escalation_model" => cfg.escalation_model = Some(as_str(key, value, lineno)?),
        "generator_model" => cfg.generator_model = Some(as_str(key, value, lineno)?),
        "judge_thresholds" => {
            cfg.judge_thresholds = Some(PathBuf::from(as_str(key, value, lineno)?))
        }
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
    scope: Target,
    language: Languages,
    max_words: usize,
    ignore: Vec<String>,
    keep_decorators: Vec<String>,
    keep_bases: Vec<String>,
    judge: JudgeKind,
    judge_model: String,
    generator_model: String,
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
        scope: cfg.scope.expect("src/default_config.toml: missing `scope`"),
        language: cfg
            .language
            .expect("src/default_config.toml: missing `language`"),
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
        judge: cfg.judge.expect("src/default_config.toml: missing `judge`"),
        judge_model: cfg
            .judge_model
            .expect("src/default_config.toml: missing `judge_model`"),
        generator_model: cfg
            .generator_model
            .expect("src/default_config.toml: missing `generator_model`"),
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

/// Defaults, then the config file, then flags. `check` passes only `config` and `database`.
fn build_config(opts: &Opts, mode: Mode) -> Result<Config> {
    let defaults = embedded_defaults();
    let file = load_file_config(&opts.config)?;
    let target = opts.scope.or(file.scope).unwrap_or(defaults.scope);
    let comment_model = opts
        .model
        .clone()
        .or(file.model.clone())
        .unwrap_or(defaults.model);
    let doc_model = opts
        .model
        .clone()
        .or(file.docstrings_model)
        .or(file.model)
        .unwrap_or(defaults.docstrings_model);
    let cfg = Config {
        target,
        language: if opts.language.is_empty() {
            vec![file.language.unwrap_or(defaults.language)]
        } else {
            opts.language.clone()
        },
        mode,
        max_summary_words: opts
            .max_words
            .or(file.max_words)
            .unwrap_or(defaults.max_words),
        endpoint: opts
            .endpoint
            .clone()
            .or(file.endpoint)
            .unwrap_or(defaults.endpoint),
        model: if target == Target::Docstrings {
            doc_model.clone()
        } else {
            comment_model
        },
        docstrings_model: doc_model,
        api_key: opts.api_key.clone().or(file.api_key),
        llm_concurrency: opts.workers.or(file.workers).unwrap_or(defaults.workers),
        dry_run: opts.dry_run,
        verbose: opts.verbose,
        database: opts.database.clone().or(file.database),
        ignore: merge_list(defaults.ignore, file.ignore),
        keep_decorators: merge_list(defaults.keep_decorators, file.keep_decorators),
        keep_bases: merge_list(defaults.keep_bases, file.keep_bases),
        judge: opts.judge.or(file.judge).unwrap_or(defaults.judge),
        judge_endpoint: opts.judge_endpoint.clone().or(file.judge_endpoint),
        judge_model: opts
            .judge_model
            .clone()
            .or(file.judge_model)
            .unwrap_or(defaults.judge_model),
        escalation_model: file.escalation_model,
        generator_model: file.generator_model.unwrap_or(defaults.generator_model),
        judge_thresholds: opts
            .judge_thresholds
            .clone()
            .or(file.judge_thresholds)
            .map(|p| {
                std::fs::read_to_string(&p).with_context(|| format!("cannot read {}", p.display()))
            })
            .transpose()?,
        ai: opts.ai,
    };
    anyhow::ensure!(cfg.llm_concurrency > 0, "workers must be at least 1");
    anyhow::ensure!(cfg.max_summary_words > 0, "max_words must be at least 1");
    Ok(cfg)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    std::panic::set_hook(Box::new(|_| {}));
    let (mode, opts) = match &cli.command {
        Command::Reduce(o) => (Mode::Reduce, o),
        Command::Delete(o) => (Mode::Delete, o),
        Command::InstallGitHook => return install_hook(),
        Command::Check(opts) => return prepush_check(opts),
    };
    anyhow::ensure!(
        !opts.dry_run || mode == Mode::Delete,
        "--dry-run only applies to delete"
    );
    anyhow::ensure!(
        !opts.ai || mode == Mode::Reduce,
        "--ai only applies to reduce"
    );
    let cfg = build_config(opts, mode)?;
    anyhow::ensure!(
        !cfg.ai || cfg.judge != commentreducr::JudgeKind::Legacy,
        "--ai needs judge = systemone or generative"
    );
    let target = cfg.target;
    if let Some(dataset) = &opts.eval {
        anyhow::ensure!(mode == Mode::Reduce, "--eval only applies to reduce");
        if commentreducr::eval::is_v2(dataset)? {
            if cfg.judge == commentreducr::JudgeKind::Legacy {
                eprintln!("error: a v2 dataset needs --judge systemone or generative");
                std::process::exit(2);
            }
            return commentreducr::eval::run_v2(dataset, &cfg, opts.eval_runs.as_deref());
        }
        return match target {
            Target::Comments => commentreducr::eval::run(dataset, &cfg),
            Target::Docstrings => commentreducr::eval::run_docstrings(dataset, &cfg),
            Target::All => anyhow::bail!("--eval requires --scope comments or --scope docstrings"),
        };
    }
    if let Some(file) = &opts.dump_items {
        return commentreducr::dump_items(file, &cfg);
    }
    let default_path = PathBuf::from(".");
    let path = opts.path.as_deref().unwrap_or(&default_path);
    if opts.diagnose {
        println!("commentreducr {}", env!("CARGO_PKG_VERSION"));
        let bad = commentreducr::diagnose(path, target, &cfg.language, &cfg.ignore)?;
        eprintln!("{bad} files with parse errors");
        if bad > 0 {
            std::process::exit(1);
        }
        return Ok(());
    }
    let stats = run(path, &cfg, opts.changed.as_deref())?;
    let noun = match target {
        Target::Comments => "comments",
        Target::Docstrings => "docstrings",
        Target::All => "items",
    };
    eprintln!(
        "{} files scanned, {} changed, {} skipped; {noun}: {} kept, {} deleted ({} lines), {} reduced ({} lines saved), {} to review, {} stale, {} LLM failures",
        stats.files_scanned,
        stats.files_changed,
        stats.files_skipped,
        stats.comments_kept,
        stats.comments_deleted,
        stats.lines_deleted,
        stats.comments_reduced,
        stats.lines_reduced,
        stats.comments_review + stats.comments_rename,
        stats.comments_stale,
        stats.llm_errors,
    );
    if cfg.ai {
        for f in &stats.findings {
            println!("{}", findings::ai_line(f));
        }
    } else {
        for line in stats.finding_lines() {
            eprintln!("{line}");
        }
    }
    if mode == Mode::Reduce {
        eprintln!(
            "classifier: {} screened, {} cached, {} errors; resume: {} verdicts cached, {} files completed",
            stats.screened,
            stats.classifier_cached,
            stats.classifier_errors,
            stats.verdicts_cached,
            stats.files_resumed
        );
    }
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
    fn config_and_defaults_keep_layering_and_reject_malformed_values() {
        let d = embedded_defaults();
        assert_eq!(d.scope, Target::All);
        assert_eq!(d.workers, 8);
        assert_eq!(d.judge, JudgeKind::SystemOne);
        let j = parse_config("judge = \"systemone\"\njudge_model = \"m\"\njudge_endpoint = \"u\"\nescalation_model = \"e\"").unwrap();
        assert_eq!(j.judge, Some(JudgeKind::SystemOne));
        assert_eq!(j.escalation_model.as_deref(), Some("e"));
        assert!(parse_config("judge = \"bogus\"").is_err());
        let c = parse_config("scope = \"docstrings\"\nworkers = 2\ndatabase = \"state.db\"\nmodel = \"m\"\nignore = [\"vendor/\"]\nmin_lines = 100\n").unwrap();
        assert_eq!(c.scope, Some(Target::Docstrings));
        assert_eq!(c.workers, Some(2));
        assert_eq!(c.database, Some(PathBuf::from("state.db")));
        assert_eq!(c.model.as_deref(), Some("m"));
        assert!(merge_list(d.ignore, c.ignore).contains(&"vendor/".to_string()));
        for text in [
            "scope = \"bad\"",
            "workers = \"two\"",
            "ignore = [1]",
            "model = ",
            "model = \"a\"\nmodel = \"b\"",
            "[section]",
        ] {
            assert!(parse_config(text).is_err(), "{text}");
        }
        assert_eq!(
            parse_config(r#"model = "a \"quoted\" name""#)
                .unwrap()
                .model
                .as_deref(),
            Some("a \"quoted\" name")
        );
    }
    #[test]
    fn cli_defaults_and_breaking_syntax() {
        for mode in ["reduce", "delete"] {
            assert!(
                Cli::try_parse_from(["commentreducr", mode, "--scope", "all", "--workers", "2"])
                    .is_ok()
            );
        }
        for old in ["comments", "docstrings", "install-hook", "prepush-check"] {
            assert!(Cli::try_parse_from(["commentreducr", old]).is_err());
        }
        assert!(Cli::try_parse_from(["commentreducr", "reduce", "--min-lines", "1"]).is_err());
    }
}
