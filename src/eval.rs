//! Run a labeled dataset (tools/dataset/comments.jsonl or tools/dataset/docstrings.jsonl) through
//! the live LLM prompt and report how well the model's decisions match the labels. Used to
//! iterate on the prompt.
use crate::judge::{Generative, Judge, Judgement, shorter, verify_texts};
use crate::llm::{DocRequest, DocVerdict, LlmClient, Verdict};
use crate::policy::{self, Thresholds, Verdict as Policy};
use crate::types::{CommentBlock, Config, JudgeKind, Language};
use crate::{parse, prose};
use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
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
        "rust" => Language::Rust,
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

/// True when the first row of `dataset` is a v2 row (`labels` and `comment_kind`).
pub fn is_v2(dataset: &Path) -> Result<bool> {
    let text = std::fs::read_to_string(dataset).context("reading dataset")?;
    let first: Option<Value> = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(serde_json::from_str)
        .transpose()
        .context("bad dataset row")?;
    Ok(first.is_some_and(|r| r.get("labels").is_some() && r.get("comment_kind").is_some()))
}

/// The rewrite the pipeline would apply, delimiters included: the generator's text written over
/// the whole comment, all its blocks as one. None when the generator gives no text or the edit
/// would be unsafe.
fn rewrite_candidate(
    row: &Value,
    lang: Language,
    writer: &Generative,
    state: &str,
    verdict: Policy,
) -> Result<Option<String>> {
    let text = row["comment"].as_str().unwrap_or("");
    let Ok(comments) = parse::extract_comments(text, lang) else {
        return Ok(None);
    };
    let blocks = parse::group_blocks(text, comments);
    if lang == Language::Python && blocks.is_empty() {
        // A docstring row: alone in a file, the string is a module docstring.
        let docs = crate::docstring::extract_docstrings(text, false, &[])?;
        let (Some(d), Some(reply)) = (docs.first(), writer.rewrite(state, verdict.as_str())?)
        else {
            return Ok(None);
        };
        return Ok(crate::docstring::replace_edit(text, d, &reply)
            .map(|e| e.replacement.trim().to_string()));
    }
    let (Some(first), Some(last)) = (blocks.first(), blocks.last()) else {
        return Ok(None);
    };
    let block = CommentBlock {
        comments: blocks.iter().flat_map(|b| b.comments.clone()).collect(),
        end: last.end,
        end_line: last.end_line,
        code_after: last.code_after,
        ..first.clone()
    };
    let Some(reply) = writer.rewrite(state, verdict.as_str())? else {
        return Ok(None);
    };
    Ok(crate::rewrite::replace_edit(text, &block, lang, &reply)
        .map(|e| e.replacement.trim().to_string()))
}

/// Judge one v2 row and build its run file (the `tools/bakeoff/run_judge.py` shape).
fn judge_row(
    row: &Value,
    judge: &dyn Judge,
    writer: &Generative,
    th: &Thresholds,
    cfg: &Config,
) -> Result<Value> {
    let kind = row["comment_kind"].as_str().unwrap_or("");
    let comment = row["comment"].as_str().unwrap_or("");
    let state = crate::render::render_state(
        &crate::render::StateRow::from_v2_row(row),
        crate::render::MAX_STATE_TOKENS,
    );
    let questions = crate::render::questions_for(crate::render::SCHEMA_STRICT, Some(kind));
    let j = judge.judge(&state, &questions)?;
    let v = policy::judge_verdict(
        &j,
        judge.one_hot(),
        kind,
        comment,
        row["annotated_code"].as_str().unwrap_or(""),
        th,
    );
    let (mut rewrite, mut scores) = (None, None);
    // As in reduction::judge_item, only System One can verify a rewrite.
    if matches!(v, Policy::RewriteShorter | Policy::FixWrongClaim)
        && cfg.judge == JudgeKind::SystemOne
        && th.rewrite
    {
        let lang = language(row["language"].as_str().unwrap_or(""))?;
        rewrite = rewrite_candidate(row, lang, writer, &state, v)?;
        if let Some(c) = &rewrite
            && (v == Policy::FixWrongClaim || shorter(c, comment))
        {
            let s = |k: &str| row[k].as_str().unwrap_or("");
            let out = verify_texts(
                judge,
                comment,
                c,
                s("annotated_code"),
                s("enclosing_scope"),
                th,
            )?;
            // Missing answers are left out, not null: score.py compares these with 0.5.
            let p: serde_json::Map<String, Value> = ["adds", "g1_candidate", "preserves"]
                .into_iter()
                .filter_map(|q| Some((q.into(), json!(out.answers.get(q)?.get("true")?))))
                .collect();
            scores = Some(p);
        }
    }
    Ok(json!({
        // policy.py's GENERATIVE_JUDGES matches "generative", not "generative:<model>".
        "id": row["id"], "judge": if judge.one_hot() { "generative".into() } else { judge.name() },
        "answers": j.answers, "verdict": j.holistic,
        "rewrite": rewrite, "latency_ms": j.latency_ms, "input_tokens": j.input_tokens,
        "raw": null, "rewrite_scores": scores,
    }))
}

