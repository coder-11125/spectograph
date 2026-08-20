//! Tests for third-party library intelligence: recognizing common npm packages,
//! package-level entrypoint resolution (`main`/`exports`, sub-paths, scoped
//! packages), and excluding `node_modules` internals from bloat findings.

use std::collections::HashSet;

use tempfile::tempdir;

use spectograph_analyzer::Analyzer;
use spectograph_core::analysis;
use spectograph_core::cost::{CostModel, CostSource};
use spectograph_core::ir::SymbolKind;
use spectograph_js::{is_recognized_npm_package, npm_package_root, JavaScriptAnalyzer};

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

fn bloat_findings(program: &mut spectograph_core::ir::Program) -> Vec<analysis::BloatFinding> {
    let model = CostModel::build(program, CostSource::Estimated);
    analysis::analyze(program, &model).findings
}

// --- Common npm package recognition -----------------------------------------

#[test]
fn npm_package_root_handles_plain_and_scoped_packages() {
    assert_eq!(npm_package_root("lodash"), "lodash");
    assert_eq!(npm_package_root("lodash/debounce"), "lodash");
    assert_eq!(npm_package_root("@scope/pkg"), "@scope/pkg");
    assert_eq!(npm_package_root("@scope/pkg/sub/path"), "@scope/pkg");
}

#[test]
fn known_packages_are_recognized_without_declared_deps() {
    let empty: HashSet<String> = HashSet::new();
    assert!(is_recognized_npm_package("lodash/debounce", &empty));
    assert!(is_recognized_npm_package("react", &empty));
    assert!(!is_recognized_npm_package(
        "totally-made-up-package-xyz",
        &empty
    ));
}

#[test]
fn declared_dependency_is_recognized_even_if_not_curated() {
    let mut declared = HashSet::new();
    declared.insert("my-internal-lib".to_string());
    assert!(is_recognized_npm_package("my-internal-lib", &declared));
    assert!(is_recognized_npm_package(
        "my-internal-lib/sub/path",
        &declared
    ));
}

// --- Package-level entrypoint detection --------------------------------------

#[test]
fn node_modules_main_field_resolves() {
    let program = analyze_js(&[
        ("main.js", "const helper = require('pkg');\nhelper();"),
        (
            "node_modules/pkg/package.json",
            r#"{"name": "pkg", "main": "lib/entry.js"}"#,
        ),
        (
            "node_modules/pkg/lib/entry.js",
            "function helper() {}\nmodule.exports = helper;",
        ),
    ]);
    let helper_fn = program
        .symbols
        .iter()
        .find(|s| s.name == "helper" && s.kind == SymbolKind::Function);
    assert!(
        helper_fn.is_some(),
        "package main field should resolve to lib/entry.js"
    );
}

#[test]
fn node_modules_exports_field_takes_priority_over_main() {
    let program = analyze_js(&[
        ("main.js", "const helper = require('pkg');\nhelper();"),
        (
            "node_modules/pkg/package.json",
            r#"{"name": "pkg", "main": "old.js", "exports": "./modern.js"}"#,
        ),
        (
            "node_modules/pkg/old.js",
            "function oldEntry() {}\nmodule.exports = oldEntry;",
        ),
        (
            "node_modules/pkg/modern.js",
            "function helper() {}\nmodule.exports = helper;",
        ),
    ]);
    let import_sym = program
        .symbols
        .iter()
        .find(|s| s.name == "helper" && s.kind == SymbolKind::Import)
        .unwrap();
    let modern_helper = program
        .symbols
        .iter()
        .find(|s| {
            s.name == "helper" && s.kind == SymbolKind::Function && s.file.ends_with("modern.js")
        })
        .expect("modern.js's helper should exist");
    let old_entry = program
        .symbols
        .iter()
        .find(|s| s.name == "oldEntry")
        .expect("old.js is still a discovered file and gets parsed regardless of resolution");

    assert!(
        program
            .references
            .iter()
            .any(|r| r.from == import_sym.id && r.to == modern_helper.id),
        "the import should resolve via the exports field, to modern.js"
    );
    assert!(
        !program
            .references
            .iter()
            .any(|r| r.from == import_sym.id && r.to == old_entry.id),
        "the import must not resolve to old.js's export just because main pointed there"
    );
}

#[test]
fn node_modules_exports_condition_object_resolves() {
    let program = analyze_js(&[
        ("main.js", "const helper = require('pkg');\nhelper();"),
        (
            "node_modules/pkg/package.json",
            r#"{"name": "pkg", "exports": {".": {"require": "./cjs.js", "import": "./esm.js"}}}"#,
        ),
        (
            "node_modules/pkg/cjs.js",
            "function helper() {}\nmodule.exports = helper;",
        ),
        ("node_modules/pkg/esm.js", "export function helper() {}"),
    ]);
    assert!(
        program
            .symbols
            .iter()
            .any(|s| s.name == "helper" && s.file.ends_with("cjs.js")),
        "the require condition should be picked for a CommonJS-style import"
    );
}

