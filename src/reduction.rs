use crate::{
    Config, FileResult, Finding, Item, ItemState, JudgeKind, Language, Plan, Rewrite, Stats,
    changeset::{Base, ChangedFile, Changeset, Tier},
    hook_model::{BlockTooLarge, Classifier},
    judge::{Generative, Judge, SystemOne, generator, shorter, verify_rewrite},
    llm::LlmClient,
    parallel, parse, plan_file,
    policy::{self, Stale, Thresholds, Verdict},
    progress::Progress,
    render,
    rewrite::{self, Edit},
    state::{Database, ItemRecord, Record, classifier_key, fingerprint},
};
use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Cache key for LLM verdicts. With `Legacy` it is exactly the original key, so cached verdicts
/// survive; judge settings and the judge-path sources join it only for other judges. The legacy
/// key hashes all of llm.rs, so keep code the legacy path does not use out of that file.
fn llm_key(cfg: &Config) -> String {
    let base = [
        include_str!("llm.rs"),
        &cfg.endpoint,
        &cfg.model,
        &cfg.docstrings_model,
        &cfg.max_summary_words.to_string(),
    ];
    if cfg.judge == JudgeKind::Legacy {
        return fingerprint(&base);
    }
    let mut parts = base.to_vec();
    let judge = format!("{:?}", cfg.judge);
    let escalation = format!("{:?}", cfg.escalation_model);
    let endpoint = format!("{:?}", cfg.judge_endpoint);
    parts.extend([
        include_str!("judge.rs"),
        include_str!("policy.rs"),
        include_str!("context.rs"),
        include_str!("evidence.rs"),
        include_str!("render.rs"),
        include_str!("schema/frontier.md"),
        include_str!("schema/verify-v1.json"),
        include_str!("schema/rewrite.md"),
        crate::render::SCHEMA_STRICT,
        crate::render::SCHEMA_STALE,
        crate::policy::THRESHOLDS_STRICT,
        &judge,
        &endpoint,
        &cfg.judge_model,
        &escalation,
        &cfg.generator_model,
        cfg.judge_thresholds.as_deref().unwrap_or(""),
        if cfg.ai { "ai" } else { "" },
    ]);
    fingerprint(&parts)
}

/// The key of a file's write checkpoint. A changeset run judges only part of a file, so its
/// checkpoint is per range: a later whole-tree run must not skip the file as completed.
fn settings_key(cfg: &Config, llm_key: &str, changes: Option<&Changeset>) -> String {
    let settings = fingerprint(&[
        llm_key,
        &format!("{:?}", cfg.target),
        // One language keys exactly as before the flag became repeatable, so saved progress holds.
        &match cfg.language.as_slice() {
            [one] => format!("{one:?}"),
            many => format!("{many:?}"),
        },
        &format!("{:?}{:?}", cfg.keep_decorators, cfg.keep_bases),
        include_str!("model/minilm-l12-classifier.json"),
        crate::hook_model::MODEL_SHA256,
    ]);
    match changes {
        None => settings,
        Some(cs) => fingerprint(&[&settings, "changed", &format!("{:?}", cs.ranges)]),
    }
}

/// What `run` works on. `changes`: judge only tiers 1 and 2 of these files. `report_only`:
/// store verdicts and report findings, but never resume, read or write the working tree.
pub(crate) struct RunMode<'a> {
    pub changes: Option<&'a Changeset>,
    pub report_only: bool,
}

struct FilePlan<'a> {
    change: Option<&'a ChangedFile>,
    path: PathBuf,
    name: String,
    lang: Language,
    plan: Plan,
    records: Vec<Record>,
    /// Per-item context and rendered state; empty for whole-tree legacy runs.
    states: Vec<ItemState>,
    failed: bool,
}