/// Run the judge over a v2 dataset and write one `<id>.json` run file per row into `runs_dir`.
/// Score the directory with `tools/bakeoff/score.py`. Rows that fail are reported and left out.
pub fn run_v2(dataset: &Path, cfg: &Config, runs_dir: Option<&Path>) -> Result<()> {
    let runs_dir = runs_dir.context("a v2 dataset needs --eval-runs DIR")?;
    let judge = crate::reduction::make_judge(cfg).context(
        "a v2 dataset needs a judge other than legacy (--judge systemone or generative)",
    )?;
    judge.check()?;
    std::fs::create_dir_all(runs_dir).context("creating runs directory")?;
    let text = std::fs::read_to_string(dataset).context("reading dataset")?;
    let rows: Vec<Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).context("bad dataset row"))
        .collect::<Result<_>>()?;
    let (writer, th) = (crate::judge::generator(cfg), Thresholds::load(cfg)?);
    let outcomes = crate::parallel(rows, cfg.llm_concurrency, |row| -> Result<Value> {
        let id = row["id"].as_str().context("row without id")?;
        anyhow::ensure!(
            !id.is_empty() && !id.contains(['/', '\\']) && !id.starts_with('.'),
            "unsafe id {id}"
        );
        let run = judge_row(row, judge.as_ref(), &writer, &th, cfg)?;
        std::fs::write(runs_dir.join(format!("{id}.json")), run.to_string())?;
        Ok(run)
    });
    let total = outcomes.len();
    let (mut failed, mut runs, mut gold) = (0, HashMap::new(), Vec::new());
    for (row, r) in outcomes {
        match r.map_err(|m| anyhow!(m)).and_then(|r| r) {
            Ok(run) => {
                runs.insert(row["id"].as_str().unwrap_or("").to_string(), run);
                gold.push(row);
            }
            Err(m) => {
                failed += 1;
                eprintln!("{}: {m:#}", row["id"].as_str().unwrap_or("?"));
            }
        }
    }
    println!(
        "{}/{total} rows judged -> {}",
        total - failed,
        runs_dir.display()
    );
    print_report(&score(&gold, &runs, &th), failed);
    anyhow::ensure!(failed == 0, "{failed} rows failed");
    Ok(())
}

const VERDICTS: [&str; 5] = [
    "keep",
    "rewrite_shorter",
    "delete",
    "rename_instead",
    "fix_wrong_claim",
];

/// The verdict `tools/bakeoff/score.py` derives from a run file: the policy applied to its answers.
fn run_verdict(row: &Value, run: &Value, th: &Thresholds) -> Policy {
    let j = Judgement {
        answers: serde_json::from_value(run["answers"].clone()).unwrap_or_default(),
        holistic: run["verdict"].as_str().map(str::to_string),
        input_tokens: None,
        latency_ms: 0,
    };
    let s = |k: &str| row[k].as_str().unwrap_or("");
    let kind = if row["comment_kind"].is_null() {
        "inline"
    } else {
        s("comment_kind")
    };
    policy::judge_verdict(
        &j,
        run["judge"] == "generative",
        kind,
        s("comment"),
        s("annotated_code"),
        th,
    )
}

