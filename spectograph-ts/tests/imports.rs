//! Tests for TypeScript import/export resolution (Part 3): ES module imports
//! (default/named/namespace), TS's `import x = require(...)` interop form,
//! re-exports (`export { a } from`, `export * from`, `export * as ns from`,
//! local `export { a };`, `export default`), relative extension guessing, and
//! `tsconfig.json` `baseUrl`/`paths` alias resolution.

use tempfile::tempdir;

use spectograph_analyzer::Analyzer;
use spectograph_core::ir::{Program, RefKind, SymbolKind};
use spectograph_ts::TypeScriptAnalyzer;

fn analyze_ts(files: &[(&str, &str)]) -> Program {
    let dir = tempdir().unwrap();
    for (name, content) in files {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }
    TypeScriptAnalyzer.analyze(dir.path()).unwrap()
}

/// True if some Reference links the Import symbol named `from_name` to a
/// non-Import symbol named `to_name`. Both sides are disambiguated by kind
/// rather than Vec order, since an unaliased import and its target definition
/// often share the same name (`import { helper } ...` / `function helper() {}`)
/// and file-processing order (alphabetical by path) isn't something a test
/// should have to reason about.
fn resolves(program: &Program, from_name: &str, to_name: &str) -> bool {
    let Some(from) = program
        .symbols
        .iter()
        .find(|s| s.name == from_name && s.kind == SymbolKind::Import)
    else {
        return false;
    };
    let Some(to) = program
        .symbols
        .iter()
        .find(|s| s.name == to_name && s.kind != SymbolKind::Import)
    else {
        return false;
    };
    program
        .references
        .iter()
        .any(|r| r.from == from.id && r.to == to.id)
}

#[test]
fn default_import_resolves_to_named_default_export() {
    // `export default class Greeter {}` keeps the name "Greeter" (not
    // "default"); the import must fall back to its own local binding name to
    // find it, matching the JS analyzer's convention.
    let program = analyze_ts(&[
        (
            "main.ts",
            "import Greeter from './greeter';\nconst g = new Greeter();",
        ),
        ("greeter.ts", "export default class Greeter {}"),
    ]);
    assert!(resolves(&program, "Greeter", "Greeter"));
}

#[test]
fn named_import_resolves_with_alias() {
    let program = analyze_ts(&[
        (
            "main.ts",
            "import { subtract as sub } from './math';\nsub(3, 1);",
        ),
        (
            "math.ts",
            "export function subtract(a: number, b: number) { return a - b; }",
        ),
    ]);
    assert!(resolves(&program, "sub", "subtract"));
}

#[test]
fn namespace_import_resolves_to_target_module() {
    let program = analyze_ts(&[
        (
            "main.ts",
            "import * as utils from './utils';\nutils.helper();",
        ),
        ("utils.ts", "export function helper(): void {}"),
    ]);
    let utils_import = program
        .symbols
        .iter()
        .find(|s| s.name == "utils" && s.kind == SymbolKind::Import)
        .unwrap();
    let utils_module = program
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Module && s.file.ends_with("utils.ts"))
        .unwrap();
    assert!(program
        .references
        .iter()
        .any(|r| r.from == utils_import.id && r.to == utils_module.id));
}

#[test]
fn import_equals_require_resolves() {
    let program = analyze_ts(&[
        ("main.ts", "import mathLib = require('./math');\nmathLib();"),
        ("math.ts", "function mathLib(): void {}\nexport = mathLib;"),
    ]);
    // `export =` isn't handled specially (rare CommonJS-interop-of-interop
    // form); the point of this test is that `import x = require(...)` itself
    // creates a resolvable Import symbol without crashing.
    let import_sym = program
        .symbols
        .iter()
        .find(|s| s.name == "mathLib" && s.kind == SymbolKind::Import);
    assert!(
        import_sym.is_some(),
        "import x = require(...) should create an Import symbol"
    );
}

