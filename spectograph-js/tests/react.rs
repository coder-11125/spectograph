//! Tests for React-specific support: JSX parsing, component detection, prop
//! tracking, and hooks usage.

use tempfile::tempdir;

use spectograph_js::react::{is_component_name, is_hook_name};
use spectograph_js::JavaScriptAnalyzer;

fn analyze_react(
    files: &[(&str, &str)],
) -> (spectograph_core::ir::Program, spectograph_js::ReactAnalysis) {
    let dir = tempdir().unwrap();
    for (name, content) in files {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }
    JavaScriptAnalyzer.analyze_react(dir.path()).unwrap()
}

// --- naming heuristics --------------------------------------------------------

#[test]
fn component_name_is_pascal_case() {
    assert!(is_component_name("Button"));
    assert!(is_component_name("App"));
    assert!(!is_component_name("div"));
    assert!(!is_component_name("button"));
    assert!(!is_component_name(""));
}

#[test]
fn hook_name_is_use_prefixed_pascal_case() {
    assert!(is_hook_name("useState"));
    assert!(is_hook_name("useMyCustomHook"));
    assert!(!is_hook_name("use")); // no name left after the prefix
    assert!(!is_hook_name("used")); // lowercase after "use"
    assert!(!is_hook_name("useless")); // lowercase after "use"
    assert!(!is_hook_name("utility"));
}

// --- JSX parsing / component detection ----------------------------------------

#[test]
fn function_component_returning_jsx_is_detected() {
    let (_program, react) = analyze_react(&[(
        "Button.jsx",
        "function Button({ label, onClick }) {\n    return <button onClick={onClick}>{label}</button>;\n}\n",
    )]);
    let button = react
        .components
        .iter()
        .find(|c| c.name == "Button")
        .expect("Button should be detected as a component");
    let mut props = button.declared_props.clone();
    props.sort();
    assert_eq!(props, vec!["label", "onClick"]);
}

#[test]
fn arrow_component_with_implicit_jsx_return_is_detected() {
    let (_program, react) = analyze_react(&[(
        "Badge.jsx",
        "const Badge = ({ count }) => <span>{count}</span>;\n",
    )]);
    assert!(
        react.components.iter().any(|c| c.name == "Badge"),
        "arrow function with implicit JSX body should be a detected component"
    );
}

#[test]
fn export_default_function_component_is_detected() {
    let (_program, react) = analyze_react(&[(
        "App.jsx",
        "export default function App() {\n    return <div>hi</div>;\n}\n",
    )]);
    assert!(
        react.components.iter().any(|c| c.name == "App"),
        "export default function should still be detected"
    );
}

#[test]
fn lowercase_function_is_not_a_component_even_if_it_returns_jsx() {
    let (_program, react) = analyze_react(&[(
        "helpers.jsx",
        "function renderRow() {\n    return <tr></tr>;\n}\n",
    )]);
    assert!(
        !react.components.iter().any(|c| c.name == "renderRow"),
        "lowercase-named function is a JSX helper, not a component by React convention"
    );
}

#[test]
fn plain_function_without_jsx_is_not_a_component() {
    let (_program, react) =
        analyze_react(&[("math.js", "function Add(a, b) {\n    return a + b;\n}\n")]);
    assert!(
        !react.components.iter().any(|c| c.name == "Add"),
        "PascalCase alone isn't enough -- it must also render JSX"
    );
}

#[test]
fn bare_props_parameter_yields_no_declared_props() {
    let (_program, react) = analyze_react(&[(
        "Card.jsx",
        "function Card(props) {\n    return <div>{props.title}</div>;\n}\n",
    )]);
    let card = react.components.iter().find(|c| c.name == "Card").unwrap();
    assert!(
        card.declared_props.is_empty(),
        "an un-destructured props param can't be statically enumerated"
    );
}

// --- JSX usage sites -----------------------------------------------------------

#[test]
fn jsx_usage_captures_passed_props() {
    let (_program, react) = analyze_react(&[(
        "App.jsx",
        r#"function Button({ label }) { return <button>{label}</button>; }
function App() {
    return <Button label="Click me" onClick={handleClick} disabled />;
}
"#,
    )]);
    let usage = react
        .jsx_usages
        .iter()
        .find(|u| u.component_name == "Button")
        .expect("Button usage should be captured");
    let mut props = usage.passed_props.clone();
    props.sort();
    assert_eq!(props, vec!["disabled", "label", "onClick"]);
}

#[test]
fn intrinsic_dom_tags_are_not_captured_as_component_usage() {
    let (_program, react) = analyze_react(&[(
        "App.jsx",
        "function App() {\n    return <div className=\"app\"><span>hi</span></div>;\n}\n",
    )]);
    assert!(
        react.jsx_usages.is_empty(),
        "lowercase intrinsic tags (div, span) are not component usages"
    );
}

// --- Component/JSX resolution feeds the regular reference graph ---------------

