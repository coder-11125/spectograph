//! Reachability and classification must stay linear in program size: a 200k-symbol,
//! 400k-reference project used to take minutes (every `incoming`/`outgoing` call rescanned
//! every reference) and has to finish in a couple of seconds even in a debug build.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use spectograph_core::analysis::{self, BloatTier};
use spectograph_core::cost::{CostModel, CostSource};
use spectograph_core::ir::{
    Environment, Language, Program, RefKind, Reference, SourceSpan, Symbol, SymbolKind,
};

fn sym(id: usize, kind: SymbolKind) -> Symbol {
    Symbol {
        id,
        name: format!("s{id}"),
        kind,
        file: PathBuf::from("big.js"),
        span: SourceSpan::new(1, 1, 1, 1),
        reachable: false,
        environment: Environment::Universal,
        is_external: false,
    }
}

fn edge(from: usize, to: usize) -> Reference {
    Reference {
        from,
        to,
        kind: RefKind::Static,
        environment: Environment::Universal,
    }
}

#[test]
fn analysis_is_linear_on_a_large_program() {
    const N: usize = 200_000;
    let mut symbols = vec![sym(0, SymbolKind::Module)];
    symbols.extend((1..N).map(|i| sym(i, SymbolKind::Function)));

    // A single chain 0 -> 1 -> ... -> N/2, plus a second half that nothing reaches. Every
    // symbol also gets a back-reference so there are ~2 edges per symbol.
    let mut references = Vec::new();
    for i in 0..N / 2 {
        references.push(edge(i, i + 1));
        references.push(edge(i + 1, i));
    }
    let mut program = Program {
        language: Language::JavaScript,
        root: PathBuf::from("."),
        symbols,
        references,
        entrypoints: vec![0],
        reachable: Vec::new(),
    };

    let start = Instant::now();
    let model = CostModel::build(&program, CostSource::Estimated);
    let report = analysis::analyze(&mut program, &model);
    let elapsed = start.elapsed();

    assert_eq!(report.reachable_symbols, N / 2 + 1);
    let dead = report
        .findings
        .iter()
        .filter(|f| f.tier == BloatTier::Guaranteed)
        .count();
    assert_eq!(dead, N - (N / 2 + 1));
    assert!(
        elapsed < Duration::from_secs(10),
        "analysis took {elapsed:?}; it should be linear in symbols + references"
    );
}
