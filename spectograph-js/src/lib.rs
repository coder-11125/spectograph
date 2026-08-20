//! Spectograph JavaScript analyzer.
//!
//! Parses JavaScript source with tree-sitter into the shared [`Program`] IR.

pub mod entrypoints;
pub mod parse;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use entrypoints::detect_entrypoint_symbols;
use parse::parse_file;
use serde_json::Value;
use spectograph_analyzer::{discover, AnalyzeError, Analyzer};
use spectograph_core::ir::{
    Environment, Language, Program, RefKind, Reference, SymbolId, SymbolKind,
};

/// Analyzer for the JavaScript language.
pub struct JavaScriptAnalyzer;

/// A curated set of very common npm packages. Used to recognize bare import
/// specifiers as legitimate external dependencies even when `node_modules` isn't
/// present on disk (e.g. analyzing a repo before `npm install`), or when a package
/// simply doesn't declare an entry point we can resolve.
pub const KNOWN_NPM_PACKAGES: &[&str] = &[
    "react",
    "react-dom",
    "react-router",
    "react-router-dom",
    "next",
    "vue",
    "svelte",
    "lodash",
    "underscore",
    "ramda",
    "moment",
    "dayjs",
    "date-fns",
    "express",
    "koa",
    "fastify",
    "hapi",
    "@nestjs/core",
    "axios",
    "node-fetch",
    "got",
    "request",
    "superagent",
    "webpack",
    "vite",
    "rollup",
    "esbuild",
    "parcel",
    "@babel/core",
    "typescript",
    "eslint",
    "prettier",
    "jest",
    "vitest",
    "mocha",
    "chai",
    "sinon",
    "redux",
    "@reduxjs/toolkit",
    "mobx",
    "rxjs",
    "zustand",
    "recoil",
    "chalk",
    "commander",
    "yargs",
    "dotenv",
    "uuid",
    "nanoid",
    "classnames",
    "prop-types",
    "styled-components",
    "tailwindcss",
    "sass",
    "less",
    "socket.io",
    "ws",
    "graphql",
    "apollo-server",
    "@apollo/client",
    "mongoose",
    "sequelize",
    "prisma",
    "knex",
    "pg",
    "mysql",
    "mysql2",
    "sqlite3",
    "jsonwebtoken",
    "bcrypt",
    "bcryptjs",
    "passport",
    "cors",
    "helmet",
    "morgan",
    "body-parser",
    "multer",
    "joi",
    "zod",
    "yup",
    "ajv",
    "immutable",
    "lru-cache",
    "debug",
    "semver",
    "glob",
    "fs-extra",
    "chokidar",
    "jquery",
    "d3",
    "three",
    "chart.js",
    "leaflet",
];

/// The package name a bare (non-relative, non-absolute) import specifier belongs to:
/// `"lodash/debounce"` -> `"lodash"`, `"@scope/pkg/sub"` -> `"@scope/pkg"`,
/// `"react"` -> `"react"`. A scoped package name is two `/`-separated segments;
/// anything else is one.
pub fn npm_package_root(specifier: &str) -> &str {
    let segments_in_name = if specifier.starts_with('@') { 2 } else { 1 };
    match specifier
        .char_indices()
        .filter(|(_, c)| *c == '/')
        .nth(segments_in_name - 1)
    {
        Some((i, _)) => &specifier[..i],
        None => specifier,
    }
}

/// True if `specifier` (a bare, non-relative import specifier) is a recognized
/// third-party package: either a well-known name, or declared in the project's
/// package.json.
pub fn is_recognized_npm_package(specifier: &str, declared_deps: &HashSet<String>) -> bool {
    let root = npm_package_root(specifier);
    KNOWN_NPM_PACKAGES.contains(&root) || declared_deps.contains(root)
}