fn macro_f1(pairs: &[(&str, &str)]) -> f64 {
    let f1s: Vec<f64> = VERDICTS
        .iter()
        .filter_map(|c| {
            let n = |f: &dyn Fn(&str, &str) -> bool| {
                pairs.iter().filter(|(g, s)| f(g, s)).count() as f64
            };
            let tp = n(&|g, s| g == *c && s == *c);
            let fp = n(&|g, s| g != *c && s == *c);
            let fnn = n(&|g, s| g == *c && s != *c);
            (tp + fp + fnn > 0.0).then(|| 2.0 * tp / (2.0 * tp + fp + fnn))
        })
        .collect();
    if f1s.is_empty() {
        0.0
    } else {
        f1s.iter().sum::<f64>() / f1s.len() as f64
    }
}

/// Gold option as the key the answer distributions use: `true`, `2`, `partly`.
fn opt_key(v: &Value) -> String {
    v.as_str().map_or_else(|| v.to_string(), str::to_string)
}

fn ratio(num: usize, den: usize) -> Value {
    if den == 0 {
        Value::Null
    } else {
        json!(num as f64 / den as f64)
    }
}

/// One group's metrics; `items` are (gold row, run file, verdict). Same keys as score.py's `summarize`.
fn summarize(items: &[(&Value, &Value, Policy)]) -> Value {
    let n = items.len();
    let pairs: Vec<(&str, &str)> = items
        .iter()
        .map(|(r, _, v)| (r["labels"]["verdict"].as_str().unwrap_or(""), v.as_str()))
        .collect();
    let (mut hz_pool, mut cost) = (0, 0.0);
    let mut c = [0usize; 6]; // review, hazard_lost, wrong_delete, churn, wrong_keep, error_introduced
    let mut confusion: BTreeMap<&str, BTreeMap<&str, usize>> =
        VERDICTS.iter().map(|g| (*g, BTreeMap::new())).collect();
    let mut qs: BTreeMap<&str, (Vec<bool>, Vec<f64>)> = BTreeMap::new();
    for ((row, run, _), (g, s)) in items.iter().zip(&pairs) {
        let lab = &row["labels"];
        let hazard = lab["hazard"].as_bool().unwrap_or(false);
        let removes = matches!(*s, "delete" | "rename_instead");
        let rewrites = matches!(*s, "rewrite_shorter" | "fix_wrong_claim");
        let review = *s == "review";
        let pool = hazard && matches!(*g, "keep" | "rewrite_shorter");
        hz_pool += pool as usize;
        let f = [
            review,
            !review && pool && removes,
            !review && !(pool && removes) && removes && !matches!(*g, "delete" | "rename_instead"),
            rewrites && *g == "keep",
            *s == "keep" && *g != "keep",
            rewrites
                && (run["rewrite_scores"]["adds"].as_f64().unwrap_or(0.0) > 0.5
                    || run["rewrite_scores"]["g1_candidate"]
                        .as_f64()
                        .unwrap_or(0.0)
                        > 0.5),
        ];
        for (k, v) in f.iter().enumerate() {
            c[k] += *v as usize;
        }
        // review, hazard_lost, wrong_delete, churn, wrong_keep, error_introduced
        cost += [0.5, 10.0, 1.0, 1.0, 0.3, 3.0]
            .iter()
            .zip(f)
            .map(|(w, b)| if b { *w } else { 0.0 })
            .sum::<f64>();
        if let Some(m) = confusion.get_mut(g) {
            *m.entry(*s).or_default() += 1;
        }
        for (q, dist) in run["answers"].as_object().into_iter().flatten() {
            let (Some(dist), false) = (dist.as_object(), lab[q.as_str()].is_null()) else {
                continue;
            };
            if dist.is_empty() {
                continue;
            }
            let gold = opt_key(&lab[q.as_str()]);
            let p = |v: &Value| v.as_f64().unwrap_or(0.0);
            // Python's max() keeps the first of equal options.
            let top = dist.iter().fold(None, |b: Option<(&String, f64)>, (k, v)| {
                if b.is_none_or(|(_, bp)| p(v) > bp) {
                    Some((k, p(v)))
                } else {
                    b
                }
            });
            let brier: f64 = dist
                .iter()
                .map(|(k, v)| (p(v) - (*k == gold) as u8 as f64).powi(2))
                .sum::<f64>()
                + if dist.contains_key(&gold) { 0.0 } else { 1.0 };
            let e = qs.entry(q).or_default();
            e.0.push(top.is_some_and(|(k, _)| *k == gold));
            e.1.push(brier);
        }
    }
    let rest: Vec<_> = pairs.iter().filter(|(_, s)| *s != "review").collect();
    let mut lat: Vec<f64> = items
        .iter()
        .filter_map(|(_, r, _)| r["latency_ms"].as_f64())
        .collect();
    lat.sort_by(f64::total_cmp);
    let tok: Vec<f64> = items
        .iter()
        .filter_map(|(_, r, _)| r["input_tokens"].as_f64())
        .collect();
    let median = match lat.len() {
        0 => Value::Null,
        l => json!((lat[(l - 1) / 2] + lat[l / 2]) / 2.0),
    };
    let questions: serde_json::Map<String, Value> = qs
        .into_iter()
        .map(|(q, (a, b))| {
            let k = a.len() as f64;
            let acc = a.iter().filter(|x| **x).count() as f64 / k;
            (
                q.into(),
                json!({"n": a.len(), "acc": acc, "brier": b.iter().sum::<f64>() / k}),
            )
        })
        .collect();
    json!({
        "n": n,
        "macro_f1": macro_f1(&pairs),
        "confusion": confusion,
        "hazards_lost": c[1],
        "hazard_pool": hz_pool,
        "hazards_lost_rate": ratio(c[1], hz_pool),
        "wrong_delete": c[2],
        "churn": c[3],
        "review_rate": ratio(c[0], n),
        "rest_accuracy": ratio(rest.iter().filter(|(g, s)| g == s).count(), rest.len()),
        "cost": cost,
        "questions": questions,
        "latency_median_ms": median,
        "latency_p95_ms": lat.get(((0.95 * lat.len() as f64) as usize).min(lat.len().saturating_sub(1))),
        "mean_input_tokens": if tok.is_empty() { Value::Null } else { json!(tok.iter().sum::<f64>() / tok.len() as f64) },
    })
}

