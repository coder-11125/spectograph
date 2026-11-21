//! Bundler glob imports: Vite's `import.meta.glob('./views/*.vue')` and webpack's
//! `require.context('./components', true, /\.vue$/)`.
//!
//! Both load every module matching a pattern, with no `import` statement naming any of them --
//! the standard way route views and auto-registered components are wired in Vue (and plenty of
//! other) projects. Without modelling them, every matched module looks unreferenced. A matched
//! module is treated as used as a whole (its exports are consumed through the returned
//! module map), exactly like `import('./x')`.
//!
//! Detection is by call shape and works on both grammars (these node kinds are identical in
//! tree-sitter-javascript and tree-sitter-typescript); patterns that aren't string literals
//! can't be enumerated and are ignored (precision first: an unmatched glob only means a module
//! is judged on its own references).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use globset::GlobBuilder;
use regex::Regex;
use spectograph_core::ir::SymbolId;
use tree_sitter::Node;

use crate::pathutil::normalize_path;

/// One glob-style bulk import found in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleGlob {
    /// Vite `import.meta.glob(patterns)` / `globEager`: include patterns, plus `!`-prefixed
    /// excludes kept in the same list.
    Vite { patterns: Vec<String> },
    /// webpack `require.context(dir, recursive, regex)`.
    WebpackContext {
        dir: String,
        recursive: bool,
        regex: Option<String>,
    },
}

fn text<'a>(node: Node, src: &'a str) -> &'a str {
    node.utf8_text(src.as_bytes()).unwrap_or("").trim()
}

/// The contents of a plain string literal node (`'./a/*.vue'`), without quotes. Template
/// strings with substitutions are not literals.
fn string_value(node: Node, src: &str) -> Option<String> {
    match node.kind() {
        "string" => Some(text(node, src).trim_matches(['\'', '"']).to_string()),
        "template_string" if !text(node, src).contains("${") => {
            Some(text(node, src).trim_matches('`').to_string())
        }
        _ => None,
    }
}

fn is_import_meta(node: Node, src: &str) -> bool {
    node.kind() == "meta_property" && text(node, src) == "import.meta"
}

/// Walk the whole tree for `import.meta.glob(...)` and `require.context(...)` calls. They
/// can appear anywhere (a route table, a function body), so this isn't limited to top level.
pub fn collect_module_globs(root: Node, src: &str) -> Vec<ModuleGlob> {
    let mut out = Vec::new();
    walk(root, src, &mut out);
    out
}

fn walk(node: Node, src: &str, out: &mut Vec<ModuleGlob>) {
    if node.kind() == "call_expression" {
        if let Some(glob) = glob_call(node, src) {
            out.push(glob);
        }
    }
    let mut c = node.walk();
    for child in node.named_children(&mut c) {
        walk(child, src, out);
    }
}

fn glob_call(call: Node, src: &str) -> Option<ModuleGlob> {
    let func = call.child_by_field_name("function")?;
    if func.kind() != "member_expression" {
        return None;
    }
    let object = func.child_by_field_name("object")?;
    let property = text(func.child_by_field_name("property")?, src);
    let args = call.child_by_field_name("arguments")?;
    let mut ac = args.walk();
    let args: Vec<Node> = args.named_children(&mut ac).collect();

    if is_import_meta(object, src) && matches!(property, "glob" | "globEager") {
        let first = args.first()?;
        let patterns = match first.kind() {
            "array" => {
                let mut c = first.walk();
                let all: Vec<Option<String>> = first
                    .named_children(&mut c)
                    .map(|e| string_value(e, src))
                    .collect();
                // One non-literal element makes the pattern list unknowable.
                if all.iter().any(Option::is_none) {
                    return None;
                }
                all.into_iter().flatten().collect()
            }
            _ => vec![string_value(*first, src)?],
        };
        return Some(ModuleGlob::Vite { patterns });
    }

    if object.kind() == "identifier" && text(object, src) == "require" && property == "context" {
        let dir = string_value(*args.first()?, src)?;
        let recursive = !matches!(args.get(1).map(|n| text(*n, src)), Some("false"));
        let regex = args.get(2).and_then(|n| {
            if n.kind() != "regex" {
                return None;
            }
            // `/pattern/flags` -> `pattern`
            let raw = text(*n, src);
            let body = raw.strip_prefix('/')?;
            let end = body.rfind('/')?;
            Some(body[..end].to_string())
        });
        return Some(ModuleGlob::WebpackContext {
            dir,
            recursive,
            regex,
        });
    }
    None
}

