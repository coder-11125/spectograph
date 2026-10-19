//! `tsconfig.json` `include`/`exclude` follow `tsc`'s directory semantics: a pattern whose
//! last segment has no extension and no wildcard is a *directory* and covers everything
//! beneath it. Treating `"packages/*/src"` (the standard monorepo include) as a literal path
//! silently dropped every file of `vuejs/core` except a handful.

use tempfile::tempdir;

use spectograph_analyzer::Analyzer;
use spectograph_core::ir::SymbolKind;
use spectograph_ts::TypeScriptAnalyzer;

/// Module files (as paths relative to the project root) the analyzer chose to parse.
fn parsed_modules(files: &[(&str, &str)]) -> Vec<String> {
    let dir = tempdir().unwrap();
    for (name, content) in files {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }
    let program = TypeScriptAnalyzer.analyze(dir.path()).unwrap();
    let mut modules: Vec<String> = program
        .symbols
        .iter()
        .filter(|s| s.kind == SymbolKind::Module)
        .map(|s| {
            s.file
                .strip_prefix(dir.path())
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    modules.sort();
    modules
}

#[test]
fn wildcard_directory_patterns_include_everything_beneath_them() {
    let modules = parsed_modules(&[
        (
            "tsconfig.json",
            r#"{ "include": ["packages/*/src", "scripts/*"], "exclude": ["packages/*/src/gen"] }"#,
        ),
        ("packages/a/src/index.ts", "export const a = 1;"),
        ("packages/a/src/deep/nested/util.ts", "export const u = 1;"),
        ("packages/a/src/gen/generated.ts", "export const g = 1;"),
        ("packages/a/dist/out.ts", "export const o = 1;"),
        ("scripts/build.ts", "export const b = 1;"),
        ("scripts/sub/more.ts", "export const m = 1;"),
    ]);
    assert_eq!(
        modules,
        vec![
            "packages/a/src/deep/nested/util.ts",
            "packages/a/src/index.ts",
            // `scripts/*` names files directly in scripts/, not its subdirectories.
            "scripts/build.ts",
        ]
    );
}

#[test]
fn a_bare_directory_name_includes_the_whole_directory() {
    let modules = parsed_modules(&[
        ("tsconfig.json", r#"{ "include": ["src"] }"#),
        ("src/a.ts", "export const a = 1;"),
        ("src/sub/b.ts", "export const b = 1;"),
        ("other/c.ts", "export const c = 1;"),
    ]);
    assert_eq!(modules, vec!["src/a.ts", "src/sub/b.ts"]);
}

#[test]
fn explicit_file_and_extension_globs_still_work() {
    let modules = parsed_modules(&[
        (
            "tsconfig.json",
            r#"{ "include": ["types/global.d.ts", "lib/**/*.ts"] }"#,
        ),
        ("types/global.d.ts", "declare const X: number;"),
        ("types/other.d.ts", "declare const Y: number;"),
        ("lib/a/b.ts", "export const b = 1;"),
        ("lib/c.js", ""),
    ]);
    assert_eq!(modules, vec!["lib/a/b.ts", "types/global.d.ts"]);
}
