//! Enumerate git-tracked source files under a directory.
use crate::types::Language;
use anyhow::Result;
use std::path::{Path, PathBuf};

/// Runs `git ls-files -z` in `root` and returns tracked files with a supported extension,
/// as absolute paths paired with their language. `root` may be a subdirectory of the repo;
/// only files under it are returned.
pub fn tracked_source_files(root: &Path) -> Result<Vec<(PathBuf, Language)>> {
    let _ = root;
    todo!("files::tracked_source_files")
}
