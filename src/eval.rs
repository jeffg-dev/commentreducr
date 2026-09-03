//! Run the labeled dataset (tools/dataset/comments.jsonl) through the live LLM prompt and report
//! how well the model's DELETE / keep decisions match the labels. Used to iterate on the prompt.
use crate::llm::{LlmClient, Verdict};
use crate::types::{CommentBlock, Config, Language};
use crate::{parse, prose};
use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::task::JoinSet;

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
        other => return Err(anyhow!("unknown language {other}")),
    })
}

/// Turn a raw comment block (delimiters included) into the same prose the tool would send,
/// plus the extractive fallback the tool would use if the model fails.
fn prose_of(comment: &str, lang: Language) -> Result<(String, String)> {
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
    let a = prose::analyze(&block, lang, 20);
    Ok((a.text, a.extractive))
}

pub async fn run(dataset: &Path, cfg: &Config) -> Result<()> {
    let text = std::fs::read_to_string(dataset).context("reading dataset")?;
    let rows: Vec<Row> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).context("bad dataset row"))
        .collect::<Result<_>>()?;
    let llm = Arc::new(LlmClient::new(cfg));
    let max_words = cfg.max_summary_words;

    let mut set = JoinSet::new();
    for row in rows {
        let llm = llm.clone();
        set.spawn(async move {
            let lang = language(&row.language)?;
            let (p, fallback) = prose_of(&row.comment, lang)?;
            // Mirror the runtime: a failed model call falls back to the extractive line.
            let v = match llm.summarize(&p, &row.context, max_words).await {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("{}: fallback ({e})", row.id);
                    Verdict::Line(fallback)
                }
            };
            Ok::<_, anyhow::Error>((row, v))
        });
    }

    let (mut n, mut agree, mut exp_del, mut got_del, mut both_del, mut kept_words) =
        (0, 0, 0, 0, 0, 0);
    let mut results = Vec::new();
    while let Some(r) = set.join_next().await {
        match r? {
            Ok(x) => results.push(x),
            Err(e) => eprintln!("row failed: {e:#}"),
        }
    }
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
    let t = &llm.tokens;
    let (rq, pt, ct, ca) = (
        t.requests.load(Ordering::Relaxed),
        t.prompt.load(Ordering::Relaxed),
        t.completion.load(Ordering::Relaxed),
        t.cached.load(Ordering::Relaxed),
    );
    println!(
        "tokens: {rq} requests, {pt} prompt ({:.0}/req, {ca} cached), {ct} completion ({:.1}/req)",
        pt as f64 / rq.max(1) as f64,
        ct as f64 / rq.max(1) as f64
    );
    Ok(())
}
