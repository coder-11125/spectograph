//! `tsconfig.json` / `jsconfig.json` reading, shared by the JS and TS analyzers.
//!
//! Handles what real projects actually ship: JSONC (comments, trailing commas), `extends`
//! chains (relative files and `node_modules` packages like `@vue/tsconfig`), and solution-style
//! root configs that only carry `references` (Vite, Angular, Nx) -- the `paths` live in the
//! referenced `tsconfig.app.json`, not the root file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::jsonc;
use crate::pathutil::normalize_path;

/// The parts of a tsconfig the analyzers care about.
#[derive(Debug, Default, Clone)]
pub struct TsConfig {
    /// `compilerOptions.baseUrl`, absolute and normalized.
    pub base_url: Option<PathBuf>,
    /// `compilerOptions.paths`, with every target already made absolute against the
    /// effective `baseUrl` (or the declaring config's directory) -- `*` wildcards preserved.
    pub paths: HashMap<String, Vec<String>>,
    /// Top-level `include`/`exclude` glob patterns, kept as `None` when the key is absent
    /// (as opposed to present-but-empty) so file filtering can tell "no tsconfig opinion
    /// here" from "matches nothing".
    pub include: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
}

impl TsConfig {
    /// Child options override the parent's, field by field.
    fn overlay(&mut self, child: TsConfig) {
        if child.base_url.is_some() {
            self.base_url = child.base_url;
        }
        if !child.paths.is_empty() {
            self.paths = child.paths;
        }
        if child.include.is_some() {
            self.include = child.include;
        }
        if child.exclude.is_some() {
            self.exclude = child.exclude;
        }
    }
}

/// Read the project's config: `tsconfig.json` if present, else `jsconfig.json`. Missing or
/// unparsable files yield an empty config -- resolution then falls back to relative and
/// `node_modules` lookup only.
pub fn read_tsconfig(root: &Path) -> TsConfig {
    for name in ["tsconfig.json", "jsconfig.json"] {
        let path = root.join(name);
        if !path.is_file() {
            continue;
        }
        let Some((mut cfg, references)) = load_file(&path, 0) else {
            continue;
        };
        // Solution-style configs: fold in what each referenced project declares.
        for reference in references {
            let target = if reference.is_dir() {
                reference.join("tsconfig.json")
            } else {
                reference
            };
            if let Some((referenced, _)) = load_file(&target, 1) {
                if cfg.base_url.is_none() {
                    cfg.base_url = referenced.base_url;
                }
                for (pattern, targets) in referenced.paths {
                    cfg.paths.entry(pattern).or_insert(targets);
                }
            }
        }
        return cfg;
    }
    TsConfig::default()
}

fn string_array(value: Option<&Value>) -> Option<Vec<String>> {
    value.and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect()
    })
}

/// Load one config file (following `extends`), returning it plus its `references` paths.
fn load_file(path: &Path, depth: usize) -> Option<(TsConfig, Vec<PathBuf>)> {
    if depth > 8 {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let json = jsonc::parse(&text)?;
    let dir = path.parent()?;

    let mut cfg = TsConfig::default();

    let extends: Vec<String> = match json.get("extends") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    };
    for spec in extends {
        if let Some(parent_path) = resolve_extends(dir, &spec) {
            if let Some((parent, _)) = load_file(&parent_path, depth + 1) {
                cfg.overlay(parent);
            }
        }
    }

    let options = json.get("compilerOptions");
    let own_base = options
        .and_then(|c| c.get("baseUrl"))
        .and_then(|v| v.as_str())
        .map(|b| normalize_path(&dir.join(b)));
    if own_base.is_some() {
        cfg.base_url = own_base;
    }

    if let Some(paths) = options
        .and_then(|c| c.get("paths"))
        .and_then(|v| v.as_object())
    {
        let base = cfg.base_url.clone().unwrap_or_else(|| normalize_path(dir));
        let mut resolved: HashMap<String, Vec<String>> = HashMap::new();
        for (pattern, targets) in paths {
            let Some(arr) = targets.as_array() else {
                continue;
            };
            let absolute: Vec<String> = arr
                .iter()
                .filter_map(|t| t.as_str())
                .map(|t| normalize_path(&base.join(t)).to_string_lossy().into_owned())
                .collect();
            resolved.insert(pattern.clone(), absolute);
        }
        if !resolved.is_empty() {
            cfg.paths = resolved;
        }
    }

    if let Some(include) = string_array(json.get("include")) {
        cfg.include = Some(include);
    }
    if let Some(exclude) = string_array(json.get("exclude")) {
        cfg.exclude = Some(exclude);
    }

    let references = json
        .get("references")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|r| r.get("path").and_then(|p| p.as_str()))
                .map(|p| normalize_path(&dir.join(p)))
                .collect()
        })
        .unwrap_or_default();

    Some((cfg, references))
}

