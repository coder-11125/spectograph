//! Small path helpers shared by the JS and TS analyzers.

use std::path::{Component, Path, PathBuf};

/// Collapse `.`/`..` components so a joined path's string form matches the plain paths
/// [`spectograph_analyzer::discover::discover_files`] produces. `PathBuf::join` preserves a
/// leading `./` verbatim (e.g. `base/./client.js`), which otherwise never string-equals the
/// canonical `base/client.js` key used for module lookups.
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir if result.file_name().is_some() => {
                result.pop();
            }
            other => result.push(other),
        }
    }
    result
}

/// `path` relative to an already-normalized `root_norm`, `/`-separated; `None` when `path`
/// is not under it.
pub fn rel_slash(root_norm: &Path, path: &Path) -> Option<String> {
    normalize_path(path)
        .strip_prefix(root_norm)
        .ok()
        .map(|r| r.to_string_lossy().replace('\\', "/"))
}

/// Join two `/`-separated relative paths, collapsing `.`, empty, and `..` segments. A `..`
/// that would climb above the start is dropped (the result stays relative to the root).
pub fn join_norm(base: &str, rel: &str) -> String {
    let mut parts: Vec<&str> = base
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// The directory part of a `/`-separated relative path (`""` for a top-level file).
pub fn parent_dir(rel: &str) -> &str {
    rel.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_collapses_dots() {
        assert_eq!(
            normalize_path(Path::new("a/./b/../c.js")),
            PathBuf::from("a/c.js")
        );
        assert_eq!(normalize_path(Path::new("./a.js")), PathBuf::from("a.js"));
    }

    #[test]
    fn join_norm_handles_dots_and_parents() {
        assert_eq!(join_norm("packages/a", "./src/x.ts"), "packages/a/src/x.ts");
        assert_eq!(join_norm("packages/a", "../b/x.ts"), "packages/b/x.ts");
        assert_eq!(join_norm("", "./x.ts"), "x.ts");
        assert_eq!(join_norm("a", "../../x.ts"), "x.ts");
    }

    #[test]
    fn parent_dir_of_top_level_is_empty() {
        assert_eq!(parent_dir("a.js"), "");
        assert_eq!(parent_dir("src/a.js"), "src");
        assert_eq!(parent_dir("a/b/c.js"), "a/b");
    }
}
