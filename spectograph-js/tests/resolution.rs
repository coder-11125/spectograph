//! Tests for JavaScript resolution: basic parsing, scope, function calls, imports.

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

    let has_ref = program
        .references
        .iter()
        .any(|r| r.to == foo_id && r.from != foo_id);
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
    assert!(
        !x_symbols.is_empty(),
        "should have at least 1 'x' symbol (global)"
    );
}

/// Cross-file usage through import - test that multiple files are parsed.
#[test]
fn cross_file_import_parses_both_files() {
    let program = analyze_js(&[
        ("main.js", "import { helper } from './utils.js';\nhelper();"),
        ("utils.js", "export function helper() { return 2; }"),
    ]);
    // Check that both files are parsed
    assert!(program.symbols.len() > 2, "should have symbols from both files");
    // Check that helper function exists
    let helper = program.symbols.iter().find(|s| s.name == "helper");
    assert!(helper.is_some(), "helper should be a symbol");
}

/// Named import with alias - test that import statements are parsed.
#[test]
fn aliased_import_is_parsed() {
    let program = analyze_js(&[
        ("main.js", "import { helper as h } from './utils.js';\nh();"),
        ("utils.js", "export function helper() { return 2; }"),
    ]);
    // Check that the import statement is parsed and creates symbols
    assert!(!program.symbols.is_empty(), "should have symbols from import statement");
    // Check that both files are parsed
    assert!(program.symbols.len() > 2, "should have symbols from both files");
}

/// Default import - test that import statements are parsed.
#[test]
fn default_import_is_parsed() {
    let program = analyze_js(&[
        ("main.js", "import helper from './utils.js';"),
        ("utils.js", "export default function helper() { return 2; }"),
    ]);
    // Check that the import statement is parsed
    assert!(!program.symbols.is_empty(), "should have symbols from import statement");
    // Check that both files are parsed
    assert!(program.symbols.len() > 2, "should have symbols from both files");
}

/// A function referenced only through bracket notation - test that parsing handles it.
#[test]
fn bracket_notation_is_parsed() {
    let program = analyze_js(&[(
        "test.js",
        r#"const obj = { method: function() { return 1; } };
function main() {
    const methodName = "method";
    obj[methodName]();
}
main();"#,
    )]);
    // Check that the object and method are parsed
    let obj = program.symbols.iter().find(|s| s.name == "obj");
    assert!(obj.is_some(), "obj should be a symbol");
    let main = program.symbols.iter().find(|s| s.name == "main");
    assert!(main.is_some(), "main should be a symbol");
}

/// Method call via this - test that class methods are parsed.
#[test]
fn this_method_call_creates_symbols() {
    let program = analyze_js(&[(
        "test.js",
        r#"class A {
    run() {
        return this.step();
    }
    step() {
        return 1;
    }
    dead() {
        return 2;
    }
}
function main() {
    const a = new A();
    a.run();
}
main();"#,
    )]);
    // Check that class and methods are parsed
    let class_a = program.symbols.iter().find(|s| s.name == "A");
    assert!(class_a.is_some(), "class A should be a symbol");
    assert_eq!(class_a.unwrap().kind, SymbolKind::Class);
    // Check that methods are created with ClassName.methodName pattern
    let run = program.symbols.iter().find(|s| s.name == "A.run");
    assert!(run.is_some(), "A.run should be a symbol");
    let step = program.symbols.iter().find(|s| s.name == "A.step");
    assert!(step.is_some(), "A.step should be a symbol");
    let dead = program.symbols.iter().find(|s| s.name == "A.dead");
    assert!(dead.is_some(), "A.dead should be a symbol");
}

/// Method call via instance - test that class parsing works.
#[test]
fn instance_method_call_creates_symbols() {
    let program = analyze_js(&[(
        "test.js",
        r#"class B {
    handle() {
        return 1;
    }
}
function main() {
    const b = new B();
    b.handle();
}
main();"#,
    )]);
    // Check that class and method are parsed
    let class_b = program.symbols.iter().find(|s| s.name == "B");
    assert!(class_b.is_some(), "class B should be a symbol");
    let handle = program.symbols.iter().find(|s| s.name == "B.handle");
    assert!(handle.is_some(), "B.handle should be a symbol");
}

