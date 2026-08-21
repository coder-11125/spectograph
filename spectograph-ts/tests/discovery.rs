//! Tests for `tsconfig.json` `include`/`exclude`-based file discovery (Part 8).
//!
//! Filtering only activates when `tsconfig.json` explicitly sets `include` or
//! `exclude`; a project with no `tsconfig.json` (or one that sets neither key)
//! parses every discovered `.ts`/`.tsx` file exactly as before -- covered
//! already by the rest of the suite (e.g. `resolution.rs`'s
//! `node_modules_symbols_are_marked_external`, which relies on `node_modules`
//! files still being parsed by default).

use tempfile::tempdir;

use spectograph_analyzer::Analyzer;
use spectograph_ts::TypeScriptAnalyzer;

fn analyze_ts(files: &[(&str, &str)]) -> spectograph_core::ir::Program {
    let dir = tempdir().unwrap();
    for (name, content) in files {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }
    TypeScriptAnalyzer.analyze(dir.path()).unwrap()
}

#[test]
fn explicit_exclude_pattern_removes_matching_files() {
    let program = analyze_ts(&[
        ("tsconfig.json", r#"{"exclude": ["legacy"]}"#),
        ("src/keep.ts", "export function keep(): void {}"),
        ("legacy/drop.ts", "export function drop(): void {}"),
    ]);
    assert!(program.symbols.iter().any(|s| s.name == "keep"));
    assert!(!program.symbols.iter().any(|s| s.name == "drop"));
}

#[test]
fn explicit_include_pattern_restricts_to_matching_files() {
    let program = analyze_ts(&[
        ("tsconfig.json", r#"{"include": ["src/**/*"]}"#),
        ("src/keep.ts", "export function keep(): void {}"),
        ("scripts/drop.ts", "export function drop(): void {}"),
    ]);
    assert!(program.symbols.iter().any(|s| s.name == "keep"));
    assert!(!program.symbols.iter().any(|s| s.name == "drop"));
}

#[test]
fn include_and_exclude_combine_exclude_wins_on_overlap() {
    let program = analyze_ts(&[
        (
            "tsconfig.json",
            r#"{"include": ["src/**/*"], "exclude": ["src/generated"]}"#,
        ),
        ("src/keep.ts", "export function keep(): void {}"),
        ("src/generated/drop.ts", "export function drop(): void {}"),
    ]);
    assert!(program.symbols.iter().any(|s| s.name == "keep"));
    assert!(!program.symbols.iter().any(|s| s.name == "drop"));
}

#[test]
fn no_tsconfig_keeps_default_discovery_including_node_modules() {
    let program = analyze_ts(&[
        ("a.ts", "export function a(): void {}"),
        ("node_modules/pkg/index.ts", "export function b(): void {}"),
    ]);
    assert!(program.symbols.iter().any(|s| s.name == "a"));
    assert!(program.symbols.iter().any(|s| s.name == "b"));
}

#[test]
fn explicit_node_modules_exclude_is_honored() {
    let program = analyze_ts(&[
        ("tsconfig.json", r#"{"exclude": ["node_modules"]}"#),
        ("a.ts", "export function a(): void {}"),
        ("node_modules/pkg/index.ts", "export function b(): void {}"),
    ]);
    assert!(program.symbols.iter().any(|s| s.name == "a"));
    assert!(!program.symbols.iter().any(|s| s.name == "b"));
}