#[test]
fn jsx_component_reference_resolves_like_any_call() {
    let (program, _react) = analyze_react(&[(
        "App.jsx",
        r#"function Button() { return <button/>; }
function App() {
    return <Button />;
}
"#,
    )]);
    let button_id = program
        .symbols
        .iter()
        .find(|s| s.name == "Button")
        .unwrap()
        .id;
    let app_id = program.symbols.iter().find(|s| s.name == "App").unwrap().id;
    assert!(
        program
            .references
            .iter()
            .any(|r| r.from == app_id && r.to == button_id),
        "JSX tag usage should resolve through the normal identifier reference graph"
    );
}

// --- Hooks usage -----------------------------------------------------------

#[test]
fn hook_calls_are_captured_and_attributed_to_the_enclosing_component() {
    let (program, react) = analyze_react(&[(
        "App.jsx",
        r#"import { useState, useEffect } from 'react';

function App() {
    const [count, setCount] = useState(0);
    useEffect(() => {
        console.log(count);
    }, [count]);
    return <div>{count}</div>;
}
"#,
    )]);
    let app_id = program.symbols.iter().find(|s| s.name == "App").unwrap().id;

    let hook_names: Vec<&str> = react
        .hook_calls
        .iter()
        .filter(|h| h.enclosing == app_id)
        .map(|h| h.hook_name.as_str())
        .collect();
    assert!(hook_names.contains(&"useState"));
    assert!(hook_names.contains(&"useEffect"));
}

#[test]
fn custom_hook_call_is_captured_too() {
    let (program, react) = analyze_react(&[(
        "App.jsx",
        r#"function App() {
    const auth = useAuth();
    return <div>{auth.user}</div>;
}
"#,
    )]);
    let app_id = program.symbols.iter().find(|s| s.name == "App").unwrap().id;
    assert!(react
        .hook_calls
        .iter()
        .any(|h| h.enclosing == app_id && h.hook_name == "useAuth"));
}

#[test]
fn built_in_hook_import_resolves_through_the_reference_graph() {
    // This was blocked by a pre-existing bug: ES6 `import { x } from 'y'` never
    // created a symbol at all (a grammar field-name mismatch), so hook imports
    // couldn't resolve to anything.
    let (program, _react) = analyze_react(&[(
        "App.jsx",
        "import { useState } from 'react';\n\nfunction App() {\n    const [x] = useState(0);\n    return <div>{x}</div>;\n}\n",
    )]);
    let use_state_import = program
        .symbols
        .iter()
        .find(|s| s.name == "useState" && s.kind == spectograph_core::ir::SymbolKind::Import)
        .expect("useState should be an Import symbol");
    let app_id = program.symbols.iter().find(|s| s.name == "App").unwrap().id;
    assert!(
        program
            .references
            .iter()
            .any(|r| r.from == app_id && r.to == use_state_import.id),
        "App should reference the useState import through the normal call graph"
    );
}

// --- Prop mismatch detection ---------------------------------------------------

#[test]
fn prop_mismatch_flags_undeclared_prop() {
    let (_program, react) = analyze_react(&[(
        "App.jsx",
        r#"function Button({ label }) { return <button>{label}</button>; }
function App() {
    return <Button label="hi" onClick={fn} />;
}
"#,
    )]);
    let mismatches =
        spectograph_js::react::find_prop_mismatches(&react.components, &react.jsx_usages);
    assert!(
        mismatches
            .iter()
            .any(|m| m.component_name == "Button" && m.prop == "onClick"),
        "passing onClick to a component that only declares `label` should be flagged: {:?}",
        mismatches
    );
    assert!(
        !mismatches.iter().any(|m| m.prop == "label"),
        "label IS declared, must not be flagged"
    );
}

#[test]
fn prop_mismatch_ignores_implicit_jsx_props() {
    let (_program, react) = analyze_react(&[(
        "App.jsx",
        r#"function Item({ text }) { return <li>{text}</li>; }
function App() {
    return <Item key="1" text="a" />;
}
"#,
    )]);
    let mismatches =
        spectograph_js::react::find_prop_mismatches(&react.components, &react.jsx_usages);
    assert!(
        mismatches.is_empty(),
        "`key` is React-implicit and must never be flagged as a mismatch: {:?}",
        mismatches
    );
}

#[test]
fn prop_mismatch_skips_components_with_bare_props_param() {
    let (_program, react) = analyze_react(&[(
        "App.jsx",
        r#"function Card(props) { return <div>{props.title}</div>; }
function App() {
    return <Card title="x" anythingElse="y" />;
}
"#,
    )]);
    let mismatches =
        spectograph_js::react::find_prop_mismatches(&react.components, &react.jsx_usages);
    assert!(
        mismatches.is_empty(),
        "a component taking a bare `props` object can't be statically checked, so it must not produce false positives: {:?}",
        mismatches
    );
}