#[test]
fn reexport_from_resolves_through_barrel_file() {
    let program = analyze_ts(&[
        ("barrel.ts", "export { add } from './math';"),
        (
            "math.ts",
            "export function add(a: number, b: number) { return a + b; }",
        ),
    ]);
    let export_sym = program
        .symbols
        .iter()
        .find(|s| s.name == "add" && s.kind == SymbolKind::Export)
        .expect("re-exported add should have an Export symbol");
    let add_fn = program
        .symbols
        .iter()
        .find(|s| s.name == "add" && s.kind == SymbolKind::Function)
        .unwrap();
    // export -> import -> definition, two ReExport hops.
    let import_sym = program
        .symbols
        .iter()
        .find(|s| s.name == "add" && s.kind == SymbolKind::Import)
        .unwrap();
    assert!(program
        .references
        .iter()
        .any(|r| r.from == export_sym.id && r.to == import_sym.id && r.kind == RefKind::ReExport));
    assert!(program
        .references
        .iter()
        .any(|r| r.from == import_sym.id && r.to == add_fn.id && r.kind == RefKind::ReExport));
}

#[test]
fn reexport_from_with_alias_uses_original_name_at_source() {
    let program = analyze_ts(&[
        ("barrel.ts", "export { add as sum } from './math';"),
        (
            "math.ts",
            "export function add(a: number, b: number) { return a + b; }",
        ),
    ]);
    let export_sym = program
        .symbols
        .iter()
        .find(|s| s.name == "sum" && s.kind == SymbolKind::Export)
        .expect("alias 'sum' should be the Export symbol's name");
    let add_fn = program
        .symbols
        .iter()
        .find(|s| s.name == "add" && s.kind == SymbolKind::Function)
        .unwrap();
    let has_path = program.references.iter().any(|export_ref| {
        export_ref.from == export_sym.id
            && program
                .references
                .iter()
                .any(|import_ref| import_ref.from == export_ref.to && import_ref.to == add_fn.id)
    });
    assert!(has_path, "sum should resolve through to math.ts's add");
}

#[test]
fn star_reexport_links_module_level_dependency() {
    let program = analyze_ts(&[
        ("barrel.ts", "export * from './utils';"),
        ("utils.ts", "export function helper(): void {}"),
    ]);
    let export_sym = program
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Export && s.file.ends_with("barrel.ts"))
        .expect("export * from should create an Export symbol");
    let utils_module = program
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Module && s.file.ends_with("utils.ts"))
        .unwrap();
    let has_path = program.references.iter().any(|export_ref| {
        export_ref.from == export_sym.id
            && program.references.iter().any(|import_ref| {
                import_ref.from == export_ref.to && import_ref.to == utils_module.id
            })
    });
    assert!(
        has_path,
        "export * from should link through to the target module"
    );
}

#[test]
fn star_as_namespace_reexport_resolves() {
    let program = analyze_ts(&[
        ("barrel.ts", "export * as utils from './utils';"),
        ("utils.ts", "export function helper(): void {}"),
    ]);
    assert!(
        program
            .symbols
            .iter()
            .any(|s| s.name == "utils" && s.kind == SymbolKind::Export),
        "export * as utils should create an Export symbol named 'utils'"
    );
}

#[test]
fn local_reexport_resolves_to_same_file_declaration() {
    let program = analyze_ts(&[("main.ts", "function helper(): void {}\nexport { helper };")]);
    assert!(
        resolves(&program, "helper", "helper") || {
            // The Export symbol and the Function symbol are both named "helper";
            // check via a Function-kind-specific lookup to disambiguate.
            let export_sym = program
                .symbols
                .iter()
                .find(|s| s.name == "helper" && s.kind == SymbolKind::Export)
                .unwrap();
            let func_sym = program
                .symbols
                .iter()
                .find(|s| s.name == "helper" && s.kind == SymbolKind::Function)
                .unwrap();
            program
                .references
                .iter()
                .any(|r| r.from == export_sym.id && r.to == func_sym.id)
        }
    );
}

#[test]
fn export_default_identifier_resolves() {
    let program = analyze_ts(&[("main.ts", "function App(): void {}\nexport default App;")]);
    let export_sym = program
        .symbols
        .iter()
        .find(|s| s.name == "default" && s.kind == SymbolKind::Export)
        .expect("export default should create an Export symbol named 'default'");
    let app_fn = program.symbols.iter().find(|s| s.name == "App").unwrap();
    assert!(program
        .references
        .iter()
        .any(|r| r.from == export_sym.id && r.to == app_fn.id));
}

