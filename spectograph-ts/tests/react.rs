//! Tests for TSX/React support (Part 6): component detection, prop tracking
//! (both JS-style destructured props and TS-idiomatic typed props via an
//! interface/object-type alias), hooks usage, and JSX resolving through the
//! normal reference graph. Reuses spectograph_js::react's types directly
//! (re-exported as spectograph_ts::react) rather than duplicating them.

use tempfile::tempdir;

use spectograph_ts::react::find_prop_mismatches;
use spectograph_ts::{ReactAnalysis, TypeScriptAnalyzer};

fn analyze_react(files: &[(&str, &str)]) -> (spectograph_core::ir::Program, ReactAnalysis) {
    let dir = tempdir().unwrap();
    for (name, content) in files {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }
    TypeScriptAnalyzer.analyze_react(dir.path()).unwrap()
}

#[test]
fn tsx_grammar_parses_real_jsx() {
    let (program, react) = analyze_react(&[(
        "App.tsx",
        "function App(): JSX.Element {\n    return <div>hello</div>;\n}\n",
    )]);
    assert!(program.symbols.iter().any(|s| s.name == "App"));
    assert!(react.components.iter().any(|c| c.name == "App"));
}

#[test]
fn destructured_props_component_is_detected() {
    let (_program, react) = analyze_react(&[(
        "Button.tsx",
        "function Button({ label, onClick }: { label: string; onClick: () => void }): JSX.Element {\n    return <button onClick={onClick}>{label}</button>;\n}\n",
    )]);
    let button = react
        .components
        .iter()
        .find(|c| c.name == "Button")
        .expect("Button should be detected");
    let mut props = button.declared_props.clone();
    props.sort();
    assert_eq!(props, vec!["label", "onClick"]);
}

#[test]
fn typed_props_via_interface_resolve_declared_props() {
    // The TS-idiomatic style: a separate `interface Props` instead of
    // destructuring -- not supported by the JS analyzer at all.
    let (_program, react) = analyze_react(&[(
        "Card.tsx",
        r#"interface CardProps {
    title: string;
    subtitle: string;
}

function Card(props: CardProps): JSX.Element {
    return <div>{props.title}</div>;
}
"#,
    )]);
    let card = react.components.iter().find(|c| c.name == "Card").unwrap();
    let mut props = card.declared_props.clone();
    props.sort();
    assert_eq!(props, vec!["subtitle", "title"]);
}

#[test]
fn typed_props_via_type_alias_object_resolve_declared_props() {
    let (_program, react) = analyze_react(&[(
        "Card.tsx",
        r#"type CardProps = {
    title: string;
};

function Card(props: CardProps): JSX.Element {
    return <div>{props.title}</div>;
}
"#,
    )]);
    let card = react.components.iter().find(|c| c.name == "Card").unwrap();
    assert_eq!(card.declared_props, vec!["title".to_string()]);
}

#[test]
fn arrow_component_with_typed_props_is_detected() {
    let (_program, react) = analyze_react(&[(
        "Badge.tsx",
        r#"interface BadgeProps {
    count: number;
}

const Badge = (props: BadgeProps): JSX.Element => <span>{props.count}</span>;
"#,
    )]);
    let badge = react.components.iter().find(|c| c.name == "Badge").unwrap();
    assert_eq!(badge.declared_props, vec!["count".to_string()]);
}

#[test]
fn untyped_bare_props_param_yields_no_declared_props() {
    let (_program, react) = analyze_react(&[(
        "Card.tsx",
        "function Card(props): JSX.Element {\n    return <div>{props.title}</div>;\n}\n",
    )]);
    let card = react.components.iter().find(|c| c.name == "Card").unwrap();
    assert!(card.declared_props.is_empty());
}

#[test]
fn lowercase_function_returning_jsx_is_not_a_component() {
    let (_program, react) = analyze_react(&[(
        "helpers.tsx",
        "function renderRow(): JSX.Element {\n    return <tr></tr>;\n}\n",
    )]);
    assert!(!react.components.iter().any(|c| c.name == "renderRow"));
}

#[test]
fn jsx_usage_captures_passed_props() {
    let (_program, react) = analyze_react(&[(
        "App.tsx",
        r#"interface ButtonProps { label: string; }
function Button({ label }: ButtonProps): JSX.Element { return <button>{label}</button>; }
function App(): JSX.Element {
    return <Button label="hi" onClick={fn} disabled />;
}
"#,
    )]);
    let usage = react
        .jsx_usages
        .iter()
        .find(|u| u.component_name == "Button")
        .unwrap();
    let mut props = usage.passed_props.clone();
    props.sort();
    assert_eq!(props, vec!["disabled", "label", "onClick"]);
}

#[test]
fn intrinsic_dom_tags_are_not_captured_as_component_usage() {
    let (_program, react) = analyze_react(&[(
        "App.tsx",
        "function App(): JSX.Element {\n    return <div className=\"app\"><span>hi</span></div>;\n}\n",
    )]);
    assert!(react.jsx_usages.is_empty());
}

#[test]
fn jsx_component_reference_resolves_through_normal_reference_graph() {
    let (program, _react) = analyze_react(&[(
        "App.tsx",
        r#"function Button(): JSX.Element { return <button/>; }
function App(): JSX.Element {
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
    assert!(program
        .references
        .iter()
        .any(|r| r.from == app_id && r.to == button_id));
}

#[test]
fn hook_calls_are_attributed_to_the_enclosing_component() {
    let (program, react) = analyze_react(&[(
        "App.tsx",
        r#"import { useState, useEffect } from 'react';

function App(): JSX.Element {
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
fn custom_hook_call_is_captured() {
    let (program, react) = analyze_react(&[(
        "App.tsx",
        "function App(): JSX.Element {\n    const auth = useAuth();\n    return <div>{auth.user}</div>;\n}\n",
    )]);
    let app_id = program.symbols.iter().find(|s| s.name == "App").unwrap().id;
    assert!(react
        .hook_calls
        .iter()
        .any(|h| h.enclosing == app_id && h.hook_name == "useAuth"));
}

#[test]
fn prop_mismatch_flags_prop_not_in_typed_interface() {
    let (_program, react) = analyze_react(&[(
        "App.tsx",
        r#"interface CardProps { title: string; }
function Card(props: CardProps): JSX.Element { return <div>{props.title}</div>; }
function App(): JSX.Element {
    return <Card title="hi" extra="oops" />;
}
"#,
    )]);
    let mismatches = find_prop_mismatches(&react.components, &react.jsx_usages);
    assert!(
        mismatches
            .iter()
            .any(|m| m.component_name == "Card" && m.prop == "extra"),
        "passing `extra` to a component whose interface only declares `title` \
         should be flagged: {:?}",
        mismatches
    );
    assert!(!mismatches.iter().any(|m| m.prop == "title"));
}

#[test]
fn prop_mismatch_skips_component_with_bare_untyped_props() {
    let (_program, react) = analyze_react(&[(
        "App.tsx",
        r#"function Card(props): JSX.Element { return <div>{props.title}</div>; }
function App(): JSX.Element {
    return <Card anything="x" />;
}
"#,
    )]);
    let mismatches = find_prop_mismatches(&react.components, &react.jsx_usages);
    assert!(
        mismatches.is_empty(),
        "a component with an unenumerable props type must not produce false positives: {:?}",
        mismatches
    );
}
