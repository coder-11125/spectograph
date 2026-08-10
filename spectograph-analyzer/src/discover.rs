//! File discovery helpers: walk a source tree, respecting `.gitignore`.

use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use spectograph_core::ir::Language;

/// Discover all source files for `language` under `root`, respecting `.gitignore`.
///
/// Hidden files are skipped by default. Returns a sorted, relative-to-root list of paths.
pub fn discover_files(root: &Path, language: Language) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let walker = WalkBuilder::new(root).hidden(true).git_ignore(true).build();

    for entry in walker {
        let entry = entry.map_err(|e| {
            std::io::Error::other(format!("walk error: {e}"))
        })?;
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if language.extensions().contains(&ext) {
                files.push(path.to_path_buf());
            }
        }
    }

    files.sort();
    Ok(files)
}
