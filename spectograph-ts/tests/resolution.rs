//! Tests for TypeScript resolution (Part 1 scope): basic parsing, scope, function
//! calls. Type-level declarations, imports/exports resolution, decorators, and TSX
//! component detection are covered by later parts.

use tempfile::tempdir;

use spectograph_analyzer::Analyzer;
use spectograph_core::ir::SymbolKind;
use spectograph_ts::TypeScriptAnalyzer;

/// Helper to create a temp dir with TS files and analyze them.
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
fn typed_function_declaration_creates_symbol() {
    let program = analyze_ts(&[(
        "test.ts",
        "function add(a: number, b: number): number { return a + b; }",
    )]);
    let add = program.symbols.iter().find(|s| s.name == "add");
    assert!(add.is_some(), "add should be a symbol");
    assert_eq!(add.unwrap().kind, SymbolKind::Function);
}

#[test]
fn class_and_typed_method_create_symbols() {
    let program = analyze_ts(&[(
        "test.ts",
        r#"class Point {
    x: number;
    y: number;
    move(dx: number, dy: number): void {
        this.x += dx;
        this.y += dy;
    }
}"#,
    )]);
    let point = program.symbols.iter().find(|s| s.name == "Point");
    assert!(point.is_some(), "Point class should be a symbol");
    assert_eq!(point.unwrap().kind, SymbolKind::Class);
    assert!(
        program.symbols.iter().any(|s| s.name == "Point.move"),
        "Point.move method should be a symbol"
    );
}

#[test]
fn const_declaration_creates_symbol() {
    let program = analyze_ts(&[("test.ts", "const x: number = 1;")]);
    let x = program.symbols.iter().find(|s| s.name == "x");
    assert!(x.is_some(), "x should be a symbol");
    assert_eq!(x.unwrap().kind, SymbolKind::Const);
}

#[test]
fn function_call_creates_reference() {
    let program = analyze_ts(&[(
        "test.ts",
        r#"function foo(): void {}
function bar(): void {
    foo();
}"#,
    )]);
    let foo_id = program.symbols.iter().find(|s| s.name == "foo").unwrap().id;
    let has_ref = program
        .references
        .iter()
        .any(|r| r.to == foo_id && r.from != foo_id);
    assert!(has_ref, "bar should reference foo");
}

#[test]
fn exported_function_creates_symbol() {
    let program = analyze_ts(&[(
        "test.ts",
        "export function greet(name: string): string { return name; }",
    )]);
    let greet = program.symbols.iter().find(|s| s.name == "greet");
    assert!(greet.is_some(), "exported function should be a symbol");
    assert_eq!(greet.unwrap().kind, SymbolKind::Function);
}

#[test]
fn generics_do_not_break_parsing() {
    let program = analyze_ts(&[(
        "test.ts",
        r#"function identity<T>(value: T): T {
    return value;
}
class Box<T> {
    contents: T;
}"#,
    )]);
    assert!(program.symbols.iter().any(|s| s.name == "identity"));
    assert!(program.symbols.iter().any(|s| s.name == "Box"));
}

#[test]
fn generic_constraints_and_defaults_do_not_break_parsing() {
    let program = analyze_ts(&[(
        "test.ts",
        r#"interface Base { id: number; }
function identity<T extends Base = Base>(value: T): T {
    return value;
}"#,
    )]);
    assert!(program.symbols.iter().any(|s| s.name == "identity"));
}

#[test]
fn interface_and_type_alias_create_type_symbols() {
    let program = analyze_ts(&[(
        "test.ts",
        "interface Point { x: number; y: number; }\ntype Id = string | number;",
    )]);
    let point = program
        .symbols
        .iter()
        .find(|s| s.name == "Point")
        .expect("Point interface should be a symbol");
    assert_eq!(point.kind, SymbolKind::Type);
    let id = program
        .symbols
        .iter()
        .find(|s| s.name == "Id")
        .expect("Id type alias should be a symbol");
    assert_eq!(id.kind, SymbolKind::Type);
}

#[test]
fn enum_creates_type_symbol_and_members() {
    let program = analyze_ts(&[("test.ts", "enum Color { Red, Green = 5, Blue }")]);
    let color = program
        .symbols
        .iter()
        .find(|s| s.name == "Color")
        .expect("Color enum should be a symbol");
    assert_eq!(color.kind, SymbolKind::Type);
    for member in ["Color.Red", "Color.Green", "Color.Blue"] {
        assert!(
            program.symbols.iter().any(|s| s.name == member),
            "{member} should be a symbol"
        );
    }
}

