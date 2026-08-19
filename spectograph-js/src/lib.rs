//! Spectograph JavaScript analyzer.
//!
//! Parses JavaScript source with tree-sitter into the shared [`Program`] IR.

pub mod entrypoints;
pub mod parse;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use entrypoints::detect_entrypoint_symbols;
use parse::parse_file;
use serde_json::Value;
use spectograph_analyzer::{discover, AnalyzeError, Analyzer};
use spectograph_core::ir::{Language, Program, RefKind, Reference, SymbolId, SymbolKind};

/// Analyzer for the JavaScript language.
pub struct JavaScriptAnalyzer;

/// Node.js-style module resolution helper
fn resolve_module_path(base: &Path, module_path: &str, _root: &Path) -> PathBuf {
    // Handle absolute paths
    if module_path.starts_with('/') || module_path.starts_with("\\") {
        return PathBuf::from(module_path);
    }

    // Handle relative paths (starting with '.' or '..')
    if module_path.starts_with('.') {
        let path = base.join(module_path);

        // If the path doesn't have an extension, try .js
        if path.extension().is_none() {
            let with_js = path.with_extension("js");
            if with_js.exists() {
                return with_js;
            }
        }

        // If path is a directory, try index.js
        if path.is_dir() {
            let index_path = path.join("index.js");
            if index_path.exists() {
                return index_path;
            }
        }

        return path;
    }

    // Handle node_modules packages (non-relative imports)
    // Walk up the directory tree looking for node_modules
    let mut current = base;
    loop {
        let node_modules = current.join("node_modules").join(module_path);

        // Try the package path
        if node_modules.exists() {
            if node_modules.is_dir() {
                // Check for package.json main field
                let package_json = node_modules.join("package.json");
                if package_json.exists() {
                    if let Ok(content) = std::fs::read_to_string(&package_json) {
                        if let Ok(json) = serde_json::from_str::<Value>(&content) {
                            if let Some(main) = json.get("main").and_then(|m| m.as_str()) {
                                let main_path = node_modules.join(main);
                                if main_path.exists() {
                                    return main_path;
                                }
                            }
                        }
                    }
                    // Fall back to index.js
                    let index_path = node_modules.join("index.js");
                    if index_path.exists() {
                        return index_path;
                    }
                }
            } else {
                // It's a file, return it
                return node_modules;
            }
        }

        // Move up to parent directory
        if let Some(parent) = current.parent() {
            current = parent;
        } else {
            break;
        }
    }

    // Fallback: just return the path as-is (will fail later)
    base.join(module_path)
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
                let source_path = result
                    .symbols
                    .iter()
                    .find(|s| s.kind == SymbolKind::Module)
                    .map(|s| s.file.parent().unwrap_or(&s.file))
                    .map(|p| resolve_module_path(p, &imp.source, root))
                    .unwrap_or_else(|| root.join(&imp.source));

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
                        });
                    } else if imp.is_default {
                        // For CommonJS default imports, fall back to global def_index
                        // This handles cases where module.exports = function() {} patterns
                        if let Some(&def_id) = def_index.get(&imp.local_name) {
                            program.references.push(Reference {
                                from: imp.symbol_id,
                                to: def_id,
                                kind: RefKind::ReExport,
                            });
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