/// score.py's `score`: overall plus per `language` and per `comment_kind`.
fn score(gold: &[Value], runs: &HashMap<String, Value>, th: &Thresholds) -> Value {
    let items: Vec<_> = gold
        .iter()
        .filter_map(|r| {
            let run = runs.get(r["id"].as_str()?)?;
            Some((r, run, run_verdict(r, run, th)))
        })
        .collect();
    let mut out = json!({"missing": gold.len() - items.len(), "overall": summarize(&items)});
    for key in ["language", "comment_kind"] {
        let vals: BTreeSet<&str> = items
            .iter()
            .map(|i| i.0[key].as_str().unwrap_or(""))
            .collect();
        out[key] = vals
            .into_iter()
            .map(|v| {
                let sub: Vec<_> = items
                    .iter()
                    .filter(|i| i.0[key].as_str().unwrap_or("") == v)
                    .cloned()
                    .collect();
                (v.to_string(), summarize(&sub))
            })
            .collect::<serde_json::Map<_, _>>()
            .into();
    }
    out
}

fn fmt(v: &Value, nd: usize) -> String {
    match v {
        Value::Null => "-".into(),
        Value::Number(n) if n.is_f64() => format!("{:.nd$}", n.as_f64().unwrap_or(0.0)),
        _ => v.to_string(),
    }
}