#[test]
fn namespace_creates_symbol_and_its_members_are_still_collected() {
    let program = analyze_ts(&[(
        "test.ts",
        r#"namespace Utils {
    export function double(n: number): number {
        return n * 2;
    }
    const internalHelper = 1;
}"#,
    )]);
    assert!(
        program.symbols.iter().any(|s| s.name == "Utils"),
        "the namespace itself should be a symbol"
    );
    assert!(
        program.symbols.iter().any(|s| s.name == "double"),
        "a function declared inside a namespace should still be collected"
    );
    assert!(
        program.symbols.iter().any(|s| s.name == "internalHelper"),
        "a const declared inside a namespace should still be collected"
    );
}

#[test]
fn exported_namespace_is_still_detected() {
    // `export namespace Foo {}` is grammatically an `export_statement`, while bare
    // `namespace Foo {}` is wrapped in an `expression_statement` -- both must unwrap
    // to the same `internal_module` handling.
    let program = analyze_ts(&[(
        "test.ts",
        "export namespace Exported {\n    function inner(): void {}\n}",
    )]);
    assert!(program.symbols.iter().any(|s| s.name == "Exported"));
    assert!(program.symbols.iter().any(|s| s.name == "inner"));
}

#[test]
fn decorators_do_not_crash_parsing() {
    // Structured decorator capture (name + arguments, attached to the right
    // symbol) is covered by spectograph-ts/tests/decorators.rs; this just
    // guards that decorated code still parses fine via the plain `analyze()`
    // API, which doesn't return decorator data at all.
    let program = analyze_ts(&[(
        "test.ts",
        r#"function Injectable() {
    return (target: unknown) => target;
}

@Injectable()
class Service {
    run(): void {}
}"#,
    )]);
    assert!(
        program.symbols.iter().any(|s| s.name == "Service"),
        "a decorated class should still be parsed"
    );
}

#[test]
fn import_creates_symbol_and_resolves_to_target_module() {
    // Cross-file resolution landed in Part 3; see spectograph-ts/tests/imports.rs
    // for the full import/export resolution suite. This test just guards the
    // basics: parsing doesn't break, and the import isn't left dangling.
    let program = analyze_ts(&[
        (
            "main.ts",
            r#"import { helper } from './utils';
function main(): void {
    helper();
}"#,
        ),
        ("utils.ts", "export function helper(): void {}"),
    ]);
    let import_sym = program
        .symbols
        .iter()
        .find(|s| s.name == "helper" && s.kind == SymbolKind::Import)
        .expect("helper should be an Import symbol");
    let helper_fn = program
        .symbols
        .iter()
        .find(|s| s.name == "helper" && s.kind == SymbolKind::Function)
        .expect("utils.ts should export a helper function");
    assert!(
        program
            .references
            .iter()
            .any(|r| r.from == import_sym.id && r.to == helper_fn.id),
        "the import should resolve to utils.ts's helper"
    );
}

#[test]
fn tsx_file_with_jsx_does_not_crash_parsing() {
    // Full component/prop/hook detection is covered by
    // spectograph-ts/tests/react.rs; this just guards that a .tsx file parses
    // fine via the plain `analyze()` API, which doesn't return React data.
    let program = analyze_ts(&[(
        "App.tsx",
        r#"function App(): JSX.Element {
    return <div>hello</div>;
}"#,
    )]);
    assert!(
        program.symbols.iter().any(|s| s.name == "App"),
        "TSX file should parse via the TSX grammar dialect"
    );
}

#[test]
fn node_modules_symbols_are_marked_external() {
    let program = analyze_ts(&[
        ("main.ts", "function main(): void {}"),
        ("node_modules/pkg/index.ts", "function internal(): void {}"),
    ]);
    let main_sym = program.symbols.iter().find(|s| s.name == "main").unwrap();
    assert!(!main_sym.is_external);
    let internal_sym = program
        .symbols
        .iter()
        .find(|s| s.name == "internal")
        .unwrap();
    assert!(
        internal_sym.is_external,
        "a symbol defined inside node_modules should be marked external"
    );
}