/// Read all dependency names declared in the root package.json, across
/// dependencies/devDependencies/peerDependencies/optionalDependencies.
pub fn read_declared_dependencies(root: &Path) -> HashSet<String> {
    let Some(json) = std::fs::read_to_string(root.join("package.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
    else {
        return HashSet::new();
    };
    [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ]
    .iter()
    .filter_map(|key| json.get(*key).and_then(|v| v.as_object()))
    .flat_map(|m| m.keys().cloned())
    .collect()
}

/// Read the `browser` field of the project's root `package.json`, if it's an object map
/// (bundlers use this to substitute browser-specific modules for Node ones, e.g.
/// `{"fs": false, "./server.js": "./client.js"}`). A string-form `browser` field is
/// handled separately as an entrypoint (see [`entrypoints::detect_from_package_json`]).
fn read_browser_map(root: &Path) -> HashMap<String, Value> {
    std::fs::read_to_string(root.join("package.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("browser").cloned())
        .and_then(|b| b.as_object().cloned())
        .map(|m| m.into_iter().collect())
        .unwrap_or_default()
}

/// Collapse `.`/`..` components so a joined path's string form matches the plain
/// paths [`spectograph_analyzer::discover::discover_files`] produces. `PathBuf::join`
/// preserves a leading `./` verbatim (e.g. `base/./client.js`), which otherwise never
/// string-equals the canonical `base/client.js` key used for module lookups.
fn normalize_path(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir if result.file_name().is_some() => {
                result.pop();
            }
            other => result.push(other),
        }
    }
    result
}

/// Try `path` as-is, then `path.js`, then `path/index.js` (Node.js's extension and
/// directory-index resolution rules), returning the first that exists as a file.
fn resolve_file_candidate(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    if path.extension().is_none() {
        let with_js = path.with_extension("js");
        if with_js.is_file() {
            return Some(with_js);
        }
    }
    if path.is_dir() {
        let index_path = path.join("index.js");
        if index_path.is_file() {
            return Some(index_path);
        }
    }
    None
}

/// Pick an entry-point file from a package.json `exports` condition object
/// (`{"require": ..., "import": ..., "default": ...}`), preferring `require` since
/// this analyzer treats CommonJS and ESM specifiers uniformly.
fn pick_export_condition(map: &serde_json::Map<String, Value>) -> Option<String> {
    ["require", "node", "import", "default"]
        .iter()
        .find_map(|key| map.get(*key).and_then(|v| v.as_str()).map(String::from))
}

/// Resolve a package.json `exports` field to a single entry-point path, handling the
/// common shapes: a bare string, `{".": "..."}`, `{".": {"require": ..., ...}}`, and a
/// condition object with no `"."` key.
fn exports_entry(exports: &Value) -> Option<String> {
    match exports {
        Value::String(s) => Some(s.clone()),
        Value::Object(map) => match map.get(".") {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Object(cond)) => pick_export_condition(cond),
            _ => pick_export_condition(map),
        },
        _ => None,
    }
}

/// Resolve a package directory (one that has been found under `node_modules`) to its
/// entry-point file, preferring the modern `exports` field over `main`, and falling
/// back to `index.js` if neither resolves to a real file.
fn resolve_package_entry(pkg_dir: &Path) -> Option<PathBuf> {
    if let Ok(content) = std::fs::read_to_string(pkg_dir.join("package.json")) {
        if let Ok(json) = serde_json::from_str::<Value>(&content) {
            let entry = json
                .get("exports")
                .and_then(exports_entry)
                .or_else(|| json.get("main").and_then(|m| m.as_str()).map(String::from));
            if let Some(entry) = entry {
                if let Some(resolved) =
                    resolve_file_candidate(&normalize_path(&pkg_dir.join(&entry)))
                {
                    return Some(resolved);
                }
            }
        }
    }
    resolve_file_candidate(&pkg_dir.join("index.js"))
}

/// Node.js-style module resolution helper, with optional bundler `browser`-field
/// remapping. Returns the resolved file path plus the [`Environment`] the resolution
/// implies (`Browser` only when a `browser` field remap was applied).
fn resolve_module_path(
    base: &Path,
    module_path: &str,
    browser_map: &HashMap<String, Value>,
) -> (PathBuf, Environment) {
    if let Some(mapped) = browser_map.get(module_path).and_then(|v| v.as_str()) {
        let (resolved, _) = resolve_module_path(base, mapped, &HashMap::new());
        return (resolved, Environment::Browser);
    }

    // Handle absolute paths
    if module_path.starts_with('/') || module_path.starts_with("\\") {
        return (PathBuf::from(module_path), Environment::Universal);
    }

    // Handle relative paths (starting with '.' or '..')
    if module_path.starts_with('.') {
        let path = normalize_path(&base.join(module_path));
        if let Some(resolved) = resolve_file_candidate(&path) {
            return (resolved, Environment::Universal);
        }
        return (path, Environment::Universal);
    }

    // Handle node_modules packages (non-relative imports), including package
    // sub-paths (`lodash/debounce`) and scoped packages (`@scope/pkg`).
    // Walk up the directory tree looking for node_modules.
    let mut current = base;
    loop {
        let candidate = current.join("node_modules").join(module_path);

        if candidate.exists() {
            if candidate.is_dir() {
                if let Some(entry) = resolve_package_entry(&candidate) {
                    return (entry, Environment::Universal);
                }
            } else {
                return (candidate, Environment::Universal);
            }
        } else if let Some(resolved) = resolve_file_candidate(&candidate) {
            // A sub-path import without an extension, e.g. `require('pkg/sub')`.
            return (resolved, Environment::Universal);
        }

        // Move up to parent directory
        if let Some(parent) = current.parent() {
            current = parent;
        } else {
            break;
        }
    }

    // Fallback: just return the path as-is (will fail later)
    (base.join(module_path), Environment::Universal)
}

impl Analyzer for JavaScriptAnalyzer {
    fn language(&self) -> Language {
        Language::JavaScript
    }

    fn analyze(&self, root: &Path) -> Result<Program, AnalyzeError> {
        let files = discover::discover_files(root, Language::JavaScript)?;
        if files.is_empty() {
            return Err(AnalyzeError::NoSourceFiles(root.display().to_string()));
        }

        let mut program = Program {
            language: Language::JavaScript,
            root: root.to_path_buf(),
            symbols: Vec::new(),
            references: Vec::new(),
            entrypoints: Vec::new(),
            reachable: Vec::new(),
        };

        let mut next_id: usize = 0;
        // Global name -> symbol id
        let mut global_names: HashMap<String, SymbolId> = HashMap::new();
        // Definition index (Function/Class/Const) by name, for import resolution.
        let mut def_index: HashMap<String, SymbolId> = HashMap::new();
        // Track each file's module id
        let mut file_module: HashMap<String, SymbolId> = HashMap::new();
        // Bundler `browser` field remapping, applied during module resolution below.
        let browser_map = read_browser_map(root);

        // Pass 1: parse each file, collect its symbols and unresolved refs.
        let mut parsed: Vec<(String, parse::FileResult)> = Vec::new();
        for path in &files {
            let src = std::fs::read_to_string(path)?;
            let module_id = next_id;
            next_id += 1;

            let result = parse_file(path, &src, module_id, &mut next_id);

            // Index symbols for resolution
            for sym in &result.symbols {
                if sym.kind != SymbolKind::Module {
                    // First definition wins for vertical slice
                    global_names.entry(sym.name.clone()).or_insert(sym.id);
                    if matches!(
                        sym.kind,
                        SymbolKind::Function | SymbolKind::Class | SymbolKind::Const
                    ) {
                        def_index.entry(sym.name.clone()).or_insert(sym.id);
                    }
                }
            }
            program.symbols.extend(result.symbols.clone());
            parsed.push((path.to_string_lossy().into_owned(), result));
            file_module.insert(path.to_string_lossy().into_owned(), module_id);
        }

        // Build module path index: file path -> module symbol id
        let module_paths: HashMap<String, SymbolId> = program
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Module)
            .map(|s| (s.file.to_string_lossy().into_owned(), s.id))
            .collect();

        // Build name_in_module index: (module_id, name) -> symbol_id
        let mut name_in_module: HashMap<(SymbolId, String), SymbolId> = HashMap::new();
        for (_path, result) in &parsed {
            for sym in &result.symbols {
                if sym.kind != SymbolKind::Module {
                    name_in_module
                        .entry((result.module_id, sym.name.clone()))
                        .or_insert(sym.id);
                }
            }
        }

        // Pass 2: resolve references within each file.
        for (_path, result) in &parsed {
            for r in &result.refs {
                let to = match &r.target {
                    parse::RefTarget::Symbol(id) => Some(*id),
                    parse::RefTarget::Name(name) => global_names.get(name).copied(),
                };
                if let Some(to) = to {
                    program.references.push(Reference {
                        from: r.from,
                        to,
                        kind: r.kind,
                        environment: r.environment,
                    });
                }
            }
        }

        // Pass 3: resolve import references to their definitions.
        // For ES6: import {x} from "y" -> link Import symbol to definition in module y
        // For CommonJS: const x = require("y") -> link Import symbol to definition in module y
        for (_path, result) in &parsed {
            for imp in &result.imports {
                // Resolve the source module with Node.js-style resolution
                let (source_path, resolved_env) = result
                    .symbols
                    .iter()
                    .find(|s| s.kind == SymbolKind::Module)
                    .map(|s| s.file.parent().unwrap_or(&s.file))
                    .map(|p| resolve_module_path(p, &imp.source, &browser_map))
                    .unwrap_or_else(|| (root.join(&imp.source), Environment::Universal));
                // An explicit guard (Tier 1) is a stronger signal than the browser-field
                // remap (which only tells us "this specifier differs in a browser build").
                let ref_env = if imp.environment != Environment::Universal {
                    imp.environment
                } else {
                    resolved_env
                };

                let source_path_str = source_path.to_string_lossy().into_owned();

                // Find the target module in module_paths
                if let Some(&target_module_id) = module_paths.get(&source_path_str) {
                    // Look up the imported name in the target module
                    if let Some(&def_id) =
                        name_in_module.get(&(target_module_id, imp.local_name.clone()))
                    {
                        program.references.push(Reference {
                            from: imp.symbol_id,
                            to: def_id,
                            kind: RefKind::ReExport,
                            environment: ref_env,
                        });
                    } else if imp.is_default {
                        // For CommonJS default imports, fall back to the global def_index
                        // (handles module.exports = function() {} patterns). Never do this
                        // for a target resolved into third-party (node_modules) code: the
                        // specific export name genuinely wasn't found there, and searching
                        // the whole project for a same-named symbol risks wrongly linking
                        // an external package's import to an unrelated local definition.
                        let target_is_external = program
                            .symbols
                            .get(target_module_id)
                            .map(|s| s.is_external)
                            .unwrap_or(false);
                        if !target_is_external {
                            if let Some(&def_id) = def_index.get(&imp.local_name) {
                                program.references.push(Reference {
                                    from: imp.symbol_id,
                                    to: def_id,
                                    kind: RefKind::ReExport,
                                    environment: ref_env,
                                });
                            }
                        }
                    }
                } else {
                    // For CommonJS modules that might not be in module_paths,
                    // try to resolve using node_modules or external packages
                    // For now, just skip external packages
                }
            }
        }

        // Pass 4: detect and mark entrypoints
        program.entrypoints = detect_entrypoint_symbols(&program, &file_module, root)
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        Ok(program)
    }
}
