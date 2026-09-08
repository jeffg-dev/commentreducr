//! Run a labeled dataset (tools/dataset/comments.jsonl or tools/dataset/docstrings.jsonl) through
//! the live LLM prompt and report how well the model's decisions match the labels. Used to
//! iterate on the prompt.
use crate::llm::{DocRequest, DocVerdict, LlmClient, Verdict};
use crate::types::{CommentBlock, Config, Language};
use crate::{parse, prose};
use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use std::path::Path;

#[derive(Deserialize, Clone)]
struct Row {
    id: String,
    language: String,
    comment: String,
    #[serde(default)]
    context: String,
    output: String,
}

fn language(name: &str) -> Result<Language> {
    Ok(match name {
        "python" => Language::Python,
        "javascript" => Language::JavaScript,
        "typescript" => Language::TypeScript,
        "tsx" => Language::Tsx,
        "yaml" => Language::Yaml,
        other => return Err(anyhow!("unknown language {other}")),
    })
}

/// Turn a raw comment block (delimiters included) into the same prose the tool would send.
fn prose_of(comment: &str, lang: Language) -> Result<String> {
    let comments = parse::extract_comments(comment, lang)?;
    let first = comments
        .first()
        .ok_or_else(|| anyhow!("no comment parsed"))?;
    let last = comments.last().unwrap();
    let block = CommentBlock {
        start: first.start,
        end: last.end,
        start_line: first.start_line,
        end_line: last.end_line,
        indent: String::new(),
        own_line: true,
        code_after: false,
        kind: first.kind,
        comments,
    };
    Ok(prose::analyze(&block, lang).text)
}

pub fn run(dataset: &Path, cfg: &Config) -> Result<()> {
    let text = std::fs::read_to_string(dataset).context("reading dataset")?;
    let rows: Vec<Row> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).context("bad dataset row"))
        .collect::<Result<_>>()?;
    let llm = LlmClient::new(cfg);
    let max_words = cfg.max_summary_words;

    let outcomes = crate::parallel(rows, cfg.llm_concurrency, |row| -> Result<Verdict> {
        let lang = language(&row.language)?;
        let p = prose_of(&row.comment, lang)?;
        llm.summarize(&p, &row.context, max_words)
            .with_context(|| format!("{}: model failed", row.id))
    });
    let mut results = Vec::new();
    for (row, r) in outcomes {
        let v = r.map_err(|m| anyhow!("{}: {m}", row.id))??;
        results.push((row, v));
    }

    let (mut n, mut agree, mut exp_del, mut got_del, mut both_del, mut kept_words) =
        (0, 0, 0, 0, 0, 0);
    results.sort_by(|a, b| a.0.id.cmp(&b.0.id));
    for (row, v) in &results {
        n += 1;
        let e_del = row.output == "DELETE";
        let g_del = *v == Verdict::Delete;
        exp_del += e_del as usize;
        got_del += g_del as usize;
        both_del += (e_del && g_del) as usize;
        agree += (e_del == g_del) as usize;
        let got = match v {
            Verdict::Delete => "DELETE".to_string(),
            Verdict::Line(s) => {
                kept_words += s.split_whitespace().count();
                s.clone()
            }
        };
        let mark = if e_del == g_del { "ok  " } else { "MISS" };
        println!(
            "{mark} {:<8} expected: {:<60} got: {got}",
            row.id, row.output
        );
    }
    let kept = n - got_del;
    println!(
        "\n{n} rows; decision accuracy {:.1}%; DELETE precision {:.1}% recall {:.1}%; kept lines avg {:.1} words",
        100.0 * agree as f64 / n.max(1) as f64,
        100.0 * both_del as f64 / got_del.max(1) as f64,
        100.0 * both_del as f64 / exp_del.max(1) as f64,
        kept_words as f64 / kept.max(1) as f64
    );
    println!("tokens: {}", llm.tokens.snapshot());
    Ok(())
}

#[derive(Deserialize, Clone)]
struct DocRow {
    id: String,
    kind: String,
    name: String,
    signature: String,
    is_test: bool,
    in_test_file: bool,
    body_lines: usize,
    #[serde(default)]
    body_preview: Vec<String>,
    docstring: String,
    output: String,
}

/// Run `tools/dataset/docstrings.jsonl` (or a compatible file) through the live docstring-rewrite
/// prompt and report decision accuracy, DELETE precision/recall, and (for kept rows) the average
/// line and word counts of the model's replacement text.
pub fn run_docstrings(dataset: &Path, cfg: &Config) -> Result<()> {
    let text = std::fs::read_to_string(dataset).context("reading dataset")?;
    let rows: Vec<DocRow> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).context("bad dataset row"))
        .collect::<Result<_>>()?;
    let llm = LlmClient::new(cfg);

    let outcomes = crate::parallel(rows, cfg.llm_concurrency, |row| -> Result<DocVerdict> {
        let req = DocRequest {
            kind: &row.kind,
            name: &row.name,
            signature: &row.signature,
            is_test: row.is_test,
            in_test_file: row.in_test_file,
            text: &row.docstring,
            body_preview: &row.body_preview,
            body_lines: row.body_lines,
        };
        llm.rewrite_docstring(&req)
            .with_context(|| format!("{}: model failed", row.id))
    });
    let mut results = Vec::new();
    for (row, r) in outcomes {
        let v = r.map_err(|m| anyhow!("{}: {m}", row.id))??;
        results.push((row, v));
    }

    let (mut n, mut agree, mut exp_del, mut got_del, mut both_del, mut kept_lines, mut kept_words) =
        (0, 0, 0, 0, 0, 0, 0);
    results.sort_by(|a, b| a.0.id.cmp(&b.0.id));
    for (row, v) in &results {
        n += 1;
        let e_del = row.output == "DELETE";
        let g_del = *v == DocVerdict::Delete;
        exp_del += e_del as usize;
        got_del += g_del as usize;
        both_del += (e_del && g_del) as usize;
        agree += (e_del == g_del) as usize;
        let got_display = match v {
            DocVerdict::Delete => "DELETE".to_string(),
            DocVerdict::Text(s) => {
                kept_lines += s.lines().count();
                kept_words += s.split_whitespace().count();
                s.lines().next().unwrap_or("").to_string()
            }
        };
        let mark = if e_del == g_del { "ok  " } else { "MISS" };
        let expected_first = row.output.lines().next().unwrap_or("");
        println!(
            "{mark} {:<8} expected: {:<60} got: {got_display}",
            row.id, expected_first
        );
    }
    let kept = n - got_del;
    println!(
        "\n{n} rows; decision accuracy {:.1}%; DELETE precision {:.1}% recall {:.1}%; kept avg \
         {:.1} lines, {:.1} words",
        100.0 * agree as f64 / n.max(1) as f64,
        100.0 * both_del as f64 / got_del.max(1) as f64,
        100.0 * both_del as f64 / exp_del.max(1) as f64,
        kept_lines as f64 / kept.max(1) as f64,
        kept_words as f64 / kept.max(1) as f64
    );
    println!("tokens: {}", llm.tokens.snapshot());
    Ok(())
}
