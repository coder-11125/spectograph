//! Tests for reachability and bloat classification.

use std::path::PathBuf;

use spectograph_core::analysis::{self, BloatTier};
use spectograph_core::cost::CostModel;
use spectograph_core::ir::{
    Environment, Language, Program, RefKind, Reference, SourceSpan, Symbol, SymbolKind,
};

fn sym(id: usize, name: &str, kind: SymbolKind) -> Symbol {
    Symbol {
        id,
        name: name.to_string(),
        kind,
        file: PathBuf::from("test.py"),
        span: SourceSpan::new(1, 1, 1, 1),
        reachable: false,
        environment: Environment::Universal,
    }
}

fn program() -> Program {
    // module(0) -> a(1); a -> b(2). c(3) is orphaned. import(4) -> d(5).
    Program {
        language: Language::Python,
        root: PathBuf::from("."),
        symbols: vec![
            sym(0, "module", SymbolKind::Module),
            sym(1, "a", SymbolKind::Function),
            sym(2, "b", SymbolKind::Function),
            sym(3, "c", SymbolKind::Function),
            sym(4, "d_import", SymbolKind::Import),
            sym(5, "d", SymbolKind::Function),
        ],
        references: vec![
            Reference {
                from: 0,
                to: 1,
                kind: RefKind::Static,
                environment: Environment::Universal,
            },
            Reference {
                from: 1,
                to: 2,
                kind: RefKind::Static,
                environment: Environment::Universal,
            },
            Reference {
                from: 4,
                to: 5,
                kind: RefKind::ReExport,
                environment: Environment::Universal,
            },
        ],
        entrypoints: vec![0],
        reachable: Vec::new(),
    }
}

#[test]
fn reachability_traverses_static_edges_only() {
    let mut p = program();
    analysis::compute_reachability(&mut p);
    // 0 -> 1 -> 2 all reachable; 3 orphan; import/d handled via re-export is static-like
    // but re-export from an unreachable import keeps it unreachable.
    assert!(p.symbol(1).unwrap().reachable);
    assert!(p.symbol(2).unwrap().reachable);
    assert!(!p.symbol(3).unwrap().reachable);
    assert!(!p.symbol(5).unwrap().reachable);
}

#[test]
fn orphan_symbol_is_guaranteed_bloat() {
    let mut p = program();
    let model = CostModel::build(&p, spectograph_core::cost::CostSource::Estimated);
    let report = analysis::analyze(&mut p, &model);
    let c = report
        .findings
        .iter()
        .find(|f| f.name == "c")
        .expect("c finding");
    assert_eq!(c.tier, BloatTier::Guaranteed);
}

#[test]
fn reexport_only_symbol_is_probable_bloat() {
    let mut p = program();
    let model = CostModel::build(&p, spectograph_core::cost::CostSource::Estimated);
    let report = analysis::analyze(&mut p, &model);
    let d = report
        .findings
        .iter()
        .find(|f| f.name == "d")
        .expect("d finding");
    assert_eq!(d.tier, BloatTier::Probable);
}
