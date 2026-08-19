//! Tests for JavaScript runtime-environment detection: `typeof window`/`typeof process`
//! guards, bare use of environment-only globals, and package.json `browser` remapping.

use tempfile::tempdir;

use spectograph_analyzer::Analyzer;
use spectograph_core::ir::Environment;
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
fn typeof_window_guard_tags_consequence_browser() {
    let program = analyze_js(&[(
        "test.js",
        r#"if (typeof window !== 'undefined') {
    function browserOnly() { return 1; }
}"#,
    )]);
    let sym = program
        .symbols
        .iter()
        .find(|s| s.name == "browserOnly")
        .expect("browserOnly should be a symbol");
    assert_eq!(sym.environment, Environment::Browser);
}

#[test]
fn typeof_process_guard_tags_consequence_node() {
    let program = analyze_js(&[(
        "test.js",
        r#"if (typeof process !== 'undefined') {
    function nodeOnly() { return 1; }
}"#,
    )]);
    let sym = program
        .symbols
        .iter()
        .find(|s| s.name == "nodeOnly")
        .expect("nodeOnly should be a symbol");
    assert_eq!(sym.environment, Environment::Node);
}

#[test]
fn else_branch_gets_opposite_environment() {
    let program = analyze_js(&[(
        "test.js",
        r#"if (typeof window !== 'undefined') {
    function browserSide() { return 1; }
} else {
    function nodeSide() { return 2; }
}"#,
    )]);
    let browser_side = program
        .symbols
        .iter()
        .find(|s| s.name == "browserSide")
        .unwrap();
    let node_side = program
        .symbols
        .iter()
        .find(|s| s.name == "nodeSide")
        .unwrap();
    assert_eq!(browser_side.environment, Environment::Browser);
    assert_eq!(node_side.environment, Environment::Node);
}

#[test]
fn negated_guard_flips_branches() {
    // `typeof window === 'undefined'` is true when window does NOT exist, so the
    // consequence is the Node-ish branch and the alternative is the Browser-ish one.
    let program = analyze_js(&[(
        "test.js",
        r#"if (typeof window === 'undefined') {
    function noWindow() { return 1; }
} else {
    function hasWindow() { return 2; }
}"#,
    )]);
    let no_window = program
        .symbols
        .iter()
        .find(|s| s.name == "noWindow")
        .unwrap();
    let has_window = program
        .symbols
        .iter()
        .find(|s| s.name == "hasWindow")
        .unwrap();
    assert_eq!(no_window.environment, Environment::Node);
    assert_eq!(has_window.environment, Environment::Browser);
}

#[test]
fn reversed_operand_order_is_still_detected() {
    let program = analyze_js(&[(
        "test.js",
        r#"if ('undefined' !== typeof document) {
    function hasDocument() { return 1; }
}"#,
    )]);
    let sym = program
        .symbols
        .iter()
        .find(|s| s.name == "hasDocument")
        .unwrap();
    assert_eq!(sym.environment, Environment::Browser);
}

#[test]
fn unguarded_global_usage_tags_enclosing_function() {
    // No `typeof` guard at all -- just touching `window` directly is still a signal.
    let program = analyze_js(&[(
        "test.js",
        r#"function paint() {
    window.document.title = "hi";
}"#,
    )]);
    let sym = program.symbols.iter().find(|s| s.name == "paint").unwrap();
    assert_eq!(sym.environment, Environment::Browser);
}

#[test]
fn unguarded_node_global_tags_module() {
    // Top-level (module-scope) use of a Node-only global, no function wrapper.
    let program = analyze_js(&[("test.js", "console.log(process.env.NODE_ENV);")]);
    let module_sym = program
        .symbols
        .iter()
        .find(|s| s.kind == spectograph_core::ir::SymbolKind::Module)
        .unwrap();
    assert_eq!(module_sym.environment, Environment::Node);
}

#[test]
fn conditional_require_is_tagged_and_resolves() {
    let program = analyze_js(&[
        (
            "main.js",
            r#"if (typeof window !== 'undefined') {
    const lib = require('./browser-lib.js');
} else {
    const lib = require('./node-lib.js');
}"#,
        ),
        ("browser-lib.js", "function lib() {}\nmodule.exports = lib;"),
        ("node-lib.js", "function lib() {}\nmodule.exports = lib;"),
    ]);
    let lib_import = program
        .symbols
        .iter()
        .find(|s| s.name == "lib" && s.kind == spectograph_core::ir::SymbolKind::Import)
        .expect("lib import should exist");
    assert_eq!(lib_import.environment, Environment::Browser);
}

#[test]
fn browser_field_remap_tags_reference_browser() {
    let program = analyze_js(&[
        (
            "package.json",
            r#"{"name": "pkg", "main": "index.js", "browser": {"./server.js": "./client.js"}}"#,
        ),
        ("index.js", "const impl = require('./server.js');\nimpl();"),
        ("server.js", "function impl() {}\nmodule.exports = impl;"),
        ("client.js", "function impl() {}\nmodule.exports = impl;"),
    ]);
    let import_sym = program
        .symbols
        .iter()
        .find(|s| s.name == "impl" && s.kind == spectograph_core::ir::SymbolKind::Import)
        .expect("impl import should exist");
    let has_browser_ref = program
        .references
        .iter()
        .any(|r| r.from == import_sym.id && r.environment == Environment::Browser);
    assert!(
        has_browser_ref,
        "import resolved via browser field remap should carry Browser environment"
    );
}

#[test]
fn browser_field_string_form_is_an_entrypoint() {
    let program = analyze_js(&[
        (
            "package.json",
            r#"{"name": "pkg", "main": "index.js", "browser": "browser-entry.js"}"#,
        ),
        ("index.js", "function main() {}"),
        ("browser-entry.js", "function browserMain() {}"),
    ]);
    let browser_entry_module = program
        .symbols
        .iter()
        .find(|s| {
            s.kind == spectograph_core::ir::SymbolKind::Module
                && s.file.ends_with("browser-entry.js")
        })
        .expect("browser-entry.js module symbol should exist");
    assert!(
        program.entrypoints.contains(&browser_entry_module.id),
        "browser-entry.js should be registered as an entrypoint"
    );
}

#[test]
fn no_guard_leaves_environment_universal() {
    let program = analyze_js(&[("test.js", "function plain() { return 1 + 1; }")]);
    let sym = program.symbols.iter().find(|s| s.name == "plain").unwrap();
    assert_eq!(sym.environment, Environment::Universal);
}
