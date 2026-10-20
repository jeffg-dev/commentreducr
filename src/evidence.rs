//! Text evidence about a comment: facts a judge can check against the code, never a verdict.
use crate::changeset::git_text;
use crate::context::{CommentKindV2, ItemContext};
use regex::Regex;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{LazyLock, Mutex};

#[derive(Debug, Clone, Default, Serialize)]
pub struct Evidence {
    pub mentions: Vec<Mention>,
    pub unresolved: Vec<String>,
    pub literal_mismatch: Vec<String>,
    pub overlap: Option<f64>,
    pub signature_echo: bool,
    pub todo_tracker: Option<bool>,
    // Filled by the blame step; null until then.
    pub comment_age_days: Option<u64>,
    pub code_age_days: Option<u64>,
    pub code_newer_than_comment: Option<bool>,
    pub authored_by_agent: Option<bool>,
    pub agent: Option<String>,
    /// Set when the unit guard fires: why the policy will not delete or rename this comment.
    pub unit_literal: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Mention {
    pub token: String,
    pub resolved: bool,
}

const STOP_WORDS: [&str; 30] = [
    "the", "a", "an", "and", "or", "of", "to", "in", "is", "it", "this", "that", "for", "on",
    "with", "as", "be", "are", "was", "by", "from", "if", "at", "not", "we", "its", "use", "so",
    "then", "when",
];

fn re(p: &str) -> Regex {
    Regex::new(p).unwrap()
}

static BACKTICK: LazyLock<Regex> = LazyLock::new(|| re(r"`([^`\s]+)`"));
static IDENT: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"\b[A-Za-z_]\w*\(\)|\b[A-Za-z_]\w*(?:\.[A-Za-z_]\w*)+|\b[A-Za-z0-9]+(?:_[A-Za-z0-9]+)+\b|\b[a-z]+[A-Z]\w*",
    )
});
static NUMBER: LazyLock<Regex> = LazyLock::new(|| re(r"\b\d+(?:\.\d+)?"));
static NUMBER_UNIT: LazyLock<Regex> = LazyLock::new(|| re(r"\b\d+(?:\.\d+)?[A-Za-z]*"));
static QUOTED: LazyLock<Regex> =
    LazyLock::new(|| re(r#"(?:^|[^\w"'])(?:"([^"\n]+)"|'([^'\n]+)')"#));
static WORD: LazyLock<Regex> = LazyLock::new(|| re(r"[A-Za-z][A-Za-z0-9]*"));
// The unit guard; `tools/bakeoff/policy.py` has the same regexes and the reason for the rule.
static UNIT_WORD: LazyLock<Regex> = LazyLock::new(|| {
    re(concat!(
        r"(?i:\b(?:milli|micro|nano)seconds?\b|\bseconds\b|\bmsecs?\b|\bms\b|\bminutes?\b|\bhours?\b|\bdays?\b",
        r"|\b(?:kilo|mega|giga)?bytes?\b|\bpx\b|\bpercent(?:age)?\b)|\b[KMGT]i?B\b|毫秒|秒|分钟|小时|天|字节",
    ))
});
static UNIT_AFTER_NUMBER: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i:\d+\s*(?:(?:s|secs?|second|mins?|h|hrs?)\b|%))"));
static TRACKER: LazyLock<Regex> = LazyLock::new(|| re(r"@\w+|\b[A-Z]{2,}-\d+\b|#\d+|https?://"));

fn is_stop(w: &str) -> bool {
    STOP_WORDS.contains(&w.to_ascii_lowercase().as_str())
}

/// Lowercased words with identifiers split on `_` and case boundaries; stop words and single
/// letters dropped.
fn tokens(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for w in WORD.find_iter(&text.replace('_', " ")) {
        let mut cur = String::new();
        let mut prev_lower = false;
        for c in w.as_str().chars() {
            if c.is_ascii_uppercase() && prev_lower {
                out.insert(std::mem::take(&mut cur));
            }
            prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
            cur.push(c.to_ascii_lowercase());
        }
        out.insert(cur);
    }
    out.retain(|t| t.len() > 1 && !is_stop(t));
    out
}

/// The unit guard: a comment naming a unit ("5 seconds", "in ms", "1小时"; a bare "second" is an
/// ordinal) over code with a number literal may be the only statement of that literal's unit, so
/// the policy never deletes or renames it. Returns the unit and the code's first literal.
pub(crate) fn unit_literal(comment: &str, code: &str) -> Option<String> {
    let u = UNIT_WORD
        .find(comment)
        .or_else(|| UNIT_AFTER_NUMBER.find(comment))?;
    let n = NUMBER.find(code)?;
    Some(format!(
        "unit {} next to literal {}",
        u.as_str().trim(),
        n.as_str()
    ))
}

/// Last dotted segment of an identifier-shaped token, without a trailing `()`.
fn key(token: &str) -> &str {
    let t = token.strip_suffix("()").unwrap_or(token);
    t.rsplit('.').next().unwrap_or(t)
}

pub(crate) fn text_evidence(
    ctx: &ItemContext,
    source: &str,
    item_bytes: (usize, usize),
) -> Evidence {
    let text = &ctx.comment;
    let (s, e) = (
        item_bytes.0.min(source.len()),
        item_bytes.1.min(source.len()),
    );
    // shortcut: resolution looks only in `source` outside the comment; the enclosing scope is a
    // subset of that text, so checking it separately would add nothing.
    let found = |k: &str| source[..s].contains(k) || source[e..].contains(k);

    let mut seen = HashSet::new();
    let spans = BACKTICK
        .captures_iter(text)
        .map(|c| c.get(1).unwrap().as_str())
        .chain(IDENT.find_iter(text).map(|m| m.as_str()));
    let mut mentions = Vec::new();
    for t in spans {
        let k = key(t);
        if k.len() >= 3 && !is_stop(k) && seen.insert(t) {
            mentions.push(Mention {
                token: t.to_owned(),
                resolved: found(k),
            });
        }
    }
    let unresolved = mentions
        .iter()
        .filter(|m| !m.resolved)
        .map(|m| m.token.clone())
        .collect();

    let code = &ctx.annotated_code;
    let code_numbers: HashSet<&str> = NUMBER.find_iter(code).map(|m| m.as_str()).collect();
    let mut literal_mismatch: Vec<String> = NUMBER_UNIT
        .find_iter(text)
        .map(|m| m.as_str())
        .filter(|m| {
            let bare = NUMBER.find(m).map_or(*m, |n| n.as_str());
            !code_numbers.contains(bare)
        })
        .map(str::to_owned)
        .collect();
    literal_mismatch.extend(
        QUOTED
            .captures_iter(text)
            .filter_map(|c| c.get(1).or(c.get(2)))
            .map(|m| m.as_str())
            .filter(|q| !code.contains(q))
            .map(str::to_owned),
    );

    let (ct, kt) = (tokens(text), tokens(code));
    let overlap = (!ct.is_empty() && !kt.is_empty())
        .then(|| ct.intersection(&kt).count() as f64 / ct.len() as f64);
    let signature_echo = ctx.comment_kind == CommentKindV2::ApiDoc
        && !ct.is_empty()
        && ct.is_subset(&tokens(code.lines().next().unwrap_or("")));
    let todo_tracker = (ctx.comment_kind == CommentKindV2::Todo).then(|| TRACKER.is_match(text));

    Evidence {
        mentions,
        unresolved,
        literal_mismatch,
        overlap,
        signature_echo,
        todo_tracker,
        unit_literal: unit_literal(text, code),
        ..Evidence::default()
    }
}

static CODE_IDENT: LazyLock<Regex> = LazyLock::new(|| re(r"[A-Za-z_][A-Za-z0-9_]*"));

/// Lowercase with `_` dropped, so `tryFoldAdd` and `try_fold_add` compare equal.
fn fold(ident: &str) -> String {
    ident.replace('_', "").to_ascii_lowercase()
}

/// The tier-2 gate: what in `comment` the change from `before` to `after` touched. An identifier
/// mention (backticked, dotted, snake or camel case, `f()`) counts when it is an identifier on a
/// line of `before` that `after_file` (the whole new file, so lines that only slid out of a
/// trimmed window still count as kept) no longer has (removed, renamed, or its condition or
/// arguments changed); a number or quoted string when `before` had it and `after` does not. Whitespace-only
/// line changes count for nothing. Empty means the change touched nothing the comment says.
// shortcut: plain-word claims ("returns the sum" over `a + b` -> `a - b`) never pass; add a
// claim-level check only if missed stale comments show up in validation.
pub(crate) fn changed_mentions(
    comment: &str,
    before: &str,
    after: &str,
    after_file: &str,
) -> Vec<String> {
    let squash = |l: &str| l.split_whitespace().collect::<Vec<_>>().join(" ");
    let kept: HashSet<String> = after_file.lines().map(squash).collect();
    let removed: HashSet<String> = before
        .lines()
        .map(squash)
        .filter(|l| !l.is_empty() && !kept.contains(l))
        .flat_map(|l| {
            CODE_IDENT
                .find_iter(&l)
                .map(|m| fold(m.as_str()))
                .collect::<Vec<_>>()
        })
        .collect();
    let numbers = |code: &str| -> HashSet<String> {
        NUMBER
            .find_iter(code)
            .map(|m| m.as_str().to_owned())
            .collect()
    };
    let (old_numbers, new_numbers) = (numbers(before), numbers(after));
    let mut out: Vec<String> = Vec::new();
    let mut push = |t: &str| {
        if !out.iter().any(|o| o == t) {
            out.push(t.to_owned());
        }
    };
    let spans = BACKTICK
        .captures_iter(comment)
        .map(|c| c.get(1).unwrap().as_str())
        .chain(IDENT.find_iter(comment).map(|m| m.as_str()));
    for t in spans {
        let k = key(t);
        if k.len() >= 3 && !is_stop(k) && removed.contains(&fold(k)) {
            push(t);
        }
    }
    for n in NUMBER.find_iter(comment).map(|m| m.as_str()) {
        if old_numbers.contains(n) && !new_numbers.contains(n) {
            push(n);
        }
    }
    for q in QUOTED
        .captures_iter(comment)
        .filter_map(|c| c.get(1).or(c.get(2)))
        .map(|m| m.as_str())
    {
        if before.contains(q) && !after.contains(q) {
            push(q);
        }
    }
    out
}

/// Per-file blame: 0-based line -> (commit sha, author time in unix seconds).
pub(crate) struct Blame {
    lines: Vec<Option<(String, i64)>>,
}

impl Blame {
    #[cfg(test)]
    pub(crate) fn line_count(&self) -> usize {
        self.lines.len()
    }
}

/// The git top-level directory of `dir`; None outside a repository.
pub(crate) fn repo_root(dir: &Path) -> Option<std::path::PathBuf> {
    let out = git_text(dir, &["rev-parse", "--show-toplevel"]).ok()?;
    Some(out.trim().into())
}

/// One `git blame --line-porcelain` per file. With `rev`, blames that revision and `path` is
/// relative to the repo root; otherwise it blames the working-tree file. None when the file is
/// untracked, the repo is missing, or git fails for any reason.
pub(crate) fn blame(repo: &Path, path: &Path, rev: Option<&str>) -> Option<Blame> {
    let path = match rev {
        Some(_) => path.to_path_buf(),
        None => path.canonicalize().ok()?,
    };
    let path = path.to_str()?;
    let mut args = vec!["blame", "--line-porcelain"];
    args.extend(rev);
    args.extend(["--", path]);
    let out = git_text(repo, &args).ok()?;
    let mut lines = Vec::new();
    let (mut sha, mut time) = (String::new(), None);
    for l in out.lines() {
        if l.starts_with('\t') {
            lines.push(time.map(|t| (std::mem::take(&mut sha), t)));
            time = None;
        } else if let Some(t) = l.strip_prefix("author-time ") {
            time = t.parse().ok();
        } else if l.len() > 40
            && l.as_bytes()[40] == b' '
            && l[..40].bytes().all(|c| c.is_ascii_hexdigit())
        {
            sha = l[..40].to_owned();
        }
    }
    Some(Blame { lines })
}

const TOOLS: [(&str, &str); 6] = [
    ("claude", "claude-code"),
    ("cursor", "cursor"),
    ("copilot", "copilot"),
    ("devin", "devin"),
    ("codex", "codex"),
    ("openai", "codex"),
];
const AGENT_DOMAINS: [(&str, &str); 5] = [
    ("anthropic.com", "claude-code"),
    ("cursor.sh", "cursor"),
    ("cursor.com", "cursor"),
    ("devin.ai", "devin"),
    ("openai.com", "codex"),
];

fn agent_of_email(email: &str) -> Option<&'static str> {
    let (local, domain) = email
        .trim()
        .to_ascii_lowercase()
        .rsplit_once('@')
        .map(|(l, d)| (l.to_owned(), d.to_owned()))?;
    // Never the bare github.com: noreply@github.com commits every web-UI merge and squash.
    if domain == "users.noreply.github.com" && local.contains("copilot") {
        return Some("copilot");
    }
    AGENT_DOMAINS
        .iter()
        .find(|(d, _)| domain == *d)
        .map(|(_, t)| *t)
}

