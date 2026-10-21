//! Inspect Python blocks in the committed trees Git is about to push.
use crate::changeset::{self, SourceBlock, git_text, temp_file};
use crate::{docstring, parse, structural, types::Language, types::Languages};
use anyhow::{Context, Result, anyhow, bail};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct HookBlock {
    pub path: PathBuf,
    /// One-based source lines in the pushed tree.
    pub start_line: usize,
    pub end_line: usize,
    pub kind: String,
    pub text: String,
    pub context: String,
    /// `Added` or `Modified`.
    pub tier: changeset::Tier,
}

fn source_blocks(
    source: &str,
    path: &Path,
    keep_decorators: &[String],
    keep_bases: &[String],
    preserve_structural: bool,
) -> Result<Vec<SourceBlock>> {
    let mut out = Vec::new();
    for block in parse::group_blocks(source, parse::extract_comments(source, Language::Python)?) {
        if preserve_structural && structural::is_structural(&block, Language::Python, source) {
            continue;
        }
        out.push(SourceBlock {
            start: block.start,
            end: block.end,
            start_line: block.start_line + 1,
            end_line: block.end_line + 1,
            kind: "comment".to_owned(),
            text: block
                .comments
                .iter()
                .map(|c| c.text.trim())
                .collect::<Vec<_>>()
                .join("\n"),
        });
    }
    for doc in docstring::extract_docstrings(source, docstring::is_test_file(path), keep_bases)? {
        if preserve_structural && docstring::is_structural(&doc, source, keep_decorators) {
            continue;
        }
        let kind = if doc.is_test {
            "test_docstring"
        } else {
            match doc.kind {
                docstring::DocKind::Module => "module_docstring",
                docstring::DocKind::Class => "class_docstring",
                docstring::DocKind::Function => "function_docstring",
            }
        };
        out.push(SourceBlock {
            start: doc.start,
            end: doc.end,
            start_line: doc.start_line + 1,
            end_line: doc.end_line + 1,
            kind: kind.to_owned(),
            text: doc.text,
        });
    }
    out.sort_by_key(|b| b.start);
    Ok(out)
}

/// The enclosing function/class (including its header), or a nearby module neighborhood. Large
/// scopes retain the header and forty lines on either side; the target itself is removed.
fn context(source: &str, tree: &tree_sitter::Tree, block: &SourceBlock) -> String {
    crate::context::context_for_range(
        source,
        tree,
        Language::Python,
        block.start,
        block.end,
        block.start_line,
        block.end_line,
    )
}

/// Some(input) is Git's pre-push stdin, including an empty list of refs. None checks
/// upstream..HEAD, falling back to known remote history or the full tree on the first push.
/// Returned targets are complete blocks; source bytes always come from committed Git blobs.
/// `range` (`BASE..HEAD`) replaces `input` and `remote`. Also returns the number of changed
/// non-Python files, which this path does not check.
pub fn changed_blocks(
    repo: &Path,
    range: Option<&str>,
    input: Option<&str>,
    remote: Option<&str>,
    ignore: &[String],
    keep_decorators: &[String],
    keep_bases: &[String],
) -> Result<(Vec<HookBlock>, usize)> {
    let set = changeset::collect(repo, range, input, remote, &[Languages::Python], ignore)?;
    let mut output = Vec::new();
    let mut seen = HashSet::new();
    for file in &set.files {
        let (path, source, changes) = (&file.path, &file.head_source, &file.hunks);
        let blocks = source_blocks(source, path, keep_decorators, keep_bases, true)
            .with_context(|| format!("{} in pushed tree", path.display()))?;
        let old_blocks = match (&file.base_source, &file.old_path) {
            // A previously broken file should not stop a push that repairs its syntax.
            (Some(old_source), Some(old)) => {
                source_blocks(old_source, old, keep_decorators, keep_bases, false)
                    .unwrap_or_default()
            }
            _ => Vec::new(),
        };
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&tree_sitter_python::LANGUAGE.into())?;
        let tree = parser
            .parse(source, None)
            .ok_or_else(|| anyhow!("Python parse failed"))?;
        for block in blocks {
            let relevant = changeset::changing_hunks(changes, &block, &old_blocks);
            if relevant.is_empty() {
                continue;
            }
            // shortcut: coarser than changeset::tier (no context); fine for a label on a finding.
            let modified = old_blocks.iter().any(|o| {
                o.kind == block.kind
                    && (o.text == block.text
                        || relevant.iter().any(|h| {
                            changeset::intersects(
                                o.start_line,
                                o.end_line,
                                h.old_start,
                                h.old_count,
                            )
                        }))
            });
            let ctx = context(source, &tree, &block);
            let key = (
                path.clone(),
                block.start_line,
                block.end_line,
                block.kind.clone(),
                block.text.clone(),
                ctx.clone(),
            );
            if seen.insert(key) {
                output.push(HookBlock {
                    path: path.clone(),
                    start_line: block.start_line,
                    end_line: block.end_line,
                    kind: block.kind,
                    text: block.text,
                    context: ctx,
                    tier: if modified {
                        changeset::Tier::Modified
                    } else {
                        changeset::Tier::Added
                    },
                });
            }
        }
    }
    Ok((output, set.other_languages))
}

