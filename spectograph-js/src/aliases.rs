//! Project-level import resolution beyond relative paths and `node_modules`.
//!
//! A bare specifier like `@/components/Button` or `@acme/ui` is not a third-party package:
//! it is a `tsconfig`/`jsconfig` `paths` alias, a `package.json` `imports` entry (`#utils`),
//! a bundler convention (`@/` and `~/` mean `src/` in Vite, Next, and Nuxt projects), or a
//! sibling package in the same monorepo. Left unresolved, the file behind each such import
//! looks unreferenced and is reported as guaranteed bloat -- the largest single source of
//! false positives on real applications.
//!
//! [`ProjectResolver::candidates`] returns candidate *base paths* (no extension guessing);
//! each analyzer feeds them through its own `resolve_file_candidate`, which knows its
//! language's extensions.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::entrypoints::source_candidates;
use crate::pathutil::normalize_path;
use crate::tsconfig::{match_alias_pattern, read_tsconfig, TsConfig};

/// Every `package.json` under `root`, skipping `node_modules` and gitignored paths.
pub fn find_package_jsons(root: &Path) -> Vec<PathBuf> {
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .filter_entry(|e| e.file_name() != "node_modules")
        .build();
    let mut found: Vec<PathBuf> = walker
        .filter_map(|entry| entry.ok())
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()) && e.file_name() == "package.json")
        .map(|e| e.into_path())
        .collect();
    found.sort();
    found
}

/// A package living in the analyzed tree (a monorepo workspace member), by its
/// `package.json` `name`.
#[derive(Debug, Clone)]
struct WorkspacePackage {
    name: String,
    dir: PathBuf,
    /// Entry-point path hints from `package.json` (`source`, `module`, `main`, `exports`...).
    hints: Vec<String>,
}

/// Resolves project-specific import specifiers. Built once per analysis.
#[derive(Debug, Clone)]
pub struct ProjectResolver {
    root: PathBuf,
    tsconfig: TsConfig,
    /// `package.json` `imports` (`#alias` -> path patterns), targets absolute.
    imports: HashMap<String, Vec<String>>,
    workspace: Vec<WorkspacePackage>,
}

fn collect_leaf_strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Array(items) => items.iter().for_each(|v| collect_leaf_strings(v, out)),
        Value::Object(map) => map.values().for_each(|v| collect_leaf_strings(v, out)),
        _ => {}
    }
}

/// Entry-point path hints for a package: the fields a bundler or Node would consult to find
/// "the" file behind `import 'name'`.
fn entry_hints(json: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for key in ["source", "module", "main", "types", "typings"] {
        if let Some(s) = json.get(key).and_then(|v| v.as_str()) {
            out.push(s.to_string());
        }
    }
    if let Some(s) = json.get("browser").and_then(|v| v.as_str()) {
        out.push(s.to_string());
    }
    if let Some(exports) = json.get("exports") {
        match exports {
            Value::Object(map) if map.keys().any(|k| k.starts_with('.')) => {
                if let Some(dot) = map.get(".") {
                    collect_leaf_strings(dot, &mut out);
                }
            }
            other => collect_leaf_strings(other, &mut out),
        }
    }
    out
}