/// The agent tool behind a commit: a `Co-Authored-By` trailer or an author/committer email.
pub(crate) fn agent_of(
    repo: &Path,
    sha: &str,
    cache: &Mutex<HashMap<String, Option<String>>>,
) -> Option<String> {
    if let Some(hit) = cache.lock().ok()?.get(sha) {
        return hit.clone();
    }
    let found =
        git_text(repo, &["log", "-1", "--format=%B%n%ae%n%ce", sha])
            .ok()
            .and_then(|log| {
                let lines: Vec<&str> = log.lines().collect();
                let emails = lines.iter().rev().take(2).copied();
                let trailers = lines
                    .iter()
                    .filter(|l| l.to_ascii_lowercase().starts_with("co-authored-by:"))
                    .map(|l| l.split_once(':').map_or("", |x| x.1));
                let by_name = trailers.clone().find_map(|t| {
                    let name = t.split('<').next().unwrap_or("").to_ascii_lowercase();
                    TOOLS
                        .iter()
                        .find(|(n, _)| name.contains(n))
                        .map(|(_, t)| *t)
                });
                by_name
                    .or_else(|| {
                        emails
                            .chain(trailers.filter_map(|t| {
                                t.split_once('<').map(|x| x.1.trim_end_matches('>'))
                            }))
                            .find_map(agent_of_email)
                    })
                    .map(str::to_owned)
            });
    cache.lock().ok()?.insert(sha.to_owned(), found.clone());
    found
}

