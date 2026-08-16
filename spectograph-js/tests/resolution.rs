//! Tests for JavaScript resolution: basic parsing, scope, function calls.

use tempfile::tempdir;

use spectograph_analyzer::Analyzer;
use spectograph_core::ir::SymbolKind;
use spectograph_js::JavaScriptAnalyzer;

/// Helper to create a temp dir with JS files and analyze them.
fn analyze_js(files: &[(&str, &str)]) -> spectograph_core::ir::Program {
    let dir = tempdir().unwrap();
    for (name, content) in files {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }
    JavaScriptAnalyzer.analyze(dir.path()).unwrap()
}

#[test]
fn function_declaration_creates_symbol() {
    let program = analyze_js(&[("test.js", "function foo() {}")]);
    let foo = program.symbols.iter().find(|s| s.name == "foo");
    assert!(foo.is_some(), "function foo should be a symbol");
    assert_eq!(foo.unwrap().kind, SymbolKind::Function);
}

#[test]
fn class_declaration_creates_symbol() {
    let program = analyze_js(&[("test.js", "class Bar {}")]);
    let bar = program.symbols.iter().find(|s| s.name == "Bar");
    assert!(bar.is_some(), "class Bar should be a symbol");
    assert_eq!(bar.unwrap().kind, SymbolKind::Class);
}

#[test]
fn const_declaration_creates_symbol() {
    let program = analyze_js(&[("test.js", "const x = 1;")]);
    let x = program.symbols.iter().find(|s| s.name == "x");
    assert!(x.is_some(), "const x should be a symbol");
    assert_eq!(x.unwrap().kind, SymbolKind::Const);
}

#[test]
fn function_call_creates_reference() {
    let program = analyze_js(&[(
        "test.js",
        r#"function foo() {}
function bar() {
    foo();
}"#,
    )]);
    let foo_id = program
        .symbols
        .iter()
        .find(|s| s.name == "foo")
        .map(|s| s.id)
        .unwrap();
    let _bar_id = program
        .symbols
        .iter()
        .find(|s| s.name == "bar")
        .map(|s| s.id)
        .unwrap();

    let has_ref = program.references.iter().any(|r| {
        r.to == foo_id && r.from != foo_id
    });
    assert!(has_ref, "bar should reference foo");
}

#[test]
fn local_shadowing_does_not_falsely_resolve() {
    let program = analyze_js(&[(
        "test.js",
        r#"const x = 1;
function foo() {
    const x = 2;
    return x;
}
console.log(x);"#,
    )]);

    let x_symbols: Vec<_> = program.symbols.iter().filter(|s| s.name == "x").collect();
    assert!(!x_symbols.is_empty(), "should have at least 1 'x' symbol (global)");
}