/// Judges one item and maps the policy verdict to the same (disposition, edit) pair as
/// `Item::reduce`, so everything downstream is unchanged.
#[allow(clippy::too_many_arguments)]
fn judge_item(
    item: &Item,
    st: &ItemState,
    plan: &FilePlan,
    writer: &Generative,
    judge: &dyn Judge,
    escalation: Option<&dyn Judge>,
    escalated: &AtomicUsize,
    th: &Thresholds,
    cfg: &Config,
    db: &Mutex<Database>,
    id: i64,
    progress: &Progress,
) -> Result<(&'static str, Option<Edit>)> {
    let source = &plan.plan.source;
    let warn = |reason: String| {
        progress.warn(format!(
            "{}:{}: rewrite rejected ({reason})",
            plan.path.display(),
            item.range().2
        ))
    };
    // Tier 2 (only items the gate passed get here): "does the changed code contradict it?"
    // It reports, never edits.
    if st.tier == Some(Tier::ScopeChanged) {
        let questions = render::questions_for(render::SCHEMA_STALE, None);
        let j = judge.judge(&st.state, &questions)?;
        let (v, checks) = policy::stale_verdict(&j.answers, th);
        let mut ledger = serde_json::to_value(&j.answers)?;
        ledger["checks"] = serde_json::to_value(&checks)?;
        db.lock()
            .unwrap()
            .store_answers(id, &ledger.to_string(), v.as_str(), &judge.name())?;
        return Ok((if v == Stale::Stale { "stale" } else { "keep" }, None));
    }
    let kind = st.ctx.comment_kind.as_str();
    let questions = render::questions_for(render::SCHEMA_STRICT, Some(kind));
    let j = judge.judge(&st.state, &questions)?;
    let (mut v, mut checks) = policy::judge_verdict_with_checks(
        &j,
        judge.one_hot(),
        kind,
        &st.ctx.comment,
        &st.ctx.annotated_code,
        th,
    );
    let mut ledger = serde_json::to_value(&j.answers)?;
    let mut decider = judge.name();
    // A second, stronger opinion on undecided items; if it is also undecided the item stays review.
    if v == Verdict::Review
        && let Some(esc) = escalation
    {
        let e = esc.judge(&st.state, &questions)?;
        (v, checks) = policy::judge_verdict_with_checks(
            &e,
            esc.one_hot(),
            kind,
            &st.ctx.comment,
            &st.ctx.annotated_code,
            th,
        );
        ledger["escalation"] = serde_json::to_value(&e.answers)?;
        decider = esc.name();
    }
    ledger["checks"] = serde_json::to_value(&checks)?;
    let outcome = match v {
        Verdict::Keep => ("keep", None),
        Verdict::Delete => ("delete", item.delete(source)),
        // Report only: renaming code is not safe to automate.
        Verdict::RenameInstead => ("rename", None),
        Verdict::Review => ("review", None),
        // The agent writes rewrites and fixes; we only say which.
        Verdict::RewriteShorter if cfg.ai => ("rewrite", None),
        Verdict::FixWrongClaim if cfg.ai => ("fix", None),
        Verdict::RewriteShorter if !th.rewrite => ("keep", None),
        Verdict::FixWrongClaim if !th.rewrite => ("review", None),
        // Only System One can verify a rewrite; other judges' rewrites go to a person.
        Verdict::RewriteShorter | Verdict::FixWrongClaim if cfg.judge != JudgeKind::SystemOne => {
            ("review", None)
        }
        Verdict::RewriteShorter | Verdict::FixWrongClaim => {
            let shorter = v == Verdict::RewriteShorter;
            match verified_rewrite(item, st, plan, writer, judge, th, shorter, &mut ledger)? {
                Ok(edit) => ("reduce", Some(edit)),
                // Never delete from a failed rewrite: the original stays.
                Err(reason) => {
                    warn(reason);
                    (
                        if v == Verdict::RewriteShorter {
                            "keep"
                        } else {
                            "review"
                        },
                        None,
                    )
                }
            }
        }
    };
    db.lock()
        .unwrap()
        .store_answers(id, &ledger.to_string(), v.as_str(), &decider)?;
    // Counted only on success, so a retried item is not counted twice.
    if ledger.get("escalation").is_some() {
        escalated.fetch_add(1, Ordering::Relaxed);
    }
    Ok(outcome)
}

/// Generates a rewrite and has the judge verify it. The outer error is a transport error (the
/// item is retried); the inner one is why the candidate was rejected.
#[allow(clippy::too_many_arguments)]
fn verified_rewrite(
    item: &Item,
    st: &ItemState,
    plan: &FilePlan,
    writer: &Generative,
    judge: &dyn Judge,
    th: &Thresholds,
    must_be_shorter: bool,
    ledger: &mut serde_json::Value,
) -> Result<Result<Edit, String>> {
    let source = &plan.plan.source;
    let verdict = if must_be_shorter {
        Verdict::RewriteShorter
    } else {
        Verdict::FixWrongClaim
    };
    let candidate = match writer.rewrite(&st.state, verdict.as_str())? {
        Some(text) => item.replacement(source, plan.lang, &text),
        None => Rewrite::Delete,
    };
    Ok(match candidate {
        Rewrite::Delete => Err("generator gave no text".to_string()),
        Rewrite::Unsafe(why) => Err(why.to_string()),
        Rewrite::Candidate(edit)
            if must_be_shorter && !shorter(&edit.replacement, &source[edit.start..edit.end]) =>
        {
            Err("not shorter".to_string())
        }
        Rewrite::Candidate(edit) => {
            let out = verify_rewrite(judge, &st.ctx.comment, edit.replacement.trim(), &st.ctx, th)?;
            ledger["verify"] = serde_json::to_value(&out.answers)?;
            out.reason.map_or(Ok(edit), Err)
        }
    })
}

