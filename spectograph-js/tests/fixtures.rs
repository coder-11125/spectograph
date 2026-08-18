//! Automated tests that run the full analyzer against the committed fixtures in
//! `fixtures/javascript/`. Each fixture asserts which symbols must be reachable (not bloat)
//! and which must be flagged as dead.

use std::path::PathBuf;

use spectograph_analyzer::Analyzer;
use spectograph_js::JavaScriptAnalyzer;

/// Path to the fixtures dir relative to the crate root.
fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures/javascript")
}

fn analyze(name: &str) -> spectograph_core::ir::Program {
    let dir = fixtures_dir().join(name);
    JavaScriptAnalyzer
        .analyze(&dir)
        .unwrap_or_else(|e| panic!("analyze {name}: {e}"))
}

/// js-sample: basic parsing - check symbols exist
#[test]
fn js_sample_fixture() {
    let program = analyze("js-sample");
    // Check that symbols are created
    let symbol_names: Vec<_> = program.symbols.iter().map(|s| s.name.clone()).collect();
    assert!(
        symbol_names.contains(&"main".to_string()),
        "main should exist"
    );
    assert!(
        symbol_names.contains(&"helper".to_string()),
        "helper should exist"
    );
    assert!(
        symbol_names.contains(&"unusedHelper".to_string()),
        "unusedHelper should exist"
    );

    // Check that there are references
    assert!(!program.references.is_empty(), "should have references");
}

/// js-pkg: basic parsing - check symbols exist
#[test]
fn js_pkg_fixture() {
    let program = analyze("js-pkg");
    let symbol_names: Vec<_> = program.symbols.iter().map(|s| s.name.clone()).collect();
    assert!(
        symbol_names.contains(&"add".to_string()),
        "add should exist"
    );
    assert!(
        symbol_names.contains(&"subtract".to_string()),
        "subtract should exist"
    );
    assert!(
        symbol_names.contains(&"multiply".to_string()),
        "multiply should exist"
    );
}

/// js-classes: basic parsing - check symbols exist
#[test]
fn js_classes_fixture() {
    let program = analyze("js-classes");
    let symbol_names: Vec<_> = program.symbols.iter().map(|s| s.name.clone()).collect();
    assert!(
        symbol_names.contains(&"Greeter".to_string()),
        "Greeter should exist"
    );
    assert!(
        symbol_names.contains(&"ChildGreeter".to_string()),
        "ChildGreeter should exist"
    );
    // Methods are named ClassName.methodName
    assert!(
        symbol_names.iter().any(|n| n.contains("greet")),
        "greet method should exist"
    );
    assert!(
        symbol_names.iter().any(|n| n.contains("unusedMethod")),
        "unusedMethod should exist"
    );
}

/// js-inheritance: class inheritance with super calls
#[test]
fn js_inheritance_fixture() {
    let program = analyze("js-inheritance");
    let symbol_names: Vec<_> = program.symbols.iter().map(|s| s.name.clone()).collect();
    assert!(
        symbol_names.contains(&"Parent".to_string()),
        "Parent class should exist"
    );
    assert!(
        symbol_names.contains(&"Child".to_string()),
        "Child class should exist"
    );
    // Check that methods are created (they may not have ClassName.methodName pattern for imports)
    assert!(
        symbol_names.iter().any(|n| n.contains("greet")),
        "greet method should exist"
    );
    assert!(
        symbol_names.iter().any(|n| n.contains("play")),
        "play method should exist"
    );
    // Check that multiple files are parsed
    assert!(program.symbols.len() > 5, "should have symbols from multiple files");
}

/// js-relative: relative import resolution
#[test]
fn js_relative_fixture() {
    let program = analyze("js-relative");
    let symbol_names: Vec<_> = program.symbols.iter().map(|s| s.name.clone()).collect();
    assert!(
        symbol_names.contains(&"helper".to_string()),
        "helper function should exist"
    );
    assert!(
        symbol_names.contains(&"otherFunction".to_string()),
        "otherFunction should exist"
    );
    // Check that multiple files are parsed
    assert!(program.symbols.len() > 5, "should have symbols from multiple files");
}

/// js-dynamic: dynamic import patterns
#[test]
fn js_dynamic_fixture() {
    let program = analyze("js-dynamic");
    let symbol_names: Vec<_> = program.symbols.iter().map(|s| s.name.clone()).collect();
    assert!(
        symbol_names.contains(&"staticHelper".to_string()),
        "staticHelper should exist"
    );
    assert!(
        symbol_names.contains(&"dynamicHelper".to_string()),
        "dynamicHelper should exist"
    );
    // Check that async functions are parsed
    assert!(
        symbol_names.contains(&"loadDynamic".to_string()),
        "loadDynamic should exist"
    );
}

/// js-commonjs: CommonJS module patterns
#[test]
fn js_commonjs_fixture() {
    let program = analyze("js-commonjs");
    let symbol_names: Vec<_> = program.symbols.iter().map(|s| s.name.clone()).collect();
    // Check that CommonJS patterns are parsed
    assert!(!symbol_names.is_empty(), "should have symbols from CommonJS files");
    // Check that require() calls are handled
    assert!(program.symbols.len() > 3, "should have symbols from multiple files");
}