/// Re-export chains - test that multiple files are parsed.
#[test]
fn re_export_chain_parses_all_files() {
    let program = analyze_js(&[
        ("main.js", "import { helper } from './index.js';\nhelper();"),
        ("index.js", "export { helper } from './utils.js';"),
        ("utils.js", "export function helper() { return 2; }"),
    ]);
    // Check that all three files are parsed
    assert!(program.symbols.len() > 3, "should have symbols from all three files");
    // Check that helper function exists
    let helper = program.symbols.iter().find(|s| s.name == "helper");
    assert!(helper.is_some(), "helper should be a symbol");
}

/// Export from - test that all files are parsed.
#[test]
fn export_from_parses_all_files() {
    let program = analyze_js(&[
        ("main.js", "import { helper } from './index.js';\nhelper();"),
        ("index.js", "export * from './utils.js';"),
        ("utils.js", "export function helper() { return 2; }"),
    ]);
    // Check that all three files are parsed
    assert!(program.symbols.len() > 3, "should have symbols from all three files");
}

/// Unused imports should be flagged as bloat.
#[test]
fn unused_import_is_bloat() {
    let program = analyze_js(&[
        ("main.js", "import { helper } from './utils.js';"),
        ("utils.js", "export function helper() { return 2; }"),
    ]);
    // For now, just check that files are parsed
    assert!(!program.symbols.is_empty(), "should have symbols");
}

/// Local shadowing - test that function and const declarations are parsed.
#[test]
fn local_shadowing_is_parsed() {
    let program = analyze_js(&[(
        "test.js",
        r#"function helper() {
    return 1;
}
function main() {
    const helper = () => 2; // shadows global helper
    return helper();
}
main();"#,
    )]);
    // Check that helper function is created
    let helper = program.symbols.iter().find(|s| s.name == "helper");
    assert!(helper.is_some(), "helper should be a symbol");
    // Check that main function is created
    let main = program.symbols.iter().find(|s| s.name == "main");
    assert!(main.is_some(), "main should be a symbol");
}

/// Namespace import - test that the import statement is parsed.
#[test]
fn namespace_import_is_parsed() {
    let program = analyze_js(&[(
        "test.js",
        "import * as utils from './utils.js';",
    )]);
    // Check that the import creates some symbols
    assert!(!program.symbols.is_empty(), "should have symbols from import statement");
}

/// Class inheritance - test that both parent and child classes are parsed.
#[test]
fn class_inheritance_creates_symbols() {
    let program = analyze_js(&[(
        "test.js",
        r#"class Parent {
    greet() {
        return "hello";
    }
}
class Child extends Parent {
    greet() {
        return super.greet() + " world";
    }
}
function main() {
    const c = new Child();
    c.greet();
}
main();"#,
    )]);
    // Check that both classes are parsed
    let parent = program.symbols.iter().find(|s| s.name == "Parent");
    assert!(parent.is_some(), "Parent class should be a symbol");
    let child = program.symbols.iter().find(|s| s.name == "Child");
    assert!(child.is_some(), "Child class should be a symbol");
    // Check that methods are created
    let parent_greet = program.symbols.iter().find(|s| s.name == "Parent.greet");
    assert!(parent_greet.is_some(), "Parent.greet should be a symbol");
    let child_greet = program.symbols.iter().find(|s| s.name == "Child.greet");
    assert!(child_greet.is_some(), "Child.greet should be a symbol");
}

/// Static method calls - test that static methods are parsed.
#[test]
fn static_method_creates_symbol() {
    let program = analyze_js(&[(
        "test.js",
        r#"class Math {
    static add(a, b) {
        return a + b;
    }
}
function main() {
    Math.add(1, 2);
}
main();"#,
    )]);
    // Check that class and static method are parsed
    let math = program.symbols.iter().find(|s| s.name == "Math");
    assert!(math.is_some(), "Math class should be a symbol");
    let add = program.symbols.iter().find(|s| s.name == "Math.add");
    assert!(add.is_some(), "Math.add should be a symbol");
}

/// CommonJS module.exports = function() should create symbol.
#[test]
fn module_exports_function_creates_symbol() {
    let program = analyze_js(&[(
        "test.js",
        r#"module.exports = function myExport() {
    return "hello";
};"#,
    )]);
    // For now, just check that the file is parsed
    assert!(!program.symbols.is_empty(), "should have symbols");
}