/// Resolve an `extends` specifier: a relative/absolute file path, or a package in
/// `node_modules` (searched upward from the extending config's directory).
fn resolve_extends(dir: &Path, spec: &str) -> Option<PathBuf> {
    let with_json = |p: PathBuf| -> Option<PathBuf> {
        if p.is_file() {
            return Some(p);
        }
        let appended = PathBuf::from(format!("{}.json", p.to_string_lossy()));
        if appended.is_file() {
            return Some(appended);
        }
        let in_dir = p.join("tsconfig.json");
        in_dir.is_file().then_some(in_dir)
    };

    if spec.starts_with('.') || spec.starts_with('/') {
        return with_json(normalize_path(&dir.join(spec)));
    }
    let mut current = Some(dir);
    while let Some(d) = current {
        if let Some(found) = with_json(d.join("node_modules").join(spec)) {
            return Some(found);
        }
        current = d.parent();
    }
    None
}

/// Match `specifier` against `paths` patterns (each may contain one `*` wildcard), returning
/// the substituted targets. An exact, non-wildcard pattern always wins; among wildcard matches
/// the longest literal prefix wins (TypeScript's own tie-breaking rule).
pub fn match_alias_pattern(
    specifier: &str,
    paths: &HashMap<String, Vec<String>>,
) -> Option<Vec<String>> {
    if let Some(targets) = paths.get(specifier) {
        return Some(targets.clone());
    }
    let mut best: Option<(usize, &str, &Vec<String>)> = None; // (prefix_len, middle, targets)
    for (pattern, targets) in paths {
        let Some(star) = pattern.find('*') else {
            continue;
        };
        let prefix = &pattern[..star];
        let suffix = &pattern[star + 1..];
        if specifier.len() < prefix.len() + suffix.len()
            || !specifier.starts_with(prefix)
            || !specifier.ends_with(suffix)
        {
            continue;
        }
        let middle = &specifier[prefix.len()..specifier.len() - suffix.len()];
        if best.map(|(len, _, _)| prefix.len() > len).unwrap_or(true) {
            best = Some((prefix.len(), middle, targets));
        }
    }
    let (_, middle, targets) = best?;
    Some(targets.iter().map(|t| t.replacen('*', middle, 1)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(dir: &Path, name: &str, content: &str) {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn reads_paths_from_jsonc() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "tsconfig.json",
            r#"{
                // Vite-style config with comments
                "compilerOptions": {
                    "baseUrl": ".",
                    "paths": { "@/*": ["./src/*"], },
                },
            }"#,
        );
        let cfg = read_tsconfig(dir.path());
        let targets = match_alias_pattern("@/lib/util", &cfg.paths).unwrap();
        assert_eq!(
            PathBuf::from(&targets[0]),
            normalize_path(&dir.path().join("src/lib/util"))
        );
    }

    #[test]
    fn follows_relative_extends() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "tsconfig.base.json",
            r#"{"compilerOptions": {"baseUrl": ".", "paths": {"~/*": ["src/*"]}}}"#,
        );
        write(
            dir.path(),
            "tsconfig.json",
            r#"{"extends": "./tsconfig.base", "include": ["src"]}"#,
        );
        let cfg = read_tsconfig(dir.path());
        assert!(cfg.paths.contains_key("~/*"));
        assert_eq!(cfg.include, Some(vec!["src".to_string()]));
    }

    #[test]
    fn follows_node_modules_extends() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "node_modules/@vue/tsconfig/tsconfig.json",
            r#"{"compilerOptions": {"baseUrl": "."}}"#,
        );
        write(
            dir.path(),
            "tsconfig.json",
            r#"{"extends": "@vue/tsconfig/tsconfig.json"}"#,
        );
        let cfg = read_tsconfig(dir.path());
        assert!(cfg.base_url.is_some());
    }

    #[test]
    fn folds_in_solution_style_references() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "tsconfig.json",
            r#"{"files": [], "references": [{"path": "./tsconfig.app.json"}]}"#,
        );
        write(
            dir.path(),
            "tsconfig.app.json",
            r#"{"compilerOptions": {"paths": {"@/*": ["./src/*"]}}}"#,
        );
        let cfg = read_tsconfig(dir.path());
        assert!(cfg.paths.contains_key("@/*"));
    }

    #[test]
    fn falls_back_to_jsconfig() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "jsconfig.json",
            r#"{"compilerOptions": {"baseUrl": "src"}}"#,
        );
        let cfg = read_tsconfig(dir.path());
        assert_eq!(cfg.base_url, Some(normalize_path(&dir.path().join("src"))));
    }

    #[test]
    fn longest_prefix_wildcard_wins_and_exact_beats_wildcard() {
        let mut paths = HashMap::new();
        paths.insert("@/*".to_string(), vec!["/a/*".to_string()]);
        paths.insert("@/ui/*".to_string(), vec!["/b/*".to_string()]);
        paths.insert("@/exact".to_string(), vec!["/c".to_string()]);
        assert_eq!(match_alias_pattern("@/ui/x", &paths).unwrap(), vec!["/b/x"]);
        assert_eq!(
            match_alias_pattern("@/other", &paths).unwrap(),
            vec!["/a/other"]
        );
        assert_eq!(match_alias_pattern("@/exact", &paths).unwrap(), vec!["/c"]);
        assert!(match_alias_pattern("lodash", &paths).is_none());
    }
}