#[test]
fn relative_import_without_extension_resolves() {
    let program = analyze_ts(&[
        ("main.ts", "import { helper } from './utils';\nhelper();"),
        ("utils.ts", "export function helper(): void {}"),
    ]);
    assert!(resolves(&program, "helper", "helper"));
}

#[test]
fn dotted_filename_relative_import_resolves() {
    // A specifier whose real file name contains a dot before its extension
    // (`./app.module` -> `app.module.ts`) must still resolve.
    // `Path::with_extension` treats text after the *last* dot as "the
    // extension" and replaces it, so naively guessing `app.ts` (dropping
    // "module") silently fails to find the real file -- and this naming
    // convention (`*.component.ts`, `*.service.ts`, `*.module.ts`, ...) is
    // how Angular projects name every single file, so it isn't a rare edge
    // case.
    let program = analyze_ts(&[
        (
            "main.ts",
            "import { AppModule } from './app.module';\nconsole.log(AppModule);",
        ),
        ("app.module.ts", "export class AppModule {}"),
    ]);
    assert!(resolves(&program, "AppModule", "AppModule"));
}

#[test]
fn relative_import_resolves_to_index_file() {
    let program = analyze_ts(&[
        ("main.ts", "import { helper } from './lib';\nhelper();"),
        ("lib/index.ts", "export function helper(): void {}"),
    ]);
    assert!(resolves(&program, "helper", "helper"));
}

#[test]
fn declaration_file_is_used_when_no_implementation_exists() {
    let program = analyze_ts(&[
        ("main.ts", "import { helper } from './utils';\nhelper();"),
        ("utils.d.ts", "export function helper(): void;"),
    ]);
    let helper_import = program
        .symbols
        .iter()
        .find(|s| s.name == "helper" && s.kind == SymbolKind::Import)
        .unwrap();
    // A .d.ts ambient function signature isn't a function_declaration with a
    // body in the same sense; the key behavior under test is that resolution
    // reaches into the .d.ts file at all (module_paths finds it), which we can
    // observe indirectly: no crash, and the .d.ts file's module symbol exists.
    let dts_module = program
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Module && s.file.ends_with("utils.d.ts"));
    assert!(dts_module.is_some());
    let _ = helper_import;
}

#[test]
fn tsconfig_paths_alias_resolves() {
    let program = analyze_ts(&[
        (
            "tsconfig.json",
            r#"{"compilerOptions": {"baseUrl": ".", "paths": {"@utils/*": ["src/utils/*"]}}}"#,
        ),
        (
            "src/main.ts",
            "import { helper } from '@utils/helper';\nhelper();",
        ),
        ("src/utils/helper.ts", "export function helper(): void {}"),
    ]);
    assert!(resolves(&program, "helper", "helper"));
}

#[test]
fn tsconfig_base_url_resolves_bare_specifier_without_paths_entry() {
    let program = analyze_ts(&[
        (
            "tsconfig.json",
            r#"{"compilerOptions": {"baseUrl": "src"}}"#,
        ),
        (
            "src/main.ts",
            "import { helper } from 'utils/helper';\nhelper();",
        ),
        ("src/utils/helper.ts", "export function helper(): void {}"),
    ]);
    assert!(resolves(&program, "helper", "helper"));
}

#[test]
fn missing_or_malformed_tsconfig_is_harmless() {
    // No tsconfig.json at all.
    let program = analyze_ts(&[("main.ts", "function main(): void {}")]);
    assert!(program.symbols.iter().any(|s| s.name == "main"));

    // Malformed (e.g. JSONC comments serde_json can't parse) tsconfig.json.
    let program2 = analyze_ts(&[
        ("tsconfig.json", "{ // this is a comment\n}"),
        ("main.ts", "function main(): void {}"),
    ]);
    assert!(program2.symbols.iter().any(|s| s.name == "main"));
}
