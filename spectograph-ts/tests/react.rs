//! Tests for TSX/React support (Part 6): component detection, prop tracking
//! (both JS-style destructured props and TS-idiomatic typed props via an
//! interface/object-type alias), hooks usage, and JSX resolving through the
//! normal reference graph. Reuses spectograph_js::react's types directly
//! (re-exported as spectograph_ts::react) rather than duplicating them.

use tempfile::tempdir;

use spectograph_analyzer::Analyzer;
use spectograph_core::analysis;
use spectograph_core::cost::{CostModel, CostSource};
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

fn analyze_ts(files: &[(&str, &str)]) -> spectograph_core::ir::Program {
    let dir = tempdir().unwrap();
    for (name, content) in files {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }
    TypeScriptAnalyzer.analyze(dir.path()).unwrap()
}

fn bloat_findings(program: &mut spectograph_core::ir::Program) -> Vec<analysis::BloatFinding> {
    let model = CostModel::build(program, CostSource::Estimated);
    analysis::analyze(program, &model).findings
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

// --- Class components -----------------------------------------------------------

#[test]
fn class_extending_react_component_is_detected() {
    let (_program, react) = analyze_react(&[(
        "App.tsx",
        "class App extends React.Component {\n    render(): JSX.Element {\n        return <div>hi</div>;\n    }\n}\n",
    )]);
    assert!(
        react.components.iter().any(|c| c.name == "App"),
        "a class extending React.Component with a JSX-returning render() should be detected"
    );
}

#[test]
fn class_extending_bare_component_import_is_detected() {
    let (_program, react) = analyze_react(&[(
        "App.tsx",
        "class App extends Component {\n    render(): JSX.Element {\n        return <div>hi</div>;\n    }\n}\n",
    )]);
    assert!(react.components.iter().any(|c| c.name == "App"));
}

#[test]
fn class_extending_pure_component_with_generic_props_is_detected() {
    // `extends React.Component<Props>` -- the generic type argument on the
    // heritage clause shouldn't confuse heritage-target detection.
    let (_program, react) = analyze_react(&[(
        "App.tsx",
        "interface AppProps {}\nclass App extends React.PureComponent<AppProps> {\n    render(): JSX.Element {\n        return <div>hi</div>;\n    }\n}\n",
    )]);
    assert!(react.components.iter().any(|c| c.name == "App"));
}

#[test]
fn class_component_without_render_is_not_a_component() {
    let (_program, react) = analyze_react(&[(
        "App.tsx",
        "class App extends React.Component {\n    helper(): number {\n        return 1;\n    }\n}\n",
    )]);
    assert!(!react.components.iter().any(|c| c.name == "App"));
}

#[test]
fn class_not_extending_component_is_not_a_component() {
    let (_program, react) = analyze_react(&[(
        "App.tsx",
        "class App {\n    render(): JSX.Element {\n        return <div>hi</div>;\n    }\n}\n",
    )]);
    assert!(
        !react.components.iter().any(|c| c.name == "App"),
        "a render() method alone isn't enough -- it must extend Component/PureComponent"
    );
}

#[test]
fn class_component_declared_props_are_empty() {
    let (_program, react) = analyze_react(&[(
        "App.tsx",
        "class App extends React.Component {\n    render(): JSX.Element {\n        return <div>{this.props.title}</div>;\n    }\n}\n",
    )]);
    let app = react.components.iter().find(|c| c.name == "App").unwrap();
    assert!(app.declared_props.is_empty());
}

#[test]
fn jsx_inside_class_render_method_is_captured_as_usage() {
    let (_program, react) = analyze_react(&[(
        "App.tsx",
        r#"function Child(): JSX.Element { return <span/>; }
class App extends React.Component {
    render(): JSX.Element {
        return <div><Child/></div>;
    }
}
"#,
    )]);
    assert!(react.jsx_usages.iter().any(|u| u.component_name == "Child"));
}

#[test]
fn class_component_lifecycle_methods_are_not_falsely_flagged_as_bloat() {
    let mut program = analyze_ts(&[
        (
            "App.tsx",
            r#"import Child from './Child';

class App extends React.Component {
    render(): JSX.Element {
        return <div><Child/></div>;
    }
    componentDidMount(): void {
        console.log('mounted');
    }
}

export default App;
"#,
        ),
        (
            "Child.tsx",
            "function Child(): JSX.Element { return <span/>; }\nexport default Child;\n",
        ),
    ]);
    let findings = bloat_findings(&mut program);
    let flagged: Vec<&str> = findings.iter().map(|f| f.name.as_str()).collect();
    assert!(
        !flagged.contains(&"App.render"),
        "render() is always called by React and must not be flagged: {:?}",
        flagged
    );
    assert!(
        !flagged.contains(&"App.componentDidMount"),
        "componentDidMount() is a React lifecycle hook and must not be flagged: {:?}",
        flagged
    );
    assert!(
        !flagged.contains(&"Child"),
        "Child is rendered from App's render() and must not be flagged: {:?}",
        flagged
    );
}

#[test]
fn class_component_non_lifecycle_method_is_still_flagged_when_unused() {
    let mut program = analyze_ts(&[(
        "App.tsx",
        r#"class App extends React.Component {
    render(): JSX.Element {
        return <div>hi</div>;
    }
    unusedHelper(): number {
        return 1;
    }
}

export default App;
"#,
    )]);
    let findings = bloat_findings(&mut program);
    assert!(
        findings.iter().any(|f| f.name == "App.unusedHelper"),
        "a non-lifecycle method that's genuinely unused should still be flagged: {:?}",
        findings.iter().map(|f| &f.name).collect::<Vec<_>>()
    );
}