const HOOK_PREFIX: &str = "#!/bin/sh\n# commentreducr pre-push hook v";
const HOOK_HEADER: &str = "#!/bin/sh\n# commentreducr pre-push hook v2\n";

fn shell_quote(path: &Path) -> Result<String> {
    let path = path
        .to_str()
        .ok_or_else(|| anyhow!("hook path is not UTF-8"))?;
    Ok(format!("'{}'", path.replace('\'', "'\\''")))
}

/// Install a warning-only checker at Git's configured hook path. Preserve an existing hook,
/// its arguments, its stdin and its failure status; reinstalling our wrapper is a no-op.
pub fn install(repo: &Path) -> Result<PathBuf> {
    let path = PathBuf::from(
        git_text(
            repo,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "hooks/pre-push",
            ],
        )?
        .trim_end_matches('\n'),
    );
    let backup = path.with_file_name("pre-push.commentreducr-original");
    let mut managed = false;
    let existing = match std::fs::symlink_metadata(&path) {
        Ok(meta) => {
            if !meta.is_file() || meta.file_type().is_symlink() {
                bail!("refusing to replace non-file hook {}", path.display());
            }
            if std::fs::read_to_string(&path).is_ok_and(|s| s.starts_with(HOOK_HEADER)) {
                return Ok(path);
            }
            managed = std::fs::read_to_string(&path).is_ok_and(|s| s.starts_with(HOOK_PREFIX));
            !managed
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => return Err(err.into()),
    };
    if std::fs::symlink_metadata(&backup).is_ok() && !managed {
        bail!(
            "existing hook backup {}; preserve or move it before installing",
            backup.display()
        );
    }
    let original = if existing || (managed && backup.exists()) {
        let quoted = shell_quote(&backup)?;
        format!("if [ -x {quoted} ]; then\n    {quoted} \"$@\" < \"$input_file\" || exit $?\nfi\n")
    } else {
        String::new()
    };
    let executable = shell_quote(&std::env::current_exe()?)?;
    let script = format!(
        "{HOOK_HEADER}input_file=$(mktemp \"${{TMPDIR:-/tmp}}/commentreducr-pre-push.XXXXXX\") || exit 1\ntrap 'rm -f \"$input_file\"' 0\ntrap 'exit 1' 1 2 3 15\ncat > \"$input_file\" || exit $?\n{original}if command -v commentreducr >/dev/null 2>&1; then\n    commentreducr check --warn \"$@\" < \"$input_file\"\nelse\n    {executable} check --warn \"$@\" < \"$input_file\"\nfi\n"
    );
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("hook has no parent directory"))?;
    std::fs::create_dir_all(dir)?;
    let tmp = temp_file(dir, ".pre-push.commentreducr", script.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp.0, std::fs::Permissions::from_mode(0o755))?;
    }
    if existing {
        std::fs::rename(&path, &backup)?;
    }
    if let Err(err) = std::fs::rename(&tmp.0, &path) {
        if existing {
            let _ = std::fs::rename(&backup, &path);
        }
        return Err(err.into());
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::changeset::tests::Repo;
    use std::io::Write;
    use std::process::{Command, Stdio};

    fn pushed(repo: &Repo, old: &str, new: &str) -> Vec<HookBlock> {
        let refs = format!("refs/heads/main {new} refs/heads/main {old}\n");
        changed_blocks(&repo.0, None, Some(&refs), Some("origin"), &[], &[], &[])
            .unwrap()
            .0
    }

    #[test]
    fn changed_lines_select_complete_committed_blocks_and_ignore_existing_noise() {
        let repo = Repo::new();
        let old = "# existing narration\nx = 1\n\ndef example():\n    \"\"\"Existing summary.\n    Existing continuation.\n    \"\"\"\n    # first comment line\n    # existing middle line\n    # last comment line\n    return 1  # unchanged trailing narration\n";
        repo.write("file with spaces.py", old);
        let base = repo.commit();
        let new = old
            .replace("Existing continuation.", "Changed continuation.")
            .replace("existing middle line", "changed middle line")
            .replace("return 1 ", "return 2 ");
        repo.write("file with spaces.py", &new);
        let tip = repo.commit();
        // The hook must see the committed version even with a broken, unrelated working copy.
        repo.write(
            "file with spaces.py",
            "def broken(:\n# uncommitted comment\n",
        );
        let blocks = pushed(&repo, &base, &tip);
        assert_eq!(blocks.len(), 2, "{blocks:?}");
        assert_eq!(blocks[0].kind, "function_docstring");
        assert_eq!(blocks[0].text, "Existing summary.\nChanged continuation.");
        assert_eq!(
            blocks[1].text,
            "# first comment line\n# changed middle line\n# last comment line"
        );
        assert_eq!((blocks[1].start_line, blocks[1].end_line), (8, 10));
        for block in &blocks {
            assert_eq!(block.path, Path::new("file with spaces.py"));
            assert!(block.context.contains("def example():"));
            assert!(block.context.contains("return 2"));
            assert!(!block.context.contains(&block.text));
            assert!(!block.context.contains("uncommitted"));
        }
    }

    #[test]
    fn pure_deletions_renames_and_code_edits_add_no_blocks() {
        let repo = Repo::new();
        repo.write(
            "original.py",
            "# existing narration\nx = 1\n\n# delete me\n",
        );
        let base = repo.commit();
        repo.write("original.py", "# existing narration\nx = 2\n");
        let deletion = repo.commit();
        assert!(pushed(&repo, &base, &deletion).is_empty());
        repo.git(&["mv", "original.py", "renamed [literal].py"]);
        let rename = repo.commit();
        assert!(pushed(&repo, &deletion, &rename).is_empty());
        repo.write("renamed [literal].py", "# changed short narration\nx = 2\n");
        let modified = repo.commit();
        assert_eq!(
            pushed(&repo, &rename, &modified)[0].text,
            "# changed short narration"
        );
    }

    #[test]
    fn deletion_within_a_surviving_block_checks_the_whole_remainder() {
        let fixtures = [
            (
                "comment",
                "    # first line\n    # interior line\n    # last line\n",
            ),
            (
                "function_docstring",
                "    \"\"\"\n    first line\n    interior line\n    last line\n    \"\"\"\n",
            ),
        ];
        for (kind, target) in fixtures {
            for removed in ["first line", "interior line", "last line"] {
                let repo = Repo::new();
                let source = |block: &str| {
                    format!(
                        "def example():\n{block}    return 1\n\n# unrelated existing narration\n"
                    )
                };
                repo.write("file.py", &source(target));
                let base = repo.commit();
                let deleted = if kind == "comment" {
                    format!("    # {removed}\n")
                } else {
                    format!("    {removed}\n")
                };
                let remainder = target.replace(&deleted, "");
                repo.write("file.py", &source(&remainder));
                let tip = repo.commit();
                let blocks = pushed(&repo, &base, &tip);
                assert_eq!(blocks.len(), 1, "{kind} removing {removed}: {blocks:?}");
                assert_eq!(blocks[0].kind, kind);
                assert!(!blocks[0].text.contains(removed));
                assert_eq!(blocks[0].text.lines().count(), 2);
                // Removing the entire block should not select the unrelated surviving comment.
                repo.write("file.py", &source(""));
                let removed_whole = repo.commit();
                assert!(pushed(&repo, &tip, &removed_whole).is_empty());
            }
        }
    }

    #[test]
    fn multiple_refs_inspect_non_head_trees_and_skip_deleted_and_duplicate_refs() {
        let repo = Repo::new();
        repo.write("file.py", "x = 1\n");
        let base = repo.commit();
        repo.git(&["checkout", "-qb", "topic"]);
        repo.write("file.py", "x = 1\n\n# short new comment\n");
        let topic = repo.commit();
        repo.git(&["checkout", "-q", "main"]);
        repo.git(&["checkout", "-qb", "other"]);
        repo.write(
            "tests/test_other.py",
            "def test_value():\n    \"\"\"Tests the value.\"\"\"\n    assert True\n",
        );
        let other = repo.commit();
        repo.git(&["checkout", "-q", "main"]);
        let zero = "0".repeat(40);
        let input = format!(
            "refs/heads/topic {topic} refs/heads/topic {base}\nrefs/heads/other {other} refs/heads/other {base}\nrefs/heads/topic {topic} refs/heads/duplicate {base}\n(delete) {zero} refs/heads/deleted {base}\n"
        );
        let blocks = changed_blocks(&repo.0, None, Some(&input), Some("origin"), &[], &[], &[])
            .unwrap()
            .0;
        assert_eq!(blocks.len(), 2, "{blocks:?}");
        assert!(blocks.iter().any(|b| b.text == "# short new comment"));
        assert!(blocks.iter().any(|b| b.kind == "test_docstring"));
        assert!(
            changed_blocks(&repo.0, None, Some(""), Some("origin"), &[], &[], &[])
                .unwrap()
                .0
                .is_empty()
        );
    }

    #[test]
    fn new_branch_and_manual_checks_use_known_history_with_first_push_fallback() {
        let repo = Repo::new();
        repo.write("file.py", "# old narration\nx = 1\n");
        let base = repo.commit();
        assert_eq!(
            changed_blocks(&repo.0, None, None, None, &[], &[], &[])
                .unwrap()
                .0
                .len(),
            1
        );
        repo.git(&["remote", "add", "origin", "/unused-hook-test-remote"]);
        repo.git(&["update-ref", "refs/remotes/origin/main", &base]);
        repo.git(&["branch", "--set-upstream-to=origin/main", "main"]);
        repo.write("file.py", "# old narration\nx = 1\n\n# new narration\n");
        let tip = repo.commit();
        let zero = "0".repeat(40);
        let new_branch = format!("refs/heads/new {tip} refs/heads/new {zero}\n");
        let blocks = changed_blocks(
            &repo.0,
            None,
            Some(&new_branch),
            Some("origin"),
            &[],
            &[],
            &[],
        )
        .unwrap()
        .0;
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "# new narration");
        let direct_url = changed_blocks(
            &repo.0,
            None,
            Some(&new_branch),
            Some("/direct-push-url"),
            &[],
            &[],
            &[],
        )
        .unwrap()
        .0;
        assert_eq!(direct_url.len(), 1);
        assert_eq!(direct_url[0].text, "# new narration");
        let manual = changed_blocks(&repo.0, None, None, None, &[], &[], &[])
            .unwrap()
            .0;
        assert_eq!(manual.len(), 1);
        assert_eq!(manual[0].text, "# new narration");
        repo.git(&["checkout", "-qb", "without-upstream"]);
        assert_eq!(
            changed_blocks(&repo.0, None, None, None, &[], &[], &[])
                .unwrap()
                .0
                .len(),
            1
        );
    }

    #[test]
    fn structural_exemptions_and_ignore_patterns_apply_without_a_length_gate() {
        let repo = Repo::new();
        repo.write("file.py", "\"\"\"Help shown by the parser.\"\"\"\nprint(__doc__)\n# SPDX-License-Identifier: MIT\n\n@tool\ndef agent():\n    \"\"\"Instructions for the agent.\"\"\"\n    pass\n\nclass Signature:\n    pass\n\nclass Input(Signature):\n    \"\"\"Instructions for the schema.\"\"\"\n    pass\n\ndef documented():\n    \"\"\">>> documented()\n    1\n    \"\"\"\n    return 1\n\n# noqa: E501\n\n# short narration\n");
        repo.write("vendor/skip.py", "# ignored narration\n");
        repo.write(
            "stub.pyi",
            "\"\"\"Ordinary module description.\"\"\"\nx: int\n",
        );
        repo.commit();
        let blocks = changed_blocks(
            &repo.0,
            None,
            None,
            None,
            &["vendor/".to_owned()],
            &["tool".to_owned()],
            &["Signature".to_owned()],
        )
        .unwrap()
        .0;
        assert_eq!(blocks.len(), 2, "{blocks:?}");
        assert!(blocks.iter().any(|b| b.text == "# short narration"));
        assert!(
            blocks
                .iter()
                .any(|b| b.kind == "module_docstring" && b.path == Path::new("stub.pyi"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn installer_preserves_existing_hook_stdin_arguments_status_and_configured_path() {
        use std::os::unix::fs::PermissionsExt;
        let repo = Repo::new();
        repo.git(&["config", "core.hooksPath", "hooks with 'quote"]);
        let hooks = repo.0.join("hooks with 'quote");
        std::fs::create_dir_all(&hooks).unwrap();
        let original = "#!/bin/sh\ncat > original.stdin\nprintf '%s\\n' \"$@\" > original.args\nexit \"${ORIGINAL_STATUS:-0}\"\n";
        std::fs::write(hooks.join("pre-push"), original).unwrap();
        std::fs::set_permissions(
            hooks.join("pre-push"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let installed = install(&repo.0).unwrap();
        assert_eq!(
            installed.canonicalize().unwrap(),
            hooks.join("pre-push").canonicalize().unwrap()
        );
        assert_eq!(install(&repo.0).unwrap(), installed);
        assert_eq!(
            std::fs::read_to_string(hooks.join("pre-push.commentreducr-original")).unwrap(),
            original
        );
        repo.write(
            "bin/commentreducr",
            "#!/bin/sh\ncat > checker.stdin\nprintf '%s\\n' \"$@\" > checker.args\n",
        );
        std::fs::set_permissions(
            repo.0.join("bin/commentreducr"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let path = format!(
            "{}:{}",
            repo.0.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let stdin = "refs/heads/main abc refs/heads/main def\n";
        let run = |status: &str| {
            // Parallel test subprocesses can briefly inherit a write descriptor on Linux.
            let mut child = Command::new("sh")
                .arg(&installed)
                .args(["origin", "remote path"])
                .env("PATH", &path)
                .env("ORIGINAL_STATUS", status)
                .current_dir(&repo.0)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(stdin.as_bytes())
                .unwrap();
            child.wait_with_output().unwrap()
        };
        let out = run("0");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(repo.0.join("original.stdin")).unwrap(),
            stdin
        );
        assert_eq!(
            std::fs::read_to_string(repo.0.join("checker.stdin")).unwrap(),
            stdin
        );
        assert_eq!(
            std::fs::read_to_string(repo.0.join("original.args")).unwrap(),
            "origin\nremote path\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo.0.join("checker.args")).unwrap(),
            "check\n--warn\norigin\nremote path\n"
        );
        std::fs::remove_file(repo.0.join("checker.stdin")).unwrap();
        assert_eq!(run("7").status.code(), Some(7));
        assert!(!repo.0.join("checker.stdin").exists());
        let legacy = std::fs::read_to_string(&installed)
            .unwrap()
            .replace("pre-push hook v2", "pre-push hook v1")
            .replace("check --warn", "prepush-check --warn");
        std::fs::write(&installed, legacy).unwrap();
        assert_eq!(install(&repo.0).unwrap(), installed);
        let updated = std::fs::read_to_string(&installed).unwrap();
        assert!(updated.starts_with(HOOK_HEADER));
        assert!(!updated.contains("prepush-check"));
        assert!(run("0").status.success());
        assert_eq!(
            std::fs::read_to_string(hooks.join("pre-push.commentreducr-original")).unwrap(),
            original
        );
    }

    #[test]
    fn install_recognises_any_managed_version() {
        let repo = Repo::new();
        let installed = install(&repo.0).unwrap();
        let old = std::fs::read_to_string(&installed)
            .unwrap()
            .replace("pre-push hook v2", "pre-push hook v1");
        std::fs::write(&installed, old).unwrap();
        install(&repo.0).unwrap();
        assert!(
            !installed
                .with_file_name("pre-push.commentreducr-original")
                .exists()
        );
        assert!(
            std::fs::read_to_string(&installed)
                .unwrap()
                .starts_with(HOOK_HEADER)
        );
    }
}
