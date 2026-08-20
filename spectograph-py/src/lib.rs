//! Spectograph Python analyzer.
//!
//! Parses Python source with tree-sitter into the shared [`Program`] IR.

pub mod entrypoints;
pub mod parse;
pub mod pyright;

use std::collections::HashMap;
use std::path::Path;

use entrypoints::scripts_from_pyproject;
use parse::parse_file;
use spectograph_analyzer::{discover, AnalyzeError, Analyzer};
use spectograph_core::ir::{
    Environment, Language, Program, RefKind, Reference, Symbol, SymbolId, SymbolKind,
};

/// Analyzer for the Python language.
pub struct PythonAnalyzer;

/// Which reference-resolution backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionMode {
    /// Hand-rolled static resolver (scope, aliasing, methods, inheritance).
    Static,
    /// basedpyright type-aware resolver (via LSP find-references).
    Pyright,
}

impl PythonAnalyzer {
    /// Analyze with a resolution backend. `Static` is equivalent to [`Analyzer::analyze`].
    pub fn analyze_with_resolution(
        &self,
        root: &Path,
        resolution: ResolutionMode,
        pyright_executable: Option<&str>,
        timeout_ms: u64,
    ) -> Result<Program, AnalyzeError> {
        let mut program = Analyzer::analyze(self, root)?;
        if resolution == ResolutionMode::Static {
            return Ok(program);
        }
        // Default to `basedpyright-langserver` on PATH unless an explicit executable is
        // given. (The npm basedpyright package ships both a CLI checker and this LSP
        // server; we need the latter for find-references.)
        let executable = match pyright_executable {
            Some(e) => e.to_string(),
            None => "basedpyright-langserver".to_string(),
        };
        let edges = crate::pyright::resolve_program_edges(root, &program, &executable, timeout_ms)
            .map_err(|e| AnalyzeError::Io(std::io::Error::other(e.to_string())))?;
        // Replace the static call-graph edges with the type-aware ones.
        let callable: std::collections::HashSet<SymbolId> = program
            .symbols
            .iter()
            .filter(|s| matches!(s.kind, SymbolKind::Function | SymbolKind::Class))
            .map(|s| s.id)
            .collect();
        program
            .references
            .retain(|r| !(callable.contains(&r.from) && callable.contains(&r.to)));
        for (from, to) in edges {
            program.references.push(Reference {
                from,
                to,
                kind: RefKind::Static,
                environment: Environment::Universal,
            });
        }
        Ok(program)
    }
}

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
        // Function/Class index scoped by (module_id, name) for entrypoint resolution.
        let mut def_by_module: HashMap<(SymbolId, String), SymbolId> = HashMap::new();
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
                        def_by_module
                            .entry((module_id, sym.name.clone()))
                            .or_insert(sym.id);
                    }
                }
            }
            program.symbols.extend(result.symbols.clone());
            parsed.push((path.to_string_lossy().into_owned(), result));
        }

        // Dotted module path -> module id, and (module id, name) -> symbol id.
        let module_paths: HashMap<String, SymbolId> = program
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Module)
            .filter_map(|s| module_path_of(&s.file, &program.root).map(|p| (p, s.id)))
            .collect();
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
                        environment: Environment::Universal,
                    });
                }
            }
        }

        // Pass 3: link each import to the definition it re-exports, so reachability
        // flows through imports to the actual code. Prefer module-scoped resolution
        // (handles relative imports correctly); fall back to the global name index.
        let import_ids: Vec<SymbolId> = program
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Import)
            .map(|s| s.id)
            .collect();
        for import_id in import_ids {
            let sym = program.symbol(import_id);
            let Some(sym) = sym else { continue };

            // Find this import's ImportDecl and the module that declared it.
            let decl_info = parsed.iter().find_map(|(_p, r)| {
                r.imports
                    .iter()
                    .find(|d| d.symbol_id == import_id)
                    .map(|d| (r.module_id, d))
            });

            let def_id = match decl_info {
                Some((importing_module, decl)) => resolve_import_target(
                    &module_paths,
                    &name_in_module,
                    &sym.name,
                    importing_module,
                    decl,
                )
                .or_else(|| def_index.get(&sym.name).copied()),
                None => def_index.get(&sym.name).copied(),
            };

            if let Some(def_id) = def_id {
                if def_id != import_id {
                    program.references.push(Reference {
                        from: import_id,
                        to: def_id,
                        kind: RefKind::ReExport,
                        environment: Environment::Universal,
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
            let is_pkg_root = module_file
                .file_name()
                .map(|n| n == "__init__.py")
                .unwrap_or(false);

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
                            environment: Environment::Universal,
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
                    environment: Environment::Universal,
                    is_external: false,
                });
                // module -> export
                program.references.push(Reference {
                    from: module_id,
                    to: id,
                    kind: RefKind::ReExport,
                    environment: Environment::Universal,
                });
                // export -> definition
                if let Some(&def_id) = def_index.get(&decl.name) {
                    program.references.push(Reference {
                        from: id,
                        to: def_id,
                        kind: RefKind::ReExport,
                        environment: Environment::Universal,
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

        // Console-script entrypoints from pyproject.toml `[project.scripts]`.
        // Resolve `module.path:function` to the module symbol and the function symbol.
        let scripts = scripts_from_pyproject(root)?;
        if !scripts.is_empty() {
            for ep in scripts {
                if let Some(&module_id) = module_paths.get(&ep.module_path) {
                    if let Some(&func_id) = def_by_module.get(&(module_id, ep.function)) {
                        if !program.entrypoints.contains(&func_id) {
                            program.entrypoints.push(func_id);
                        }
                    }
                }
            }
        }

        Ok(program)
    }
}

/// Compute a file's dotted module path relative to `root`.
///
/// - `pkg/mod.py`       -> `pkg.mod`
/// - `pkg/mod/__init__.py` -> `pkg.mod`
fn module_path_of(file: &Path, root: &Path) -> Option<String> {
    let rel = file.strip_prefix(root).unwrap_or(file);
    let mut comps: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    if comps.is_empty() {
        return None;
    }
    let is_init = comps.last().map(|s| s == "__init__.py").unwrap_or(false);
    if is_init {
        comps.pop();
    } else if let Some(last) = comps.last_mut() {
        if let Some(ext) = last.rsplit_once('.') {
            *last = ext.0.to_string();
        }
    }
    if comps.is_empty() {
        None
    } else {
        Some(comps.join("."))
    }
}

/// Resolve an import to the symbol it re-exports, using module-scoped resolution.
///
/// `importing_module` is the module containing the import; `decl` carries its relative
/// depth and dotted module path. The imported `name` is looked up within the resolved
/// target module, so `from .util import helper` finds `helper` in `pkg.util`, not some
/// other module's `helper`.
fn resolve_import_target(
    module_paths: &HashMap<String, SymbolId>,
    name_in_module: &HashMap<(SymbolId, String), SymbolId>,
    name: &str,
    importing_module: SymbolId,
    decl: &parse::ImportDecl,
) -> Option<SymbolId> {
    let importing_path = module_paths
        .iter()
        .find(|(_, &id)| id == importing_module)
        .map(|(p, _)| p.clone())?;
    let target = if decl.depth > 0 {
        relative_target(&importing_path, decl.depth, &decl.module_path)?
    } else {
        decl.module_path.clone()
    };
    if target.is_empty() {
        return None;
    }
    let target_module = module_paths.get(&target)?;
    name_in_module
        .get(&(*target_module, name.to_string()))
        .copied()
}

/// Compute the dotted module path a relative import refers to.
///
/// `importing` is the importing module's full dotted path (e.g. `pkg.sub.mod`).
/// Its package is all but the last component. `depth` dots climb up from that package:
/// `depth == 1` stays in the package, `2` goes up one, etc. The `module_path` suffix is
/// appended (empty for `from . import x`).
fn relative_target(importing: &str, depth: usize, module_path: &str) -> Option<String> {
    let mut comps: Vec<&str> = importing.split('.').collect();
    // Drop the module's own file component -> the containing package.
    comps.pop()?;
    // Climb up (depth - 1) more levels.
    let up = depth.saturating_sub(1);
    if comps.len() < up {
        return None;
    }
    comps.truncate(comps.len() - up);
    for part in module_path.split('.').filter(|s| !s.is_empty()) {
        comps.push(part);
    }
    if comps.is_empty() {
        None
    } else {
        Some(comps.join("."))
    }
}
