//! Spectograph Python analyzer.
//!
//! Parses Python source with tree-sitter into the shared [`Program`] IR.

pub mod parse;

use std::collections::HashMap;
use std::path::Path;

use parse::parse_file;
use spectograph_analyzer::{Analyzer, AnalyzeError, discover};
use spectograph_core::ir::{
    Language, Program, Reference, RefKind, Symbol, SymbolId, SymbolKind,
};

/// Analyzer for the Python language.
pub struct PythonAnalyzer;

impl Analyzer for PythonAnalyzer {
    fn language(&self) -> Language {
        Language::Python
    }

    fn analyze(&self, root: &Path) -> Result<Program, AnalyzeError> {
        let files = discover::discover_files(root, Language::Python)?;
        if files.is_empty() {
            return Err(AnalyzeError::NoSourceFiles(root.display().to_string()));
        }

        let mut program = Program {
            language: Language::Python,
            root: root.to_path_buf(),
            symbols: Vec::new(),
            references: Vec::new(),
            entrypoints: Vec::new(),
            reachable: Vec::new(),
        };

        let mut next_id: usize = 0;
        // Global name -> symbol id (used for cross-file reference resolution).
        let mut global_names: HashMap<String, SymbolId> = HashMap::new();
        // Definition index (Function/Class) by name, for import re-export linking.
        let mut def_index: HashMap<String, SymbolId> = HashMap::new();
        // Track each file's module id and whether it contains an entrypoint.
        let mut file_entry: HashMap<String, bool> = HashMap::new();

        // Pass 1: parse each file, collect its symbols and unresolved refs.
        let mut parsed: Vec<(String, parse::FileResult)> = Vec::new();
        for path in &files {
            let src = std::fs::read_to_string(path)?;
            let module_id = next_id;
            next_id += 1;

            let result = parse_file(path, &src, module_id, &mut next_id);

            // Record entrypoint detection: file contains a `__main__` guard.
            let has_main = src.contains("__main__");
            file_entry.insert(path.to_string_lossy().into_owned(), has_main);

            for sym in &result.symbols {
                if sym.kind != spectograph_core::ir::SymbolKind::Module {
                    // Index by name, first definition wins for the vertical slice.
                    global_names.entry(sym.name.clone()).or_insert(sym.id);
                    if matches!(
                        sym.kind,
                        spectograph_core::ir::SymbolKind::Function
                            | spectograph_core::ir::SymbolKind::Class
                    ) {
                        def_index.entry(sym.name.clone()).or_insert(sym.id);
                    }
                }
            }
            program.symbols.extend(result.symbols.clone());
            parsed.push((path.to_string_lossy().into_owned(), result));
        }

        // Pass 2: resolve references. Already-symbol targets are used directly;
        // name targets (dynamic/reflection) resolve against the global index.
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

        // Pass 3: link each import to the definition it re-exports, so reachability
        // flows through imports to the actual code.
        let import_ids: Vec<SymbolId> = program
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Import)
            .map(|s| s.id)
            .collect();
        for import_id in import_ids {
            let sym = program.symbol(import_id);
            let Some(sym) = sym else { continue };
            if let Some(&def_id) = def_index.get(&sym.name) {
                if def_id != import_id {
                    program.references.push(Reference {
                        from: import_id,
                        to: def_id,
                        kind: RefKind::ReExport,
                    });
                }
            }
        }

        // Pass 4: create Export symbols for each `__all__` declaration, link them to
        // their definitions, and add module -> import/export edges so reaching a module
        // reaches its re-exports and public API.
        for (_path, result) in &parsed {
            let module_id = result.module_id;
            let module_file = program
                .symbol(module_id)
                .map(|s| s.file.clone())
                .unwrap_or_default();
            let is_pkg_root = module_file.file_name().map(|n| n == "__init__.py").unwrap_or(false);

            // In `__init__.py`, top-level imports ARE re-exports (public API), so
            // reaching the package root reaches them. In ordinary modules an import is
            // only "used" when referenced, so we don't add this edge there.
            if is_pkg_root {
                for sym in &result.symbols {
                    if sym.kind == SymbolKind::Import {
                        program.references.push(Reference {
                            from: module_id,
                            to: sym.id,
                            kind: RefKind::ReExport,
                        });
                    }
                }
            }

            // `__all__` exports.
            for decl in &result.exports {
                let id = next_id;
                next_id += 1;
                program.symbols.push(Symbol {
                    id,
                    name: decl.name.clone(),
                    kind: SymbolKind::Export,
                    file: module_file.clone(),
                    span: decl.span,
                    reachable: false,
                });
                // module -> export
                program.references.push(Reference {
                    from: module_id,
                    to: id,
                    kind: RefKind::ReExport,
                });
                // export -> definition
                if let Some(&def_id) = def_index.get(&decl.name) {
                    program.references.push(Reference {
                        from: id,
                        to: def_id,
                        kind: RefKind::ReExport,
                    });
                }
            }
        }

        // Entrypoints: modules with a `__main__` guard, plus `__init__.py` package roots.
        for sym in &program.symbols {
            if sym.kind == SymbolKind::Module {
                let file = sym.file.to_string_lossy().into_owned();
                let is_init = sym
                    .file
                    .file_name()
                    .map(|n| n == "__init__.py")
                    .unwrap_or(false);
                if file_entry.get(&file).copied().unwrap_or(false) || is_init {
                    program.entrypoints.push(sym.id);
                }
            }
        }

        Ok(program)
    }
}