#[test]
fn node_modules_subpath_import_resolves_with_extension_guessing() {
    let program = analyze_js(&[
        (
            "main.js",
            "const debounce = require('lodash/debounce');\ndebounce();",
        ),
        (
            "node_modules/lodash/debounce.js",
            "function debounce() {}\nmodule.exports = debounce;",
        ),
    ]);
    assert!(
        program
            .symbols
            .iter()
            .any(|s| s.name == "debounce" && s.file.ends_with("lodash/debounce.js")),
        "require('lodash/debounce') should resolve to node_modules/lodash/debounce.js"
    );
}

#[test]
fn node_modules_scoped_package_resolves() {
    let program = analyze_js(&[
        ("main.js", "const core = require('@scope/pkg');\ncore();"),
        (
            "node_modules/@scope/pkg/package.json",
            r#"{"name": "@scope/pkg", "main": "index.js"}"#,
        ),
        (
            "node_modules/@scope/pkg/index.js",
            "function core() {}\nmodule.exports = core;",
        ),
    ]);
    assert!(
        program
            .symbols
            .iter()
            .any(|s| s.name == "core" && s.kind == SymbolKind::Function),
        "scoped package @scope/pkg should resolve via its package.json main"
    );
}

// --- External dependency marking (don't flag as bloat) -----------------------

#[test]
fn node_modules_symbols_are_marked_external() {
    let program = analyze_js(&[
        ("main.js", "require('pkg');"),
        (
            "node_modules/pkg/package.json",
            r#"{"name": "pkg", "main": "index.js"}"#,
        ),
        ("node_modules/pkg/index.js", "function internal() {}"),
    ]);
    let main_module = program
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Module && s.file.ends_with("main.js"))
        .unwrap();
    assert!(
        !main_module.is_external,
        "the project's own file is not external"
    );

    let pkg_internal = program
        .symbols
        .iter()
        .find(|s| s.name == "internal")
        .unwrap();
    assert!(
        pkg_internal.is_external,
        "a symbol defined inside node_modules should be marked external"
    );
}

#[test]
fn used_node_modules_function_is_not_reported_as_bloat() {
    // Reproduces the exact shape of fixtures/javascript/js-node-modules: a package
    // whose export function name differs from the local require() binding name.
    let mut program = analyze_js(&[
        (
            "main.js",
            "const helper = require('test-package');\nhelper();",
        ),
        (
            "node_modules/test-package/package.json",
            r#"{"name": "test-package", "main": "index.js"}"#,
        ),
        (
            "node_modules/test-package/index.js",
            "function packageHelper() { console.log('hi'); }\nmodule.exports = packageHelper;",
        ),
    ]);
    let findings = bloat_findings(&mut program);
    assert!(
        findings.iter().all(|f| f.name != "packageHelper"),
        "packageHelper is third-party code and genuinely used; it must not be reported as bloat: {:?}",
        findings
    );
}

#[test]
fn unreachable_node_modules_internal_is_still_not_reported() {
    // Even code inside node_modules that this analyzer can't prove is reachable
    // (e.g. an internal helper the package's own entry file doesn't call) is not
    // actionable for the project being analyzed, so it must never surface either.
    let mut program = analyze_js(&[
        ("main.js", "require('pkg');"),
        (
            "node_modules/pkg/package.json",
            r#"{"name": "pkg", "main": "index.js"}"#,
        ),
        (
            "node_modules/pkg/index.js",
            "function neverCalledInternally() {}",
        ),
    ]);
    let findings = bloat_findings(&mut program);
    assert!(
        findings.is_empty(),
        "no node_modules symbol should ever be reported as bloat: {:?}",
        findings
    );
}

#[test]
fn unused_local_import_is_still_reported_as_bloat() {
    // Per product decision: recognizing a package as external does not exempt an
    // unused *import* of it in the consumer's own file -- that's still dead weight
    // in the project's own code.
    let mut program = analyze_js(&[(
        "main.js",
        "const _ = require('lodash');\nfunction main() { return 1; }\nmain();",
    )]);
    let findings = bloat_findings(&mut program);
    assert!(
        findings.iter().any(|f| f.name == "_"),
        "an unused import of a recognized external package should still be flagged: {:?}",
        findings
    );
}

#[test]
fn unresolved_export_name_does_not_cross_link_to_unrelated_local_symbol() {
    // main.js's own `helper` is unrelated to node_modules/test-package's export
    // (named packageHelper). Before the fix, the CommonJS default-import fallback
    // searched the whole project by name and could wrongly link the import to this
    // unrelated local function.
    let program = analyze_js(&[
        (
            "main.js",
            "function helper() { return 'local'; }\nhelper();\n\
             const pkg = require('test-package');\npkg();",
        ),
        (
            "node_modules/test-package/package.json",
            r#"{"name": "test-package", "main": "index.js"}"#,
        ),
        (
            "node_modules/test-package/index.js",
            "function packageHelper() {}\nmodule.exports = packageHelper;",
        ),
    ]);
    let pkg_import = program
        .symbols
        .iter()
        .find(|s| s.name == "pkg" && s.kind == SymbolKind::Import)
        .unwrap();
    let local_helper = program
        .symbols
        .iter()
        .find(|s| s.name == "helper" && s.kind == SymbolKind::Function)
        .unwrap();
    let wrongly_linked = program
        .references
        .iter()
        .any(|r| r.from == pkg_import.id && r.to == local_helper.id);
    assert!(
        !wrongly_linked,
        "the external package import must not be linked to an unrelated local symbol"
    );
}