/// The finding's text: what the verdict means for the comment.
fn finding_message(verdict: &str) -> &'static str {
    match verdict {
        "delete" => "says nothing the code does not",
        "reduce" => "rewritten",
        "review" => "undecided; needs a person",
        "rename" => "a rename could say this instead",
        "stale" => "contradicts the changed code",
        _ => "flagged",
    }
}

/// The `evidence:` block of a rendered state, one line.
fn evidence_line(state: &str) -> String {
    state
        .lines()
        .skip_while(|l| *l != "evidence:")
        .skip(1)
        .take_while(|l| l.contains(": "))
        .collect::<Vec<_>>()
        .join("; ")
}

pub(crate) fn make_judge(cfg: &Config) -> Option<Box<dyn Judge>> {
    match cfg.judge {
        JudgeKind::Legacy => None,
        JudgeKind::SystemOne => Some(Box::new(SystemOne::new(
            &cfg.judge_url(),
            &cfg.judge_model,
            cfg.api_key.as_deref(),
        ))),
        JudgeKind::Generative => Some(Box::new(Generative::new(cfg))),
    }
}

pub(crate) fn run(
    root: &Path,
    tracked: Vec<(PathBuf, Language)>,
    cfg: &Config,
    mode: RunMode,
) -> Result<Stats> {
    let judge = make_judge(cfg);
    let escalation = cfg.escalation_model.as_ref().and_then(|model| {
        make_judge(&Config {
            judge_model: model.clone(),
            ..cfg.clone()
        })
    });
    let escalated = AtomicUsize::new(0);
    let mut th = Thresholds::load(cfg)?;
    if cfg.ai {
        th.margin = th.margin.max(th.ai_margin);
    }
    let mut database = Database::open(root, cfg.database.as_deref())?;
    eprintln!("state: {}", database.path.display());
    let llm_key = llm_key(cfg);
    // Tier-2 verdicts never share a cache row with full-schema ones (which can be deletes).
    let stale_key = fingerprint(&[&llm_key, "stale", render::SCHEMA_STALE]);
    let settings = settings_key(cfg, &llm_key, mode.changes);
    let units: Vec<(PathBuf, Language, Option<&ChangedFile>)> = match mode.changes {
        // Changeset paths are relative to the repo top level, not to `root`.
        Some(cs) => cs
            .files
            .iter()
            .map(|f| (cs.root.join(&f.path), f.lang, Some(f)))
            .collect(),
        None => tracked.into_iter().map(|(p, l)| (p, l, None)).collect(),
    };
    let mut stats = Stats {
        files_scanned: units.len(),
        files_skipped: mode.changes.map_or(0, |c| c.not_utf8),
        ..Stats::default()
    };
    let mut classifier = None;
    let mut plans = Vec::new();
    for (path, lang, change) in units {
        let planned = (|| -> Result<Option<FilePlan>> {
            // Report-only never touches the working tree, where the file may not even exist.
            if !mode.report_only {
                ensure!(
                    !std::fs::symlink_metadata(&path)?.file_type().is_symlink(),
                    "source file is a symlink"
                );
            }
            let path = match path.canonicalize() {
                Ok(p) => p,
                Err(_) if mode.report_only => path.clone(),
                Err(e) => return Err(e.into()),
            };
            let name = path.to_str().context("file path is not UTF-8")?.to_owned();
            // Plan before checking checkpoints so structural/config exemptions and symlink safety still apply.
            let plan = match change {
                None => plan_file(&path, lang, cfg)?,
                Some(f) => crate::plan_source(&path, f.head_source.clone(), lang, cfg)?,
            };
            // Report-only skips resume: it can write a prepared rewrite to the working tree.
            if !mode.report_only {
                let disk = match change {
                    Some(_) => Some(std::fs::read_to_string(&path)?),
                    None => None,
                };
                let current = disk.as_deref().unwrap_or(&plan.source);
                // `--ai` re-reports a finished file's open advice instead of skipping it; its
                // cached verdicts keep that cheap.
                if !cfg.ai
                    && let Some(changed) = database.resume(&name, &settings, current)?
                {
                    stats.files_resumed += 1;
                    stats.files_changed += changed as usize;
                    return Ok(None);
                }
                if let Some(f) = change {
                    ensure!(
                        current == plan.source,
                        "working tree differs from {}; commit or stash, then rerun",
                        &f.head[..f.head.len().min(12)]
                    );
                }
            }
            let source_hash = fingerprint(&[&plan.source]);
            // A tier-2 verdict depends on what the range compared against, not only the head.
            let stale_key = fingerprint(&[
                &stale_key,
                change.and_then(|f| f.base_source.as_deref()).unwrap_or(""),
            ]);
            let mut records = Vec::new();
            let mut failed = false;
            let tree = parse::parse_tree(&plan.source, lang)?;
            let mut plan = plan;
            // Blame, context, evidence and rendering are judge-path work; whole-tree legacy runs
            // skip them. A changeset needs them for the tiers, whatever the judge.
            let mut states = match (change, mode.changes) {
                (Some(f), Some(cs)) => {
                    let base = f.base_source.as_ref().map_or_else(Base::default, |b| {
                        let old = f.old_path.as_deref().unwrap_or(&f.path);
                        Base::new(old, b.clone(), lang, cfg)
                    });
                    crate::plan_states(&plan, &tree, lang, &path, Some((f, &base, cs)))
                }
                _ if cfg.judge == JudgeKind::Legacy => Vec::new(),
                _ => crate::plan_states(&plan, &tree, lang, &path, None),
            };
            // Tier 3 is never screened, judged, stored or edited. `states` stays aligned with
            // `plan.items`.
            if change.is_some() {
                let items = std::mem::take(&mut plan.items);
                for (item, st) in items.into_iter().zip(std::mem::take(&mut states)) {
                    if st.tier == Some(Tier::Untouched) {
                        stats.untouched += 1;
                        plan.structural.push(item);
                    } else {
                        stats.changed_items += 1;
                        plan.items.push(item);
                        states.push(st);
                    }
                }
            }
            for (index, item) in plan.items.iter().enumerate() {
                let (start, end, start_line, end_line) = item.range();
                let stale = states
                    .get(index)
                    .is_some_and(|s| s.tier == Some(Tier::ScopeChanged));
                let text = item.text();
                // Non-Python items keep an empty context: it is part of the cached classifier key.
                let context = if lang == Language::Python {
                    crate::context::context_for_range(
                        &plan.source,
                        &tree,
                        lang,
                        start,
                        end,
                        start_line,
                        end_line,
                    )
                } else {
                    String::new()
                };
                let key = classifier_key(item.kind(), &text, &context);
                // The screener asks "is this bloat", not "is this still true": tier 2 skips it.
                let decision = if lang != Language::Python || stale {
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
                let mut record = database.item(&ItemRecord {
                    file: &name,
                    source_hash: &source_hash,
                    start,
                    end,
                    start_line,
                    end_line,
                    kind: item.kind(),
                    classifier_key: &key,
                    llm_key: if stale { &stale_key } else { &llm_key },
                    classification,
                })?;
                if let Some(item_state) = states.get(index) {
                    database.store_state(
                        record.id,
                        &item_state.state,
                        &item_state.context,
                        &item_state.evidence,
                    )?;
                }
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
                // Tier 2 is judged only when the change touched something the comment names,
                // and never without stale questions (the legacy judge): otherwise it is kept
                // and never becomes a job, so it can never reach `Item::reduce`.
                let gated_out = states
                    .get(index)
                    .and_then(|s| s.change.as_ref())
                    .is_some_and(|c| c.changed_mentions.is_empty());
                if stale && (gated_out || judge.is_none()) && !record.ready {
                    database.verdict(record.id, "keep", None)?;
                    record.ready = true;
                    record.result_type = Some("keep".into());
                }
                records.push(record);
            }
            Ok(Some(FilePlan {
                change,
                path,
                name,
                lang,
                plan,
                records,
                states,
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
    if let Some(judge) = &judge
        && !jobs.is_empty()
    {
        judge.check()?;
        if let Some(esc) = &escalation {
            esc.check()?;
        }
    }
    // The verified rewrite path (System One only) generates with `generator_model`.
    let writer = generator(cfg);
    if cfg.judge == JudgeKind::SystemOne && th.rewrite && !cfg.ai && !jobs.is_empty() {
        writer.check()?;
    }
    if judge.is_none() {
        if jobs.iter().any(|(_, _, doc)| !doc) {
            comments.check()?;
        }
        if jobs.iter().any(|(_, _, doc)| *doc)
            && (docs_model_differs(cfg) || !jobs.iter().any(|(_, _, doc)| !doc))
        {
            docs.check()?;
        }
    }
    eprintln!(
        "{} files, {} flagged items to send to the LLM",
        plans.len(),
        jobs.len()
    );
    let tokens = judge.as_ref().map_or(&comments.tokens, |j| j.tokens());
    let mut progress = Progress::new(jobs.len(), plans.len(), Some(tokens));
    progress.stderr_only = cfg.ai;
    let db = Mutex::new(database);
    let results = parallel(
        jobs,
        cfg.llm_concurrency,
        |&(file, index, is_doc)| -> Result<Record> {
            let plan = &plans[file];
            let item = &plan.plan.items[index];
            let id = plan.records[index].id;
            db.lock().unwrap().running(id)?;
            let outcome = match &judge {
                None => item.reduce(
                    &plan.plan.source,
                    plan.lang,
                    if is_doc { &docs } else { &comments },
                    cfg,
                ),
                Some(judge) => judge_item(
                    item,
                    &plan.states[index],
                    plan,
                    &writer,
                    judge.as_ref(),
                    escalation.as_deref(),
                    &escalated,
                    &th,
                    cfg,
                    &db,
                    id,
                    &progress,
                ),
            };
            progress.tick();
            match outcome {
                Ok((disposition, edit)) => {
                    let (disposition, edit) = match edit {
                        Some(e) if e.replacement != plan.plan.source[e.start..e.end] => {
                            (disposition, Some(e))
                        }
                        // Only a delete or reduce without a real edit is downgraded to keep.
                        _ if matches!(
                            disposition,
                            "review" | "rename" | "stale" | "rewrite" | "fix"
                        ) =>
                        {
                            (disposition, None)
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
    stats.escalated = escalated.load(Ordering::Relaxed);
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
    // `--ai` paths are relative to where the agent ran us.
    let cwd = std::env::current_dir()
        .and_then(|d| d.canonicalize())
        .unwrap_or_default();
    for plan in plans {
        let result = (|| -> Result<FileResult> {
            let mut result = FileResult {
                kept: plan.plan.structural.len(),
                ..FileResult::default()
            };
            // A partial file is never written, but report-only still reports the finished items.
            if plan.failed && !mode.report_only {
                result.kept += plan.plan.items.len();
                return Ok(result);
            }
            let mut edits = Vec::new();
            for (index, (item, record)) in plan.plan.items.iter().zip(&plan.records).enumerate() {
                if plan.failed && !record.ready {
                    result.kept += 1;
                    continue;
                }
                ensure!(record.ready, "unfinished item checkpoint");
                let (_, _, start, end) = item.range();
                let st = plan.states.get(index);
                let verdict = record.result_type.as_deref();
                // Defence in depth: tier 2 never edits, in any mode.
                if st.is_some_and(|s| s.tier == Some(Tier::ScopeChanged)) {
                    ensure!(
                        matches!(verdict, Some("keep" | "stale")) && record.edit.is_none(),
                        "line {start}: a comment whose code changed has a {} verdict",
                        verdict.unwrap_or("missing")
                    );
                }
                // Findings: changeset runs report every verdict; `--ai` reports what it left undone.
                let report = plan.change.is_some()
                    || (cfg.ai && !matches!(verdict, Some("delete" | "reduce")));
                if let (true, Some(st), Some(v)) = (report, st, verdict)
                    && v != "keep"
                {
                    let checks = database
                        .answers(record.id)?
                        .and_then(|a| serde_json::from_value(a.get("checks")?.clone()).ok())
                        .unwrap_or_default();
                    result.findings.push(Finding {
                        path: match plan.change {
                            Some(f) => f.path.clone(),
                            None => plan.path.strip_prefix(&cwd).unwrap_or(&plan.path).into(),
                        },
                        comment: st.ctx.comment.clone(),
                        start_line: start,
                        end_line: end,
                        tier: st.tier.unwrap_or(Tier::Added),
                        kind: st.ctx.comment_kind.as_str().to_owned(),
                        verdict: v.to_owned(),
                        checks,
                        // Tier 2's evidence is what the change touched in the comment.
                        evidence: match st.change.as_ref().filter(|_| v == "stale") {
                            Some(c) => format!("changed: {}", c.changed_mentions.join(", ")),
                            None => evidence_line(&st.state),
                        },
                        message: finding_message(v).to_owned(),
                    });
                }
                match verdict {
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
                    Some("review" | "rewrite" | "fix") => result.review += 1,
                    Some("rename") => result.rename += 1,
                    Some("stale") => result.stale += 1,
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
            if mode.report_only {
                return Ok(result);
            }
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
    let judged = judge.as_ref().map(|j| j.tokens().snapshot());
    let esc = escalation.as_ref().map(|j| j.tokens().snapshot());
    for d in [Some(docs.tokens.snapshot()), judged, esc]
        .into_iter()
        .flatten()
    {
        tokens.requests += d.requests;
        tokens.prompt += d.prompt;
        tokens.completion += d.completion;
        tokens.cached += d.cached;
    }
    stats.tokens = Some(tokens);
    Ok(stats)
}

fn docs_model_differs(cfg: &Config) -> bool {
    cfg.model != cfg.docstrings_model
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn cfg(judge: JudgeKind) -> Config {
        Config {
            target: crate::Target::All,
            language: vec![crate::Languages::All],
            mode: crate::Mode::Reduce,
            max_summary_words: 20,
            endpoint: "http://e/v1".into(),
            model: "m".into(),
            docstrings_model: "d".into(),
            api_key: None,
            llm_concurrency: 1,
            dry_run: false,
            verbose: false,
            database: None,
            ignore: vec![],
            keep_decorators: vec![],
            keep_bases: vec![],
            judge,
            judge_endpoint: None,
            judge_model: "j".into(),
            escalation_model: None,
            generator_model: "g".into(),
            judge_thresholds: None,
            ai: false,
        }
    }

    #[test]
    fn legacy_llm_key_is_unchanged_and_judge_settings_only_affect_other_judges() {
        let legacy = cfg(JudgeKind::Legacy);
        let original = fingerprint(&[include_str!("llm.rs"), "http://e/v1", "m", "d", "20"]);
        assert_eq!(llm_key(&legacy), original);
        let mut other = legacy.clone();
        other.judge_model = "z".into();
        assert_eq!(llm_key(&other), original);
        let mut judged = cfg(JudgeKind::SystemOne);
        let before = llm_key(&judged);
        assert_ne!(before, original);
        judged.judge_model = "z".into();
        assert_ne!(llm_key(&judged), before);
    }

    #[test]
    fn judge_thresholds_merge_over_embedded_and_join_the_key() {
        let mut judged = cfg(JudgeKind::SystemOne);
        let before = llm_key(&judged);
        judged.judge_thresholds = Some(r#"{"g1": 0.9}"#.into());
        assert_ne!(llm_key(&judged), before);
        let th = Thresholds::load(&judged).unwrap();
        assert_eq!((th.g1, th.c2_min), (0.9, Thresholds::strict().c2_min));
        judged.judge_thresholds = Some(r#"{"g2": 0.9}"#.into());
        assert!(Thresholds::load(&judged).is_err());
    }

    use crate::changeset::{self, tests::Repo};
    use serde_json::{Value, json};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::Arc;

    const BASE: &str = "function add(a, b) {\n  // returns the sum\n  return a + b;\n}\n\nfunction retry() {\n  // wait 300ms before retrying\n  sleep(300);\n}\n\nfunction untouched() {\n  // UNTOUCHED_NOTE keep this\n  return 1;\n}\n";
    const NEW: &str = "\n// NEW_NOTE set c\nconst c = 3;\n";

    /// A repo whose second commit adds one comment (tier 1), changes the code under two
    /// unchanged comments (tier 2) and leaves one function alone (tier 3).
    fn fixture() -> (Repo, String, String) {
        let repo = Repo::new();
        repo.write("a.js", BASE);
        let base = repo.commit();
        let head = BASE
            .replace("a + b", "a - b")
            .replace("sleep(300)", "sleep(500)")
            + NEW;
        repo.write("a.js", &head);
        let spec = format!("{base}..{}", repo.commit());
        (repo, spec, head)
    }

    type Calls = Arc<Mutex<Vec<(String, Value)>>>;

    /// An HTTP server for chat and System One requests; `handler` gets the path and body and
    /// returns the status and the System One body or the chat message text.
    fn mock(handler: impl Fn(&str, &Value) -> (u16, String) + Send + 'static) -> (String, Calls) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let calls: Calls = Arc::default();
        let seen = calls.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut r = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                let path = line.split(' ').nth(1).unwrap_or_default().to_owned();
                let mut len = 0;
                loop {
                    let mut l = String::new();
                    if r.read_line(&mut l).unwrap_or(0) == 0 || l == "\r\n" {
                        break;
                    }
                    if let Some(v) = l.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; len];
                r.read_exact(&mut body).unwrap();
                let body: Value = serde_json::from_slice(&body).unwrap_or_default();
                let (status, text) = handler(&path, &body);
                seen.lock().unwrap().push((path.clone(), body));
                let reply = if path.ends_with("/chat/completions") {
                    json!({"choices": [{"message": {"content": text}}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1}})
                    .to_string()
                } else {
                    text
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} R\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
            }
        });
        (url, calls)
    }

    /// System One: strict questions say delete (c2 = 0), stale questions give `g1`.
    fn systemone(g1: f64) -> impl Fn(&str, &Value) -> (u16, String) {
        move |path, r| {
            if path.ends_with("/chat/completions") {
                return (200, "K1 returns the difference".into());
            }
            let q = &r["questions"];
            let noul = |p: f64| json!({"type": "noul", "noul": p});
            let answers = if q.get("ping").is_some() {
                json!({"ping": noul(0.0)})
            } else if q.get("preserves").is_some() {
                json!({"preserves": noul(0.1), "adds": noul(0.0), "g1_candidate": noul(0.0)})
            } else if q.get("c2_info_gain").is_some() {
                // Fails the tier-3 item, so a whole-tree run writes nothing.
                if r["state"]
                    .as_str()
                    .unwrap_or("")
                    .contains("comment:\n// UNTOUCHED_NOTE")
                {
                    return (500, "down".into());
                }
                json!({"g1_contradicts": noul(0.0),
                    "c2_info_gain": {"type": "score", "probabilities": {"0": 1.0}},
                    "verdict": {"type": "choice", "probabilities": {"delete": 1.0}}})
            } else {
                json!({"g1_contradicts": noul(g1)})
            };
            (200, json!({ "answers": answers }).to_string())
        }
    }

    fn changed_run(repo: &Repo, spec: &str, cfg: &Config, report_only: bool) -> Stats {
        let cs = changeset::collect(
            &repo.0,
            Some(spec),
            None,
            None,
            &[crate::Languages::All],
            &[],
        )
        .unwrap();
        let mode = RunMode {
            changes: Some(&cs),
            report_only,
        };
        run(&repo.0, vec![], cfg, mode).unwrap()
    }

    fn with_endpoint(judge: JudgeKind, url: &str) -> Config {
        Config {
            endpoint: url.into(),
            ..cfg(judge)
        }
    }

    fn results(repo: &Repo) -> Vec<(String, String)> {
        let db =
            rusqlite::Connection::open(repo.0.join(".git/commentreducr/state.sqlite")).unwrap();
        let mut q = db
            .prepare("SELECT start_line, result_type FROM items ORDER BY start_line")
            .unwrap();
        q.query_map([], |r| Ok((r.get::<_, i64>(0)?.to_string(), r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn sent(calls: &Calls, needle: &str) -> bool {
        calls
            .lock()
            .unwrap()
            .iter()
            .any(|(_, b)| b.to_string().contains(needle))
    }

    /// Tier 1 is deleted; tier 2 is reported, never edited, and judged only when the change
    /// touched what it says (line 7's 300, not line 2's "sum"); tier 3 is never stored or sent.
    #[test]
    fn changed_run_judges_tiers_one_and_two_only() {
        let (repo, spec, head) = fixture();
        let (url, calls) = mock(systemone(0.9));
        let stats = changed_run(
            &repo,
            &spec,
            &with_endpoint(JudgeKind::SystemOne, &url),
            false,
        );
        let after = std::fs::read_to_string(repo.0.join("a.js")).unwrap();
        assert_eq!(after, head.replace("// NEW_NOTE set c\n", ""));
        let rows = results(&repo);
        let want =
            [("2", "keep"), ("7", "stale"), ("16", "delete")].map(|(a, b)| (a.into(), b.into()));
        assert_eq!(rows, want);
        // Other items' enclosing scope can show it; no request is about it.
        assert!(!sent(&calls, "comment:\\n// UNTOUCHED_NOTE"));
        assert!(sent(&calls, "comment:\\n// NEW_NOTE"));
        let stale: Vec<_> = calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, b)| b["questions"].get("g1_contradicts").is_some())
            .filter(|(_, b)| b["questions"].get("c2_info_gain").is_none())
            .map(|(_, b)| b.clone())
            .collect();
        assert_eq!(stale.len(), 1);
        for b in &stale {
            let ids: Vec<_> = b["questions"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect();
            assert_eq!(ids, ["g1_contradicts"]);
            let state = b["state"].as_str().unwrap();
            assert!(state.contains("annotated_code_before:") && state.contains("wait 300ms"));
        }
        let f: Vec<_> = stats
            .findings
            .iter()
            .map(|f| (f.start_line, f.tier, f.verdict.as_str()))
            .collect();
        assert_eq!(
            f,
            [
                (7, Tier::ScopeChanged, "stale"),
                (16, Tier::Added, "delete")
            ]
        );
        assert!(
            stats.findings[0]
                .checks
                .iter()
                .any(|c| c.0 == "g1" && c.1 == 0.9)
        );
        assert!(stats.findings[1].checks.iter().any(|c| c.0 == "c2"));
        assert_eq!(stats.findings[0].evidence, "changed: 300");
        assert_eq!(stats.comments_stale, 1);
    }

    #[test]
    fn changed_run_skips_a_dirty_file_and_resumes_a_completed_one() {
        let (repo, spec, head) = fixture();
        let (url, _) = mock(systemone(0.0));
        let cfg = with_endpoint(JudgeKind::SystemOne, &url);
        repo.write("a.js", &(head.clone() + "// local\n"));
        let stats = changed_run(&repo, &spec, &cfg, false);
        assert_eq!(stats.files_skipped, 1);
        assert_eq!(
            std::fs::read_to_string(repo.0.join("a.js")).unwrap(),
            head.clone() + "// local\n"
        );
        repo.write("a.js", &head);
        assert_eq!(changed_run(&repo, &spec, &cfg, false).files_skipped, 0);
        let again = changed_run(&repo, &spec, &cfg, false);
        assert_eq!((again.files_resumed, again.files_skipped), (1, 0));
    }

    /// Legacy: tier 1 reaches the chat model; tier 2 has no stale questions, so it is kept
    /// without a request.
    #[test]
    fn legacy_changed_run_keeps_tier_two() {
        let (repo, spec, _) = fixture();
        let (url, calls) = mock(|_, _| (200, "DELETE".into()));
        changed_run(&repo, &spec, &with_endpoint(JudgeKind::Legacy, &url), false);
        assert!(sent(&calls, "NEW_NOTE"));
        assert!(!sent(&calls, "returns the sum") && !sent(&calls, "before retrying"));
        let want =
            [("2", "keep"), ("7", "keep"), ("16", "delete")].map(|(a, b)| (a.into(), b.into()));
        assert_eq!(results(&repo), want);
    }

    /// A full-schema delete cached by a failed whole-tree run is never reused for tier 2.
    #[test]
    fn tier_two_never_reuses_a_cached_delete() {
        let (repo, spec, head) = fixture();
        let (url, _) = mock(systemone(0.0));
        let cfg = with_endpoint(JudgeKind::SystemOne, &url);
        let tracked = vec![(repo.0.join("a.js"), Language::JavaScript)];
        let mode = RunMode {
            changes: None,
            report_only: false,
        };
        assert!(run(&repo.0, tracked, &cfg, mode).unwrap().has_errors());
        assert_eq!(std::fs::read_to_string(repo.0.join("a.js")).unwrap(), head);
        changed_run(&repo, &spec, &cfg, false);
        let after = std::fs::read_to_string(repo.0.join("a.js")).unwrap();
        assert!(
            after.contains("returns the sum") && !after.contains("NEW_NOTE"),
            "{after}"
        );
    }

    /// Report-only writes nothing; a write-mode run afterwards reuses every verdict.
    #[test]
    fn report_only_stores_verdicts_and_writes_nothing() {
        let (repo, spec, head) = fixture();
        let (url, calls) = mock(systemone(0.9));
        let cfg = with_endpoint(JudgeKind::SystemOne, &url);
        let file = repo.0.join("a.js");
        let mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
        let stats = changed_run(&repo, &spec, &cfg, true);
        assert_eq!(stats.findings.len(), 2);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), head);
        assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), mtime);
        let n = calls.lock().unwrap().len();
        changed_run(&repo, &spec, &cfg, false);
        assert_eq!(calls.lock().unwrap().len(), n);
        assert!(!std::fs::read_to_string(&file).unwrap().contains("NEW_NOTE"));
    }

    /// A tier-2 verdict cached for one base is not reused for another base of the same head.
    #[test]
    fn tier_two_cache_depends_on_the_base() {
        let repo = Repo::new();
        repo.write("a.js", BASE);
        let y = repo.commit();
        repo.write("a.js", &BASE.replace("sleep(300)", "sleep(500);\n  x()"));
        let x = repo.commit();
        repo.write("a.js", &(BASE.replace("sleep(300)", "sleep(500)") + NEW));
        let h = repo.commit();
        let (url, _) = mock(systemone(0.9));
        let cfg = with_endpoint(JudgeKind::SystemOne, &url);
        let stale = |spec: &str| {
            changed_run(&repo, spec, &cfg, true)
                .findings
                .iter()
                .filter(|f| f.verdict == "stale")
                .count()
        };
        assert_eq!(stale(&format!("{x}..{h}")), 0);
        assert_eq!(stale(&format!("{y}..{h}")), 1);
    }

    /// Report-only keeps the findings of a file whose other judge call failed.
    #[test]
    fn report_only_keeps_findings_when_one_call_fails() {
        let (repo, spec, _) = fixture();
        let ok = systemone(0.9);
        let (url, _) = mock(move |path, r| {
            let q = &r["questions"];
            if q.get("g1_contradicts").is_some() && q.get("c2_info_gain").is_none() {
                return (500, "down".into());
            }
            ok(path, r)
        });
        let cfg = with_endpoint(JudgeKind::SystemOne, &url);
        let stats = changed_run(&repo, &spec, &cfg, true);
        assert_eq!(stats.llm_errors, 1);
        let f: Vec<_> = stats
            .findings
            .iter()
            .map(|f| (f.start_line, f.verdict.as_str()))
            .collect();
        assert_eq!(f, [(16, "delete")]);
    }

    /// Report-only never resumes a prepared write, which would write to the working tree.
    #[test]
    fn report_only_ignores_a_prepared_write() {
        let (repo, spec, head) = fixture();
        let (url, _) = mock(systemone(0.0));
        let cfg = with_endpoint(JudgeKind::SystemOne, &url);
        let cs = changeset::collect(
            &repo.0,
            Some(&spec),
            None,
            None,
            &[crate::Languages::All],
            &[],
        )
        .unwrap();
        let file = repo.0.join("a.js").canonicalize().unwrap();
        let settings = settings_key(&cfg, &llm_key(&cfg), Some(&cs));
        Database::open(&repo.0, None)
            .unwrap()
            .prepare(file.to_str().unwrap(), &settings, &head, "PREPARED\n", &[])
            .unwrap();
        changed_run(&repo, &spec, &cfg, true);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), head);
        // The row was live: a write-mode run recovers it.
        changed_run(&repo, &spec, &cfg, false);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "PREPARED\n");
    }
}