#[test]
fn non_component_class_lifecycle_named_method_is_not_protected() {
    let mut program = analyze_ts(&[(
        "App.tsx",
        "class NotAComponent {\n    render(): number {\n        return 1;\n    }\n}\n\nexport default NotAComponent;\n",
    )]);
    let findings = bloat_findings(&mut program);
    assert!(
        findings.iter().any(|f| f.name == "NotAComponent.render"),
        "render() on a non-Component class is an ordinary method and should be flagged if unused: {:?}",
        findings.iter().map(|f| &f.name).collect::<Vec<_>>()
    );
}

// --- forwardRef / memo -wrapped components --------------------------------------

#[test]
fn bare_forward_ref_component_is_detected() {
    let (_program, react) = analyze_react(&[(
        "Input.tsx",
        "const Input = forwardRef((props: { value: string }, ref) => {\n    return <input ref={ref} value={props.value} />;\n});\n",
    )]);
    assert!(react.components.iter().any(|c| c.name == "Input"));
}

#[test]
fn namespaced_memo_component_with_typed_destructured_props_is_detected() {
    let (_program, react) = analyze_react(&[(
        "Badge.tsx",
        "const Badge = React.memo(({ count }: { count: number }) => <span>{count}</span>);\n",
    )]);
    let badge = react.components.iter().find(|c| c.name == "Badge").unwrap();
    assert_eq!(badge.declared_props, vec!["count".to_string()]);
}

#[test]
fn nested_memo_forward_ref_component_is_detected() {
    let (_program, react) = analyze_react(&[(
        "Button.tsx",
        "const Button = memo(forwardRef(function Button(props: { label: string }, ref) {\n    return <button ref={ref}>{props.label}</button>;\n}));\n",
    )]);
    assert!(react.components.iter().any(|c| c.name == "Button"));
}

#[test]
fn wrapper_call_to_unrelated_function_is_not_a_component() {
    let (_program, react) = analyze_react(&[(
        "helpers.tsx",
        "const Wrapped = debounce(() => {\n    return <div/>;\n});\n",
    )]);
    assert!(!react.components.iter().any(|c| c.name == "Wrapped"));
}

#[test]
fn hook_calls_inside_forward_ref_component_are_attributed_correctly() {
    let (program, react) = analyze_react(&[(
        "Input.tsx",
        r#"import { useState } from 'react';

const Input = forwardRef((props: {}, ref) => {
    const [value, setValue] = useState('');
    return <input ref={ref} value={value} />;
});
"#,
    )]);
    let input_id = program
        .symbols
        .iter()
        .find(|s| s.name == "Input")
        .unwrap()
        .id;
    assert!(react
        .hook_calls
        .iter()
        .any(|h| h.enclosing == input_id && h.hook_name == "useState"));
}

#[test]
fn jsx_usage_of_forward_ref_component_is_captured() {
    let (_program, react) = analyze_react(&[(
        "App.tsx",
        r#"const Input = forwardRef((props: {}, ref) => <input ref={ref} />);
function App(): JSX.Element {
    return <Input placeholder="hi" />;
}
"#,
    )]);
    assert!(react.jsx_usages.iter().any(|u| u.component_name == "Input"));
}

// --- Fragments and namespaced JSX tags ------------------------------------------

#[test]
fn component_returning_a_bare_fragment_is_still_detected() {
    // `<>...</>` parses as a plain `jsx_element` with an unnamed opening/
    // closing tag in this grammar -- there's no separate `jsx_fragment` node
    // kind, so `contains_jsx`'s existing `jsx_element` check already covers
    // it. Locking that in with a regression test.
    let (_program, react) = analyze_react(&[(
        "App.tsx",
        "function App({ children }: { children: JSX.Element }): JSX.Element {\n    return <>{children}</>;\n}\n",
    )]);
    assert!(react.components.iter().any(|c| c.name == "App"));
}

#[test]
fn namespaced_jsx_tag_usage_is_captured() {
    let (_program, react) = analyze_react(&[(
        "App.tsx",
        "function App(): JSX.Element {\n    return <Menu.Item label=\"hi\" />;\n}\n",
    )]);
    let usage = react
        .jsx_usages
        .iter()
        .find(|u| u.component_name == "Menu.Item")
        .expect("Menu.Item usage should be captured");
    assert_eq!(usage.passed_props, vec!["label".to_string()]);
}

#[test]
fn deeply_namespaced_jsx_tag_usage_is_captured() {
    let (_program, react) = analyze_react(&[(
        "App.tsx",
        "function App(): JSX.Element {\n    return <Nav.Menu.Item/>;\n}\n",
    )]);
    assert!(react
        .jsx_usages
        .iter()
        .any(|u| u.component_name == "Nav.Menu.Item"));
}

#[test]
fn namespaced_and_intrinsic_tags_mixed_are_distinguished_correctly() {
    let (_program, react) = analyze_react(&[(
        "App.tsx",
        r#"function App(): JSX.Element {
    return (
        <Nav.Menu.Item foo="x">
            <Nav.Item bar="y" />
            <div className="plain" />
        </Nav.Menu.Item>
    );
}
"#,
    )]);
    let names: Vec<&str> = react
        .jsx_usages
        .iter()
        .map(|u| u.component_name.as_str())
        .collect();
    assert!(names.contains(&"Nav.Menu.Item"));
    assert!(names.contains(&"Nav.Item"));
    assert!(
        !names.contains(&"div"),
        "the lowercase intrinsic tag must still be excluded: {:?}",
        names
    );
}
