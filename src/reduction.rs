use crate::{
    Config, FileResult, Item, Language, Plan, Stats,
    hook_model::{BlockTooLarge, Classifier},
    llm::LlmClient,
    parallel, plan_file,
    progress::Progress,
    rewrite,
    state::{Database, ItemRecord, Record, classifier_key, fingerprint},
};
use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

struct FilePlan {
    path: PathBuf,
    name: String,
    lang: Language,
    plan: Plan,
    records: Vec<Record>,
    failed: bool,
}

pub(crate) fn run(root: &Path, tracked: Vec<(PathBuf, Language)>, cfg: &Config) -> Result<Stats> {
    let mut database = Database::open(root, cfg.database.as_deref())?;
    eprintln!("state: {}", database.path.display());
    let llm_key = fingerprint(&[
        include_str!("llm.rs"),
        &cfg.endpoint,
        &cfg.model,
        &cfg.docstrings_model,
        &cfg.max_summary_words.to_string(),
    ]);
    let settings = fingerprint(&[
        &llm_key,
        &format!("{:?}", cfg.target),
        &format!("{:?}", cfg.language),
        &format!("{:?}{:?}", cfg.keep_decorators, cfg.keep_bases),
        include_str!("model/minilm-l12-classifier.json"),
        crate::hook_model::MODEL_SHA256,
    ]);
    let mut stats = Stats {
        files_scanned: tracked.len(),
        ..Stats::default()
    };
    let mut classifier = None;
    let mut plans = Vec::new();
    for (path, lang) in tracked {
        let planned = (|| -> Result<Option<FilePlan>> {
            ensure!(
                !std::fs::symlink_metadata(&path)?.file_type().is_symlink(),
                "source file is a symlink"
            );
            let path = path.canonicalize()?;
            let name = path.to_str().context("file path is not UTF-8")?.to_owned();
            // Plan before checking checkpoints so structural/config exemptions and symlink safety still apply.
            let plan = plan_file(&path, lang, cfg)?;
            if let Some(changed) = database.resume(&name, &settings, &plan.source)? {
                stats.files_resumed += 1;
                stats.files_changed += changed as usize;
                return Ok(None);
            }
            let source_hash = fingerprint(&[&plan.source]);
            let mut records = Vec::new();
            let mut failed = false;
            let tree = if lang == Language::Python {
                let mut parser = tree_sitter::Parser::new();
                parser.set_language(&tree_sitter_python::LANGUAGE.into())?;
                Some(
                    parser
                        .parse(&plan.source, None)
                        .context("Python parse failed")?,
                )
            } else {
                None
            };
            for item in &plan.items {
                let (start, end, start_line, end_line) = item.range();
                let text = item.text();
                let context = tree
                    .as_ref()
                    .map(|t| {
                        crate::hook_git::context_for_range(
                            &plan.source,
                            t,
                            start,
                            end,
                            start_line,
                            end_line,
                        )
                    })
                    .unwrap_or_default();
                let key = classifier_key(item.kind(), &text, &context);
                let decision = if lang != Language::Python {
                    Ok((true, "DIRECT"))
                } else if let Some((flag, _)) = database.classification(&key)? {
                    stats.classifier_cached += 1;
                    Ok((flag, if flag { "FLAG" } else { "PASS" }))
                } else {
                    if classifier.is_none() {
                        crate::hook_model::install_model()?;
                        classifier = Some(Classifier::load()?);
                    }
                    stats.screened += 1;
                    let classifier = classifier.as_mut().unwrap();
                    match classifier.probability(item.kind(), &text, &context) {
                        Ok(p) => database
                            .remember_classification(&key, p, classifier.threshold())
                            .map(|flag| (flag, if flag { "FLAG" } else { "PASS" })),
                        Err(error) if error.is::<BlockTooLarge>() => Ok((true, "DIRECT")),
                        Err(error) => Err(error),
                    }
                };
                let classification = decision.as_ref().map_or("ERROR", |d| d.1);
                let record = database.item(&ItemRecord {
                    file: &name,
                    source_hash: &source_hash,
                    start,
                    end,
                    start_line,
                    end_line,
                    kind: item.kind(),
                    classifier_key: &key,
                    llm_key: &llm_key,
                    classification,
                })?;
                match decision {
                    Ok((true, _)) if record.ready => {
                        stats.verdicts_cached += 1;
                    }
                    Err(error) => {
                        database.error(record.id, &format!("{error:#}"))?;
                        eprintln!(
                            "warning: {}:{start_line}: classifier failed ({error:#})",
                            path.display()
                        );
                        stats.classifier_errors += 1;
                        failed = true;
                    }
                    _ => {}
                }
                records.push(record);
            }
            Ok(Some(FilePlan {
                path,
                name,
                lang,
                plan,
                records,
                failed,
            }))
        })();
        match planned {
            Ok(Some(p)) => plans.push(p),
            Ok(None) => {}
            Err(e) => {
                eprintln!("warning: skipping {} ({e:#})", path.display());
                stats.files_skipped += 1;
            }
        }
    }
    let mut doc_cfg = cfg.clone();
    doc_cfg.model = cfg.docstrings_model.clone();
    let comments = LlmClient::new(cfg);
    let docs = LlmClient::new(&doc_cfg);
    let mut jobs = Vec::new();
    for (file, plan) in plans.iter().enumerate().filter(|(_, p)| !p.failed) {
        for (index, (item, record)) in plan.plan.items.iter().zip(&plan.records).enumerate() {
            if !record.ready {
                jobs.push((file, index, matches!(item, Item::Docstring(_))));
            }
        }
    }
    // Cached verdicts and all-PASS runs need no live LLM.
    if jobs.iter().any(|(_, _, doc)| !doc) {
        comments.check()?;
    }
    if jobs.iter().any(|(_, _, doc)| *doc)
        && (docs_model_differs(cfg) || !jobs.iter().any(|(_, _, doc)| !doc))
    {
        docs.check()?;
    }
    eprintln!(
        "{} files, {} flagged items to send to the LLM",
        plans.len(),
        jobs.len()
    );
    let progress = Progress::new(jobs.len(), plans.len(), Some(&comments.tokens));
    let db = Mutex::new(database);
    let results = parallel(
        jobs,
        cfg.llm_concurrency,
        |&(file, index, is_doc)| -> Result<Record> {
            let plan = &plans[file];
            let item = &plan.plan.items[index];
            let id = plan.records[index].id;
            db.lock().unwrap().running(id)?;
            let outcome = item.reduce(
                &plan.plan.source,
                plan.lang,
                if is_doc { &docs } else { &comments },
                cfg,
            );
            progress.tick();
            match outcome {
                Ok((disposition, edit)) => {
                    let (disposition, edit) = match edit {
                        Some(e) if e.replacement != plan.plan.source[e.start..e.end] => {
                            (disposition, Some(e))
                        }
                        _ => ("keep", None),
                    };
                    db.lock().unwrap().verdict(id, disposition, edit.as_ref())?;
                    Ok(Record {
                        id,
                        ready: true,
                        result_type: Some(disposition.into()),
                        edit,
                    })
                }
                Err(e) => {
                    db.lock().unwrap().error(id, &format!("{e:#}"))?;
                    Err(e)
                }
            }
        },
    );
    for ((file, index, _), result) in results {
        match result {
            Ok(Ok(record)) => plans[file].records[index] = record,
            other => {
                let error = match other {
                    Ok(Err(e)) => format!("{e:#}"),
                    Err(e) => e,
                    _ => unreachable!(),
                };
                progress.warn(format!(
                    "{}:{}: LLM failed ({error})",
                    plans[file].path.display(),
                    plans[file].plan.items[index].range().2
                ));
                plans[file].failed = true;
                stats.llm_errors += 1;
            }
        }
    }
    let mut database = db.into_inner().unwrap();
    for plan in plans {
        let result = (|| -> Result<FileResult> {
            let mut result = FileResult {
                kept: plan.plan.kept,
                ..FileResult::default()
            };
            if plan.failed {
                result.kept += plan.plan.items.len();
                return Ok(result);
            }
            let mut edits = Vec::new();
            for (item, record) in plan.plan.items.iter().zip(&plan.records) {
                ensure!(record.ready, "unfinished item checkpoint");
                let (_, _, start, end) = item.range();
                match record.result_type.as_deref() {
                    Some("delete") => {
                        result.deleted += 1;
                        result.lines_deleted += end - start + 1;
                    }
                    Some("reduce") => {
                        result.reduced += 1;
                        result.lines_reduced += (end - start + 1).saturating_sub(
                            record
                                .edit
                                .as_ref()
                                .map_or(0, |e| e.replacement.lines().count()),
                        );
                    }
                    _ => result.kept += 1,
                }
                if cfg.verbose {
                    progress.print(format!(
                        "{}:{start}: {}",
                        plan.path.display(),
                        record.result_type.as_deref().unwrap_or("pending")
                    ));
                }
                if let Some(edit) = &record.edit {
                    ensure!(
                        edit.start <= edit.end
                            && edit.end <= plan.plan.source.len()
                            && plan.plan.source.is_char_boundary(edit.start)
                            && plan.plan.source.is_char_boundary(edit.end),
                        "invalid cached edit range"
                    );
                    edits.push(edit.clone());
                }
            }
            let after = rewrite::apply(&plan.plan.source, edits);
            crate::parse::extract_comments(&after, plan.lang)
                .context("rewrite would introduce a parse error")?;
            ensure!(
                std::fs::read_to_string(&plan.path)? == plan.plan.source,
                "file changed during analysis; rerun to rescan"
            );
            database.prepare(
                &plan.name,
                &settings,
                &plan.plan.source,
                &after,
                &plan.records.iter().map(|r| r.id).collect::<Vec<_>>(),
            )?;
            if after != plan.plan.source {
                crate::atomic_write(&plan.path, &plan.plan.source, &after)?;
                result.changed = true;
            }
            database.complete(&plan.name, &settings)?;
            if let Some(line) = result.summary_line(&plan.path) {
                progress.print(line);
            }
            Ok(result)
        })();
        match result {
            Ok(r) => stats.merge(r),
            Err(e) => {
                progress.warn(format!("skipping {} ({e:#})", plan.path.display()));
                stats.files_skipped += 1;
            }
        }
        progress.file_done();
    }
    progress.finish();
    stats.elapsed = progress.elapsed();
    let mut tokens = comments.tokens.snapshot();
    let d = docs.tokens.snapshot();
    tokens.requests += d.requests;
    tokens.prompt += d.prompt;
    tokens.completion += d.completion;
    tokens.cached += d.cached;
    stats.tokens = Some(tokens);
    Ok(stats)
}

fn docs_model_differs(cfg: &Config) -> bool {
    cfg.model != cfg.docstrings_model
}
