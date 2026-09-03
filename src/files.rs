//! Enumerate git-tracked source files under a directory.
use crate::types::Language;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Runs `git ls-files -z` in `root` and returns tracked files with a supported extension,
/// as absolute paths paired with their language. `root` may be a subdirectory of the repo;
/// only files under it are returned.
pub fn tracked_source_files(root: &Path) -> Result<Vec<(PathBuf, Language)>> {
    let output = Command::new("git")
        .arg("ls-files")
        .arg("-z")
        .current_dir(root)
        .output()
        .with_context(|| format!("failed to run `git ls-files` in {}", root.display()))?;

    if !output.status.success() {
        anyhow::bail!(
            "`git ls-files` failed in {}: {}",
            root.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let root = root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", root.display()))?;

    let stdout =
        String::from_utf8(output.stdout).context("`git ls-files` output was not valid UTF-8")?;

    let mut files = Vec::new();
    for rel in stdout.split('\0') {
        if rel.is_empty() {
            continue;
        }
        let path = Path::new(rel);
        if let Some(lang) = Language::from_path(path) {
            let full = root.join(path);
            let full = full
                .canonicalize()
                .with_context(|| format!("failed to canonicalize {}", full.display()))?;
            files.push((full, lang));
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_on_repo_and_filters_by_extension() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let files = tracked_source_files(root).unwrap();
        // This is a Rust project; .rs files are not a supported language, so none
        // of the returned paths should end in .rs, but every path should be absolute.
        for (p, _) in &files {
            assert!(p.is_absolute());
            assert_ne!(p.extension().unwrap(), "rs");
        }
    }
}