impl ProjectResolver {
    pub fn load(root: &Path) -> Self {
        let tsconfig = read_tsconfig(root);

        let mut imports: HashMap<String, Vec<String>> = HashMap::new();
        if let Some(map) = std::fs::read_to_string(root.join("package.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v.get("imports").and_then(|i| i.as_object()).cloned())
        {
            for (pattern, target) in map {
                let mut leaves = Vec::new();
                collect_leaf_strings(&target, &mut leaves);
                let absolute: Vec<String> = leaves
                    .into_iter()
                    .filter(|t| t.starts_with('.'))
                    .map(|t| normalize_path(&root.join(t)).to_string_lossy().into_owned())
                    .collect();
                if !absolute.is_empty() {
                    imports.insert(pattern, absolute);
                }
            }
        }

        let mut workspace = Vec::new();
        for pj in find_package_jsons(root) {
            let Some(json) = std::fs::read_to_string(&pj)
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            else {
                continue;
            };
            let (Some(name), Some(dir)) = (json.get("name").and_then(|n| n.as_str()), pj.parent())
            else {
                continue;
            };
            workspace.push(WorkspacePackage {
                name: name.to_string(),
                dir: normalize_path(dir),
                hints: entry_hints(&json),
            });
        }

        Self {
            root: normalize_path(root),
            tsconfig,
            imports,
            workspace,
        }
    }

    /// The parsed `tsconfig.json`/`jsconfig.json` (for `include`/`exclude` filtering).
    pub fn tsconfig(&self) -> &TsConfig {
        &self.tsconfig
    }

    /// Candidate base paths for a bare specifier, highest priority first. May be empty.
    pub fn candidates(&self, specifier: &str) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();

        if let Some(targets) = match_alias_pattern(specifier, &self.tsconfig.paths) {
            out.extend(targets.iter().map(|t| normalize_path(Path::new(t))));
        }

        if specifier.starts_with('#') {
            if let Some(targets) = match_alias_pattern(specifier, &self.imports) {
                out.extend(targets.iter().map(|t| normalize_path(Path::new(t))));
            }
        }

        for pkg in &self.workspace {
            if specifier == pkg.name {
                for hint in &pkg.hints {
                    out.push(normalize_path(&pkg.dir.join(hint)));
                    for source in source_candidates(hint) {
                        out.push(normalize_path(&pkg.dir.join(source)));
                    }
                }
                for index in ["src/index", "index", "lib/index"] {
                    out.push(pkg.dir.join(index));
                }
            } else if let Some(rest) = specifier
                .strip_prefix(pkg.name.as_str())
                .and_then(|r| r.strip_prefix('/'))
            {
                for dir in ["", "src", "lib"] {
                    out.push(normalize_path(&pkg.dir.join(dir).join(rest)));
                }
            }
        }

        if let Some(base) = &self.tsconfig.base_url {
            out.push(normalize_path(&base.join(specifier)));
        }

        out.extend(self.conventional(specifier));
        out
    }

    /// Bundler-convention aliases that need no config file: `@/` and `~/` mean the source
    /// root in Vite, Next, Nuxt, and Vue CLI projects; `$lib/` is SvelteKit's.
    fn conventional(&self, specifier: &str) -> Vec<PathBuf> {
        let root = &self.root;
        let under = |dirs: &[&str], rest: &str| -> Vec<PathBuf> {
            dirs.iter()
                .map(|d| normalize_path(&root.join(d).join(rest)))
                .collect()
        };
        if let Some(rest) = specifier.strip_prefix("@/") {
            return under(&["src", "", "app"], rest);
        }
        if let Some(rest) = specifier.strip_prefix("~/") {
            return under(&["src", "", "app"], rest);
        }
        if let Some(rest) = specifier
            .strip_prefix("~~/")
            .or_else(|| specifier.strip_prefix("@@/"))
        {
            return under(&[""], rest);
        }
        if let Some(rest) = specifier.strip_prefix("$lib/") {
            return under(&["src/lib"], rest);
        }
        if specifier.starts_with("src/") {
            return under(&[""], specifier);
        }
        Vec::new()
    }
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

    fn has(candidates: &[PathBuf], root: &Path, rel: &str) -> bool {
        candidates.contains(&normalize_path(&root.join(rel)))
    }

    #[test]
    fn at_slash_maps_to_src_without_any_config() {
        let dir = tempdir().unwrap();
        let resolver = ProjectResolver::load(dir.path());
        let c = resolver.candidates("@/components/Button");
        assert!(has(&c, dir.path(), "src/components/Button"));
    }

    #[test]
    fn package_json_imports_are_resolved() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r##"{"name": "app", "imports": {"#utils/*": "./src/utils/*.js"}}"##,
        );
        let resolver = ProjectResolver::load(dir.path());
        let c = resolver.candidates("#utils/date");
        assert!(has(&c, dir.path(), "src/utils/date.js"));
    }

    #[test]
    fn workspace_packages_resolve_to_source_not_dist() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"name": "root", "private": true}"#,
        );
        write(
            dir.path(),
            "packages/ui/package.json",
            r#"{"name": "@acme/ui", "main": "dist/index.js"}"#,
        );
        let resolver = ProjectResolver::load(dir.path());
        let c = resolver.candidates("@acme/ui");
        assert!(has(&c, dir.path(), "packages/ui/src/index.ts"));
        let sub = resolver.candidates("@acme/ui/button");
        assert!(has(&sub, dir.path(), "packages/ui/src/button"));
    }

    #[test]
    fn tsconfig_paths_come_first() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "tsconfig.json",
            r#"{"compilerOptions": {"baseUrl": ".", "paths": {"@lib/*": ["lib/*"]}}}"#,
        );
        let resolver = ProjectResolver::load(dir.path());
        let c = resolver.candidates("@lib/x");
        assert_eq!(c.first(), Some(&normalize_path(&dir.path().join("lib/x"))));
    }

    #[test]
    fn unrelated_bare_specifiers_yield_only_base_url_style_candidates() {
        let dir = tempdir().unwrap();
        let resolver = ProjectResolver::load(dir.path());
        assert!(resolver.candidates("react").is_empty());
    }
}
