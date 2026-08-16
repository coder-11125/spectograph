//! Spectograph JavaScript analyzer.
//!
//! Parses JavaScript source with tree-sitter into the shared [`Program`] IR.

pub mod entrypoints;
pub mod parse;

use std::collections::HashMap;
use std::path::Path;

use entrypoints::detect_entrypoint_symbols;
use parse::parse_file;
use spectograph_analyzer::{discover, AnalyzeError, Analyzer};
use spectograph_core::ir::{Language, Program, RefKind, Reference, SymbolId, SymbolKind};

/// Analyzer for the JavaScript language.
pub struct JavaScriptAnalyzer;

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
                // Resolve the source module
                let source_path = result
                    .symbols
                    .iter()
                    .find(|s| s.kind == SymbolKind::Module)
                    .map(|s| s.file.parent().unwrap_or(&s.file))
                    .map(|p| p.join(&imp.source))
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
                        // Try to find default export in the target module
                        // Look for a symbol with the same name as the import
                        if let Some(&def_id) = def_index.get(&imp.local_name) {
                            program.references.push(Reference {
                                from: imp.symbol_id,
                                to: def_id,
                                kind: RefKind::ReExport,
                            });
                        }
                    }
                }
            }
        }

        // Pass 4: detect and mark entrypoints
        program.entrypoints = detect_entrypoint_symbols(&program, &file_module, root)
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        Ok(program)
    }
}
