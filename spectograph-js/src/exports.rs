//! Public-API reachability for exported declarations, and member-name resolution.
//!
//! `export function run() {}` produces an ordinary function symbol with no edge from its
//! module -- only `export { x }`, `export default`, and `export * from` produce `Export`
//! roots. Left like that, the declarations that *are* a package's public API (everything
//! exported from its `index.ts`) or a framework's route contract (`export default function
//! Page()` in `app/page.tsx`) look unreferenced and are reported as guaranteed bloat.
//!
//! Two rules fix that, both sound by construction:
//!
//! 1. **API roots.** An exported declaration in a module that [`crate::entrypoints`]
//!    identified as a public surface (package-declared or framework convention) is used by
//!    something outside the analyzed code.
//! 2. **Whole-module uses.** `export * from './x'`, `import * as ns from './x'`,
//!    `import('./x')`, and `require('./x')` use a module as a whole; its exported
//!    declarations are therefore reachable whenever that use is.
//!
//! Exports in any *other* module stay subject to ordinary analysis -- an exported function
//! nobody imports is exactly the bloat this tool exists to find.

use std::collections::{HashMap, HashSet};

use spectograph_core::ir::{Environment, Program, RefKind, Reference, SymbolId};

/// Exported declarations per module symbol, as recorded by each file's parse.
pub type ExportedByModule = HashMap<SymbolId, Vec<SymbolId>>;

/// Add `Static` edges from modules (rule 1) and from whole-module users (rule 2) to the
/// exported declarations of the module in question.
///
/// `api_roots` are module ids [`crate::entrypoints::detect_entries`] reported as public
/// surfaces; `whole_module_uses` are `(user_symbol, module)` pairs.
pub fn add_export_edges(
    program: &mut Program,
    exported_by_module: &ExportedByModule,
    api_roots: &[SymbolId],
    whole_module_uses: &[(SymbolId, SymbolId)],
) {
    let mut edges: HashSet<(SymbolId, SymbolId)> = HashSet::new();

    for module in api_roots {
        if let Some(exported) = exported_by_module.get(module) {
            edges.extend(exported.iter().map(|e| (*module, *e)));
        }
    }
    for (user, module) in whole_module_uses {
        if let Some(exported) = exported_by_module.get(module) {
            edges.extend(exported.iter().map(|e| (*user, *e)));
        }
    }

    let mut edges: Vec<(SymbolId, SymbolId)> = edges.into_iter().collect();
    edges.sort_unstable();
    program
        .references
        .extend(edges.into_iter().map(|(from, to)| Reference {
            from,
            to,
            kind: RefKind::Static,
            environment: Environment::Universal,
        }));
}

/// Index of every symbol a property access can name, by short member name:
/// `Service.run` and `Color.Red` are found under `run` and `Red`, and every exported
/// declaration under its own name (so `ns.helper()` on a namespace import finds `helper`).
pub fn member_index(
    program: &Program,
    exported_by_module: &ExportedByModule,
) -> HashMap<String, Vec<SymbolId>> {
    let mut index: HashMap<String, Vec<SymbolId>> = HashMap::new();
    for sym in &program.symbols {
        if let Some((_, short)) = sym.name.rsplit_once('.') {
            if !short.is_empty() {
                index.entry(short.to_string()).or_default().push(sym.id);
            }
        }
    }
    for ids in exported_by_module.values() {
        for id in ids {
            if let Some(sym) = program.symbols.get(*id) {
                if !sym.name.contains('.') {
                    index.entry(sym.name.clone()).or_default().push(*id);
                }
            }
        }
    }
    for ids in index.values_mut() {
        ids.sort_unstable();
        ids.dedup();
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use spectograph_core::ir::{Language, SourceSpan, Symbol, SymbolKind};

    fn sym(id: usize, name: &str, kind: SymbolKind) -> Symbol {
        Symbol {
            id,
            name: name.to_string(),
            kind,
            file: PathBuf::from("a.ts"),
            span: SourceSpan::new(1, 1, 1, 1),
            reachable: false,
            environment: Environment::Universal,
            is_external: false,
        }
    }

    fn program(symbols: Vec<Symbol>) -> Program {
        Program {
            language: Language::TypeScript,
            root: PathBuf::from("."),
            symbols,
            references: Vec::new(),
            entrypoints: Vec::new(),
            reachable: Vec::new(),
        }
    }

    #[test]
    fn api_roots_and_whole_module_users_reach_the_exports() {
        let mut p = program(vec![
            sym(0, "m", SymbolKind::Module),
            sym(1, "run", SymbolKind::Function),
            sym(2, "user", SymbolKind::Function),
        ]);
        let exported: ExportedByModule = HashMap::from([(0, vec![1])]);
        add_export_edges(&mut p, &exported, &[0], &[(2, 0)]);
        let edges: Vec<(usize, usize)> = p.references.iter().map(|r| (r.from, r.to)).collect();
        assert!(edges.contains(&(0, 1)), "{edges:?}");
        assert!(edges.contains(&(2, 1)), "{edges:?}");
    }

    #[test]
    fn member_index_finds_dotted_members_and_exports() {
        let p = program(vec![
            sym(0, "m", SymbolKind::Module),
            sym(1, "Service.run", SymbolKind::Function),
            sym(2, "helper", SymbolKind::Function),
        ]);
        let exported: ExportedByModule = HashMap::from([(0, vec![2])]);
        let index = member_index(&p, &exported);
        assert_eq!(index.get("run"), Some(&vec![1]));
        assert_eq!(index.get("helper"), Some(&vec![2]));
    }
}