/// Fill the git fields. `item_lines` and `ctx.annotated_lines` are 0-based and inclusive; the
/// comment's own lines do not count as code. Anything unknown stays None.
pub(crate) fn git_evidence(
    ev: &mut Evidence,
    blame: &Blame,
    ctx: &ItemContext,
    item_lines: (usize, usize),
    now: i64,
    repo: &Path,
    cache: &Mutex<HashMap<String, Option<String>>>,
) {
    let newest = |lines: &mut dyn Iterator<Item = usize>| {
        lines
            .filter_map(|i| blame.lines.get(i)?.as_ref())
            .max_by_key(|(_, t)| *t)
    };
    let age = |t: i64| (now - t).max(0) as u64 / 86_400;
    let comment = newest(&mut (item_lines.0..=item_lines.1));
    ev.comment_age_days = comment.map(|(_, t)| age(*t));
    if let Some((a, b)) = ctx.annotated_lines {
        let outside = |i: &usize| *i < item_lines.0 || *i > item_lines.1;
        // A trailing comment shares its only code line; count that line rather than none.
        let code = if (a..=b).any(|i| outside(&i)) {
            newest(&mut (a..=b).filter(outside))
        } else {
            newest(&mut (a..=b))
        };
        ev.code_age_days = code.map(|(_, t)| age(*t));
    }
    ev.code_newer_than_comment = ev
        .code_age_days
        .zip(ev.comment_age_days)
        .map(|(c, m)| c < m);
    // An uncommitted line has no commit to inspect.
    if let Some((sha, _)) = comment.filter(|(s, _)| s.bytes().any(|c| c != b'0')) {
        ev.agent = agent_of(repo, sha, cache);
        ev.authored_by_agent = Some(ev.agent.is_some());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{Placement, Symbol};

    fn ctx(kind: CommentKindV2, comment: &str, code: &str) -> ItemContext {
        ItemContext {
            language: "python",
            comment_kind: kind,
            placement: Placement::BeforeStatement,
            comment: comment.to_owned(),
            annotated_code: code.to_owned(),
            annotated_lines: None,
            scope_lines: None,
            enclosing_scope: String::new(),
            symbol: Symbol {
                name: None,
                public: None,
                is_test: false,
            },
        }
    }

    fn run(kind: CommentKindV2, comment: &str, code: &str, other: &str) -> Evidence {
        let source = format!("{comment}\n{code}\n{other}");
        text_evidence(&ctx(kind, comment, code), &source, (0, comment.len()))
    }

    #[test]
    fn unresolved_mentions() {
        let ev = run(
            CommentKindV2::Inline,
            "# calls `fetch_user` then refresh_cache()",
            "x = fetch_user(1)",
            "",
        );
        assert_eq!(ev.unresolved, ["refresh_cache()"]);
        assert!(ev.mentions[0].resolved);
    }

    #[test]
    fn literal_mismatch() {
        let ev = run(
            CommentKindV2::Inline,
            "# retry 3 times, wait 300ms, log \"giving up\"",
            "retry(3, wait=500)",
            "",
        );
        assert_eq!(ev.literal_mismatch, ["300ms", "giving up"]);
    }

    #[test]
    fn unit_literal_guard() {
        let u = |c, k| unit_literal(c, k);
        assert_eq!(
            u("// 5 seconds", "gcTime: 5000,").as_deref(),
            Some("unit seconds next to literal 5000")
        );
        assert!(u("// 设置1小时过期", "setex(k, 3600)").is_some());
        assert_eq!(
            u("# wait 30s", "sleep(30)").as_deref(),
            Some("unit 30s next to literal 30")
        );
        assert!(u("# size in KB", "n = 1024").is_some());
        assert!(u("# the second arg", "f(1, 2)").is_none(), "ordinal");
        assert!(u("# 2d array", "x[1]").is_none());
        assert!(
            u("# timeout in seconds", "timeout = t").is_none(),
            "no literal"
        );
    }

    #[test]
    fn overlap_restating() {
        let ev = run(
            CommentKindV2::Inline,
            "# increment counter",
            "counter += 1",
            "",
        );
        assert_eq!(ev.overlap, Some(0.5));
    }

    #[test]
    fn signature_echo() {
        let doc = "\"\"\"Get the user name.\"\"\"";
        let ev = run(
            CommentKindV2::ApiDoc,
            doc,
            "def get_user_name(self):\n    return self.name",
            "",
        );
        assert!(ev.signature_echo);
        assert!(
            ev.literal_mismatch.is_empty(),
            "docstring quotes are not a literal"
        );
        let ev = run(CommentKindV2::ApiDoc, doc, "def other(self):", "");
        assert!(!ev.signature_echo);
    }

    #[test]
    fn changed_mentions_gate() {
        let g = |c: &str, b: &str, a: &str| changed_mentions(c, b, a, a);
        // Nearby code changed, nothing the comment names: no candidate.
        let sig = (
            "fn f(ctx: &mut Ctx) {\n    go(ctx);",
            "fn f(ctx: &Ctx) {\n    go(ctx);",
        );
        assert!(g("// Fold the logical expression", sig.0, sig.1).is_empty());
        assert!(g("// Step 3: pick a language", "pick();", "pick();").is_empty());
        assert!(
            g("// uses `ctx`", "  go(ctx);", "go(ctx);").is_empty(),
            "whitespace"
        );
        // A renamed identifier, matched across camel and snake case.
        assert_eq!(
            g(
                "// reads result.pattern",
                "x = result.pattern",
                "x = result.data"
            ),
            ["result.pattern"]
        );
        assert_eq!(
            g(
                "// like tryFoldAdd",
                "fn try_fold_add(a)",
                "fn try_fold_add(a, b)"
            ),
            ["tryFoldAdd"]
        );
        // A changed number or string; one still present after the change does not count.
        assert_eq!(
            g(
                "// wait 300ms, then log \"retry\"",
                "sleep(300); log(\"retry\")",
                "sleep(500); log(\"again\")"
            ),
            ["300", "retry"]
        );
        assert!(g("// wait 300ms", "sleep(300)", "sleep(300); x()").is_empty());
        // A line that only left the window (still in the file) was not removed.
        assert!(
            changed_mentions("// calls fetch_user", "fetch_user(1)", "", "fetch_user(1)")
                .is_empty()
        );
        // An identifier the change did not touch.
        assert!(
            g(
                "// calls fetch_user",
                "fetch_user(1)\nx = 1",
                "fetch_user(1)\nx = 2"
            )
            .is_empty()
        );
    }

    #[test]
    fn todo_tracker() {
        let code = "pass";
        let t = |c| run(CommentKindV2::Todo, c, code, "").todo_tracker;
        assert_eq!(t("# TODO fix, see JIRA-12"), Some(true));
        assert_eq!(t("# TODO fix someday"), Some(false));
        assert_eq!(
            run(CommentKindV2::Inline, "# x", code, "").todo_tracker,
            None
        );
    }

    #[test]
    fn git_blame_and_agent() {
        let dir =
            std::env::temp_dir().join(format!("commentreducr-evidence-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let git = |args: &[&str], date: &str| {
            let out = std::process::Command::new("git")
                .args(["-c", "user.name=T", "-c", "user.email=t@example.invalid"])
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .env("GIT_AUTHOR_DATE", date)
                .env("GIT_COMMITTER_DATE", date)
                .current_dir(&dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q"], "2020-01-01T00:00:00Z");
        let file = dir.join("a.py");
        std::fs::write(&file, "x = 1\n").unwrap();
        git(&["add", "a.py"], "2020-01-01T00:00:00Z");
        git(&["commit", "-qm", "first"], "2020-01-01T00:00:00Z");
        std::fs::write(&file, "x = 1\n# bump\ny = 2\n").unwrap();
        git(&["add", "a.py"], "2020-01-11T00:00:00Z");
        let msg = "second\n\nCo-authored-by: Claude <noreply@anthropic.com>";
        git(&["commit", "-qm", msg], "2020-01-11T00:00:00Z");

        let b = blame(&dir, &file, None).unwrap();
        let mut c = ctx(CommentKindV2::Inline, "# bump", "x = 1");
        c.annotated_lines = Some((0, 0));
        let mut ev = Evidence::default();
        let now = 1_578_700_800 + 5 * 86_400; // 2020-01-11 + 5 days
        git_evidence(
            &mut ev,
            &b,
            &c,
            (1, 1),
            now,
            &dir,
            &Mutex::new(HashMap::new()),
        );
        assert_eq!(ev.agent.as_deref(), Some("claude-code"));
        assert_eq!(ev.authored_by_agent, Some(true));
        assert_eq!((ev.comment_age_days, ev.code_age_days), (Some(5), Some(15)));
        assert_eq!(ev.code_newer_than_comment, Some(false));

        // Trailing comment: code and comment share line 1, so code age comes from that line.
        c.annotated_lines = Some((1, 1));
        let mut ev = Evidence::default();
        git_evidence(
            &mut ev,
            &b,
            &c,
            (1, 1),
            now,
            &dir,
            &Mutex::new(HashMap::new()),
        );
        assert_eq!(ev.code_age_days, Some(5));

        let new = dir.join("new.py");
        std::fs::write(&new, "x = 1\n").unwrap();
        assert!(blame(&dir, &new, None).is_none());
        assert_eq!(agent_of_email("noreply@github.com"), None);
        assert_eq!(
            agent_of_email("1+Copilot@users.noreply.github.com"),
            Some("copilot")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