/// score.py's `print_report`, minus the judge name.
fn print_report(rep: &Value, failed: usize) {
    println!("== v2 eval (rows without a run: {})", rep["missing"]);
    let cols = [
        "n",
        "macro_f1",
        "hazards_lost",
        "hazards_lost_rate",
        "wrong_delete",
        "churn",
        "review_rate",
        "rest_accuracy",
        "cost",
    ];
    println!(
        "{:<22}{}",
        "group",
        cols.iter().map(|c| format!("{c:>18}")).collect::<String>()
    );
    let mut groups = vec![("overall".to_string(), &rep["overall"])];
    for key in ["language", "comment_kind"] {
        for (k, v) in rep[key].as_object().into_iter().flatten() {
            groups.push((format!("{}:{k}", &key[..4]), v));
        }
    }
    for (name, s) in groups {
        println!(
            "{name:<22}{}",
            cols.iter()
                .map(|c| format!("{:>18}", fmt(&s[*c], 3)))
                .collect::<String>()
        );
    }
    let o = &rep["overall"];
    println!("\nconfusion (rows gold, cols system):");
    let cl: Vec<&str> = VERDICTS.iter().copied().chain(["review"]).collect();
    println!(
        "{:<17}{}",
        "",
        cl.iter()
            .map(|c| format!("{:>12}", &c[..c.len().min(11)]))
            .collect::<String>()
    );
    for g in VERDICTS {
        let n = |c: &str| o["confusion"][g][c].as_u64().unwrap_or(0);
        println!(
            "{g:<17}{}",
            cl.iter()
                .map(|c| format!("{:>12}", n(c)))
                .collect::<String>()
        );
    }
    if let Some(qs) = o["questions"].as_object().filter(|q| !q.is_empty()) {
        println!("\nper-question:        n    acc   brier");
        for (q, s) in qs {
            println!(
                "{q:<18}{:>4}{:>7.3}{:>8.3}",
                s["n"],
                s["acc"].as_f64().unwrap_or(0.0),
                s["brier"].as_f64().unwrap_or(0.0)
            );
        }
    }
    println!(
        "\nlatency median {} ms, p95 {} ms, mean input tokens {}{}",
        fmt(&o["latency_median_ms"], 1),
        fmt(&o["latency_p95_ms"], 1),
        fmt(&o["mean_input_tokens"], 0),
        if failed > 0 {
            format!("\n{failed} rows failed and are not scored")
        } else {
            String::new()
        }
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn same(a: &Value, b: &Value, path: &str) {
        match (a, b) {
            (Value::Object(x), Value::Object(y)) => {
                assert_eq!(
                    x.keys().collect::<Vec<_>>(),
                    y.keys().collect::<Vec<_>>(),
                    "{path}"
                );
                x.iter()
                    .for_each(|(k, v)| same(v, &y[k], &format!("{path}.{k}")));
            }
            (Value::Number(x), Value::Number(y)) => {
                let (x, y) = (x.as_f64().unwrap(), y.as_f64().unwrap());
                assert!((x - y).abs() < 1e-9, "{path}: {x} vs {y}");
            }
            _ => assert_eq!(a, b, "{path}"),
        }
    }

    /// expected.json comes from `score.py --json` on the same files (see tests/fixtures/eval).
    #[test]
    fn metrics_match_score_py() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/eval");
        let read = |p: String| std::fs::read_to_string(p).unwrap();
        let gold: Vec<Value> = read(format!("{dir}/gold-synthetic.jsonl"))
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let runs: HashMap<String, Value> = std::fs::read_dir(format!("{dir}/runs"))
            .unwrap()
            .map(|f| {
                serde_json::from_str::<Value>(&read(f.unwrap().path().display().to_string()))
                    .unwrap()
            })
            .map(|r| (r["id"].as_str().unwrap().to_string(), r))
            .collect();
        let expected: Value = serde_json::from_str(&read(format!("{dir}/expected.json"))).unwrap();
        same(
            &score(&gold, &runs, &Thresholds::strict()),
            &expected["runs"],
            "report",
        );
    }
}