/// Split a Vite pattern into the directory it is anchored at and the glob relative to it:
/// `./views/*.vue` from `/p/src` -> (`/p/src`, `views/*.vue`); `../x/*.vue` climbs;
/// `/src/pages/**` is relative to the project root. Other forms (aliases such as `@/`,
/// bare specifiers) can't be anchored here and return `None`.
fn anchor_pattern(
    pattern: &str,
    file_dir: &Path,
    project_root: &Path,
) -> Option<(PathBuf, String)> {
    if let Some(rest) = pattern.strip_prefix('/') {
        return Some((normalize_path(project_root), rest.to_string()));
    }
    if !(pattern.starts_with("./") || pattern.starts_with("../")) {
        return None;
    }
    let mut anchor = file_dir.to_path_buf();
    let mut rest = pattern;
    loop {
        if let Some(r) = rest.strip_prefix("./") {
            rest = r;
        } else if let Some(r) = rest.strip_prefix("../") {
            anchor = anchor.parent().map(Path::to_path_buf).unwrap_or_default();
            rest = r;
        } else {
            break;
        }
    }
    Some((normalize_path(&anchor), rest.to_string()))
}

/// Modules (by id) matched by `globs`, written in a file located in `file_dir`. `modules`
/// maps normalized module file paths to their module symbol ids.
pub fn resolve_module_globs(
    globs: &[ModuleGlob],
    file_dir: &Path,
    project_root: &Path,
    modules: &HashMap<String, SymbolId>,
) -> Vec<SymbolId> {
    let mut hits: Vec<SymbolId> = Vec::new();
    for glob in globs {
        match glob {
            ModuleGlob::Vite { patterns } => {
                let compile = |p: &str| -> Option<(PathBuf, globset::GlobMatcher)> {
                    let (anchor, rel) = anchor_pattern(p, file_dir, project_root)?;
                    let matcher = GlobBuilder::new(&rel)
                        .literal_separator(true)
                        .build()
                        .ok()?
                        .compile_matcher();
                    Some((anchor, matcher))
                };
                let includes: Vec<_> = patterns
                    .iter()
                    .filter(|p| !p.starts_with('!'))
                    .filter_map(|p| compile(p))
                    .collect();
                let excludes: Vec<_> = patterns
                    .iter()
                    .filter_map(|p| p.strip_prefix('!'))
                    .filter_map(compile)
                    .collect();
                let matches = |set: &[(PathBuf, globset::GlobMatcher)], file: &Path| {
                    set.iter().any(|(anchor, m)| {
                        file.strip_prefix(anchor)
                            .ok()
                            .is_some_and(|rel| m.is_match(rel))
                    })
                };
                for (key, &id) in modules {
                    let file = Path::new(key);
                    if matches(&includes, file) && !matches(&excludes, file) {
                        hits.push(id);
                    }
                }
            }
            ModuleGlob::WebpackContext {
                dir,
                recursive,
                regex,
            } => {
                let dir_path = normalize_path(&file_dir.join(dir));
                // No regex argument means "everything" in webpack.
                let re = match regex {
                    Some(r) => match Regex::new(r) {
                        Ok(re) => Some(re),
                        Err(_) => continue,
                    },
                    None => None,
                };
                for (key, &id) in modules {
                    let Ok(rel) = Path::new(key).strip_prefix(&dir_path) else {
                        continue;
                    };
                    if !*recursive && rel.components().count() > 1 {
                        continue;
                    }
                    // webpack tests keys of the form `./sub/File.vue`.
                    let rel_key = format!("./{}", rel.to_string_lossy().replace('\\', "/"));
                    if re.as_ref().is_none_or(|re| re.is_match(&rel_key)) {
                        hits.push(id);
                    }
                }
            }
        }
    }
    hits.sort_unstable();
    hits.dedup();
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn globs(src: &str) -> Vec<ModuleGlob> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_javascript::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(src, None).unwrap();
        collect_module_globs(tree.root_node(), src)
    }

    fn modules(paths: &[&str]) -> HashMap<String, SymbolId> {
        paths
            .iter()
            .enumerate()
            .map(|(i, p)| (p.to_string(), i))
            .collect()
    }

    #[test]
    fn detects_vite_glob_forms() {
        assert_eq!(
            globs("const m = import.meta.glob('./views/*.vue')"),
            vec![ModuleGlob::Vite {
                patterns: vec!["./views/*.vue".into()]
            }]
        );
        assert_eq!(
            globs("import.meta.glob(['./a/*.js', '!./a/skip.js'], { eager: true })"),
            vec![ModuleGlob::Vite {
                patterns: vec!["./a/*.js".into(), "!./a/skip.js".into()]
            }]
        );
        // A non-literal pattern is unknowable.
        assert!(globs("import.meta.glob(`./${dir}/*.js`)").is_empty());
        assert!(globs("import.meta.glob(['./a/*.js', pattern])").is_empty());
        assert!(globs("other.glob('./a/*.js')").is_empty());
    }

    #[test]
    fn detects_webpack_context() {
        assert_eq!(
            globs("require.context('./components', false, /\\.vue$/i)"),
            vec![ModuleGlob::WebpackContext {
                dir: "./components".into(),
                recursive: false,
                regex: Some("\\.vue$".into())
            }]
        );
        assert_eq!(
            globs("require.context('./c')"),
            vec![ModuleGlob::WebpackContext {
                dir: "./c".into(),
                recursive: true,
                regex: None
            }]
        );
    }

    #[test]
    fn vite_patterns_match_relative_root_and_negated() {
        let mods = modules(&[
            "src/views/Home.vue",
            "src/views/About.vue",
            "src/views/sub/Deep.vue",
            "src/other/X.vue",
        ]);
        let one = |pats: &[&str]| {
            let g = ModuleGlob::Vite {
                patterns: pats.iter().map(|s| s.to_string()).collect(),
            };
            let mut ids = resolve_module_globs(&[g], Path::new("src"), Path::new(""), &mods);
            ids.sort_unstable();
            ids.iter()
                .map(|i| {
                    mods.iter()
                        .find(|(_, v)| *v == i)
                        .map(|(k, _)| k.clone())
                        .unwrap()
                })
                .collect::<Vec<_>>()
        };
        let mut shallow = one(&["./views/*.vue"]);
        shallow.sort();
        assert_eq!(shallow, vec!["src/views/About.vue", "src/views/Home.vue"]);
        assert_eq!(one(&["./views/**/*.vue"]).len(), 3);
        assert_eq!(one(&["/src/other/*.vue"]), vec!["src/other/X.vue"]);
        assert_eq!(
            one(&["./views/*.vue", "!./views/Home.vue"]),
            vec!["src/views/About.vue"]
        );
        // Aliased patterns can't be anchored.
        assert!(one(&["@/views/*.vue"]).is_empty());
    }

    #[test]
    fn webpack_context_respects_recursion_and_regex() {
        let mods = modules(&["c/A.vue", "c/b.js", "c/sub/D.vue"]);
        let run = |recursive: bool, regex: Option<&str>| {
            resolve_module_globs(
                &[ModuleGlob::WebpackContext {
                    dir: "./c".into(),
                    recursive,
                    regex: regex.map(String::from),
                }],
                Path::new(""),
                Path::new(""),
                &mods,
            )
            .len()
        };
        assert_eq!(run(true, Some("\\.vue$")), 2);
        assert_eq!(run(false, Some("\\.vue$")), 1);
        assert_eq!(run(true, None), 3);
    }
}