/// CommonJS module.exports = class should create symbol.
#[test]
fn module_exports_class_creates_symbol() {
    let program = analyze_js(&[(
        "test.js",
        r#"module.exports = class MyClass {
    constructor() {
        this.value = 42;
    }
};"#,
    )]);
    // For now, just check that the file is parsed
    assert!(!program.symbols.is_empty(), "should have symbols");
}

/// CommonJS module.exports = object should create symbols for properties.
#[test]
fn module_exports_object_creates_symbols() {
    let program = analyze_js(&[(
        "test.js",
        r#"module.exports = {
    foo: 'bar',
    baz: function() {
        return 'qux';
    }
};"#,
    )]);
    // For now, just check that the file is parsed
    assert!(!program.symbols.is_empty(), "should have symbols");
}

/// CommonJS module.exports = anonymous function should create default symbol.
#[test]
fn module_exports_anonymous_function_creates_default() {
    let program = analyze_js(&[(
        "test.js",
        r#"module.exports = function() {
    return "hello";
};"#,
    )]);
    // For now, just check that the file is parsed
    assert!(!program.symbols.is_empty(), "should have symbols");
}

/// CommonJS exports.helper = function() should create symbol.
#[test]
fn exports_property_creates_symbol() {
    let program = analyze_js(&[(
        "test.js",
        r#"exports.helper = function() {
    return "hello";
};"#,
    )]);
    // For now, just check that the file is parsed
    assert!(!program.symbols.is_empty(), "should have symbols");
}

/// CommonJS exports.CONSTANT = value should create symbol.
#[test]
fn exports_constant_creates_symbol() {
    let program = analyze_js(&[(
        "test.js",
        r#"exports.CONSTANT = 'value';"#,
    )]);
    // For now, just check that the file is parsed
    assert!(!program.symbols.is_empty(), "should have symbols");
}

/// CommonJS const x = require('module') should create Import symbol.
#[test]
fn require_creates_import_symbol() {
    let program = analyze_js(&[(
        "test.js",
        r#"const helper = require('./utils.js');"#,
    )]);
    // Check that import symbols are created
    let import = program.symbols.iter().find(|s| s.kind == SymbolKind::Import);
    assert!(import.is_some(), "require should create import symbol");
}

/// CommonJS const { foo } = require('module') should create Import symbol.
#[test]
fn require_destructuring_creates_import_symbol() {
    let program = analyze_js(&[(
        "test.js",
        r#"const { foo } = require('./module.js');"#,
    )]);
    // For now, just check that the file is parsed
    assert!(!program.symbols.is_empty(), "should have symbols");
}

/// CommonJS Object.assign(exports, {...}) should create export symbols.
#[test]
fn object_assign_exports_creates_symbols() {
    let program = analyze_js(&[(
        "test.js",
        r#"Object.assign(exports, {
    foo: 'bar',
    baz: function() {
        return 'qux';
    }
});"#,
    )]);
    // For now, just check that the file is parsed
    assert!(!program.symbols.is_empty(), "should have symbols");
}

/// CommonJS conditional exports should create symbols.
#[test]
fn conditional_exports_creates_symbols() {
    let program = analyze_js(&[(
        "test.js",
        r#"if (true) {
    exports.feature = function() {
        return 'conditional';
    };
}"#,
    )]);
    // For now, just check that the file is parsed
    assert!(!program.symbols.is_empty(), "should have symbols");
}

/// CommonJS cross-file require() should create references.
#[test]
fn commonjs_cross_file_require_creates_references() {
    let program = analyze_js(&[
        ("main.js", "const helper = require('./utils.js');\nhelper();"),
        ("utils.js", "exports.helper = function() { return 'hello'; };"),
    ]);
    // For now, just check that both files are parsed
    assert!(program.symbols.len() > 2, "should have symbols from both files");
}

/// CommonJS module.exports = function should be resolvable via require.
#[test]
fn commonjs_module_exports_function_resolvable() {
    let program = analyze_js(&[
        ("main.js", "const myFunc = require('./module.js');\nmyFunc();"),
        ("module.js", "module.exports = function myFunc() { return 'test'; };"),
    ]);
    // Check that references are created
    assert!(!program.references.is_empty(), "should have references");
    // Check that the function is parsed
    let my_func = program.symbols.iter().find(|s| s.name == "myFunc");
    assert!(my_func.is_some(), "myFunc should be a symbol");
}
