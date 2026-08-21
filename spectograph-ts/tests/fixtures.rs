//! Automated tests that run the full analyzer against the committed fixtures in
//! `fixtures/typescript/`.

use std::path::PathBuf;

use spectograph_analyzer::Analyzer;
use spectograph_core::ir::SymbolKind;
use spectograph_ts::TypeScriptAnalyzer;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures/typescript")
}

fn analyze(name: &str) -> spectograph_core::ir::Program {
    let dir = fixtures_dir().join(name);
    TypeScriptAnalyzer
        .analyze(&dir)
        .unwrap_or_else(|e| panic!("analyze {name}: {e}"))
}

#[test]
fn ts_sample_fixture() {
    let program = analyze("ts-sample");
    let names: Vec<_> = program.symbols.iter().map(|s| s.name.clone()).collect();
    assert!(names.contains(&"add".to_string()), "add should exist");
    assert!(
        names.contains(&"Calculator".to_string()),
        "Calculator class should exist"
    );
    assert!(
        names.contains(&"Calculator.reset".to_string()),
        "Calculator.reset method should exist"
    );
    assert!(
        names.contains(&"calc".to_string()),
        "calc const should exist"
    );
    assert!(
        names.contains(&"subtract".to_string()),
        "exported subtract function should exist"
    );

    let calculator = program
        .symbols
        .iter()
        .find(|s| s.name == "Calculator")
        .unwrap();
    assert_eq!(calculator.kind, SymbolKind::Class);
}
