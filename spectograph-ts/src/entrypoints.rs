//! Entrypoint detection for TypeScript projects.
//!
//! Narrows entrypoints down to the files a project's own tooling or convention
//! actually treats as an entry, instead of the placeholder "every module is a
//! potential entrypoint" default used before this landed: `package.json`'s
//! `main`/`bin`/`browser`/`exports` fields (the format isn't TS-specific, so
//! this reuses [`spectograph_js::entrypoints::detect_from_package_json`]
//! rather than duplicating it), plus the conventional `index.ts(x)`/`main.ts(x)`
//! at the project root. Only when none of those resolve to anything does it
//! fall back to treating every non-external module as an entrypoint, so a
//! project with no recognizable entry convention doesn't get reported as 100%
//! bloat.

use std::path::{Path, PathBuf};

use spectograph_core::ir::{Program, SymbolId, SymbolKind};

const CONVENTIONAL_ENTRY_NAMES: &[&str] = &[
    "index.ts",
    "index.tsx",
    "index.mts",
    "index.cts",
    "main.ts",
    "main.tsx",
];

/// Detect entrypoint module symbol ids for an already-built [`Program`].
pub fn detect_entrypoint_symbols(program: &Program, root: &Path) -> Vec<SymbolId> {
    let module_by_file: std::collections::HashMap<&Path, SymbolId> = program
        .symbols
        .iter()
        .filter(|s| s.kind == SymbolKind::Module)
        .map(|s| (s.file.as_path(), s.id))
        .collect();

    let mut entrypoints = Vec::new();
    let mut add = |candidate: Option<PathBuf>| {
        let Some(id) = candidate
            .as_deref()
            .and_then(|p| module_by_file.get(p))
            .copied()
        else {
            return;
        };
        if !entrypoints.contains(&id) {
            entrypoints.push(id);
        }
    };

    for ep in spectograph_js::entrypoints::detect_from_package_json(root).unwrap_or_default() {
        add(crate::resolve_file_candidate(&crate::normalize_path(
            &PathBuf::from(&ep.path),
        )));
    }
    for name in CONVENTIONAL_ENTRY_NAMES {
        add(crate::resolve_file_candidate(&root.join(name)));
    }

    if entrypoints.is_empty() {
        for sym in &program.symbols {
            if sym.kind == SymbolKind::Module && !sym.is_external {
                entrypoints.push(sym.id);
            }
        }
    }

    entrypoints
}

#[cfg(test)]
mod tests {
    use super::*;
    use spectograph_analyzer::Analyzer;
    use tempfile::tempdir;

    fn analyze(files: &[(&str, &str)]) -> Program {
        let dir = tempdir().unwrap();
        for (name, content) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
        }
        crate::TypeScriptAnalyzer.analyze(dir.path()).unwrap()
    }

    #[test]
    fn package_json_main_field_is_the_entrypoint() {
        let program = analyze(&[
            ("package.json", r#"{"main": "src/index.ts"}"#),
            ("src/index.ts", "export function run(): void {}"),
            ("src/other.ts", "export function unused(): void {}"),
        ]);
        let entry_files: Vec<String> = program
            .entrypoints
            .iter()
            .filter_map(|id| program.symbol(*id))
            .map(|s| s.file.to_string_lossy().into_owned())
            .collect();
        assert_eq!(entry_files.len(), 1);
        assert!(entry_files[0].ends_with("src/index.ts"));
    }

    #[test]
    fn conventional_index_ts_is_the_entrypoint_without_package_json() {
        let program = analyze(&[
            ("index.ts", "export function run(): void {}"),
            ("other.ts", "export function unused(): void {}"),
        ]);
        let entry_files: Vec<String> = program
            .entrypoints
            .iter()
            .filter_map(|id| program.symbol(*id))
            .map(|s| s.file.to_string_lossy().into_owned())
            .collect();
        assert_eq!(entry_files.len(), 1);
        assert!(entry_files[0].ends_with("index.ts"));
    }

    #[test]
    fn falls_back_to_every_module_when_no_convention_matches() {
        let program = analyze(&[
            ("a.ts", "export function a(): void {}"),
            ("b.ts", "export function b(): void {}"),
        ]);
        assert_eq!(program.entrypoints.len(), 2);
    }

    #[test]
    fn node_modules_files_are_excluded_from_the_fallback() {
        let program = analyze(&[
            ("a.ts", "export function a(): void {}"),
            ("node_modules/pkg/index.ts", "export function b(): void {}"),
        ]);
        let entry_files: Vec<String> = program
            .entrypoints
            .iter()
            .filter_map(|id| program.symbol(*id))
            .map(|s| s.file.to_string_lossy().into_owned())
            .collect();
        assert_eq!(entry_files.len(), 1);
        assert!(entry_files[0].ends_with("a.ts"));
    }
}
