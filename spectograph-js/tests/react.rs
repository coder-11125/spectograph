//! Tests for React-specific support: JSX parsing, component detection, prop
//! tracking, and hooks usage.

use tempfile::tempdir;

use spectograph_analyzer::Analyzer;
use spectograph_core::analysis;
use spectograph_core::cost::{CostModel, CostSource};
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

// --- Class components -----------------------------------------------------------

#[test]
fn class_extending_react_component_is_detected() {
    let (_program, react) = analyze_react(&[(
        "App.jsx",
        "class App extends React.Component {\n    render() {\n        return <div>hi</div>;\n    }\n}\n",
    )]);
    assert!(
        react.components.iter().any(|c| c.name == "App"),
        "a class extending React.Component with a JSX-returning render() should be detected"
    );
}

#[test]
fn class_extending_bare_component_import_is_detected() {
    // `import { Component } from 'react'; class App extends Component` -- the
    // namespace varies with how React was imported, so detection matches on
    // the trailing identifier only.
    let (_program, react) = analyze_react(&[(
        "App.jsx",
        "class App extends Component {\n    render() {\n        return <div>hi</div>;\n    }\n}\n",
    )]);
    assert!(react.components.iter().any(|c| c.name == "App"));
}

#[test]
fn class_extending_pure_component_is_detected() {
    let (_program, react) = analyze_react(&[(
        "App.jsx",
        "class App extends React.PureComponent {\n    render() {\n        return <div>hi</div>;\n    }\n}\n",
    )]);
    assert!(react.components.iter().any(|c| c.name == "App"));
}

#[test]
fn class_component_without_render_is_not_a_component() {
    let (_program, react) = analyze_react(&[(
        "App.jsx",
        "class App extends React.Component {\n    helper() {\n        return 1;\n    }\n}\n",
    )]);
    assert!(
        !react.components.iter().any(|c| c.name == "App"),
        "no render() at all means nothing to detect it by"
    );
}

#[test]
fn class_not_extending_component_is_not_a_component() {
    let (_program, react) = analyze_react(&[(
        "App.jsx",
        "class App {\n    render() {\n        return <div>hi</div>;\n    }\n}\n",
    )]);
    assert!(
        !react.components.iter().any(|c| c.name == "App"),
        "a render() method alone isn't enough -- it must extend Component/PureComponent"
    );
}

#[test]
fn class_component_declared_props_are_empty() {
    // Unlike a function component's destructured parameter, a class
    // component's props are read off `this.props.x`, which we don't do flow
    // analysis on -- same "can't enumerate" case as a bare `props` parameter.
    let (_program, react) = analyze_react(&[(
        "App.jsx",
        "class App extends React.Component {\n    render() {\n        return <div>{this.props.title}</div>;\n    }\n}\n",
    )]);
    let app = react.components.iter().find(|c| c.name == "App").unwrap();
    assert!(app.declared_props.is_empty());
}

#[test]
fn jsx_inside_class_render_method_is_captured_as_usage() {
    let (_program, react) = analyze_react(&[(
        "App.jsx",
        r#"function Child() { return <span/>; }
class App extends React.Component {
    render() {
        return <div><Child/></div>;
    }
}
"#,
    )]);
    assert!(
        react.jsx_usages.iter().any(|u| u.component_name == "Child"),
        "JSX rendered from inside a class component's render() should still be captured"
    );
}

#[test]
fn class_component_lifecycle_methods_are_not_falsely_flagged_as_bloat() {
    // Without an explicit edge, `render()` (and anything it renders) always
    // looks unreachable and gets flagged as guaranteed bloat, since React
    // calls it through its own reconciler -- never through code in the
    // analyzed codebase. Same category of problem as Angular DI's
    // constructor injection in the TS analyzer.
    let mut program = analyze_js(&[
        (
            "App.jsx",
            r#"import Child from './Child';

class App extends React.Component {
    render() {
        return <div><Child/></div>;
    }
    componentDidMount() {
        console.log('mounted');
    }
}

export default App;
"#,
        ),
        (
            "Child.jsx",
            "function Child() { return <span/>; }\nexport default Child;\n",
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
    // The lifecycle-edge protection must be specific to React's own
    // lifecycle names -- an ordinary unused helper method on the same class
    // should still be caught.
    let mut program = analyze_js(&[(
        "App.jsx",
        r#"class App extends React.Component {
    render() {
        return <div>hi</div>;
    }
    unusedHelper() {
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
    // A plain class (doesn't extend Component/PureComponent) that happens to
    // have a method named `render` is not a React component -- that method
    // must not get the reachability exemption.
    let mut program = analyze_js(&[(
        "App.jsx",
        "class NotAComponent {\n    render() {\n        return 1;\n    }\n}\n\nexport default NotAComponent;\n",
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
        "Input.jsx",
        "const Input = forwardRef((props, ref) => {\n    return <input ref={ref} value={props.value} />;\n});\n",
    )]);
    assert!(
        react.components.iter().any(|c| c.name == "Input"),
        "forwardRef((props, ref) => ...) should be detected as a component"
    );
}

#[test]
fn namespaced_memo_component_is_detected() {
    let (_program, react) = analyze_react(&[(
        "Badge.jsx",
        "const Badge = React.memo(({ count }) => <span>{count}</span>);\n",
    )]);
    let badge = react.components.iter().find(|c| c.name == "Badge").unwrap();
    assert_eq!(badge.declared_props, vec!["count".to_string()]);
}

#[test]
fn nested_memo_forward_ref_component_is_detected() {
    // memo(forwardRef(...)) is a common real combination.
    let (_program, react) = analyze_react(&[(
        "Button.jsx",
        "const Button = memo(forwardRef(function Button(props, ref) {\n    return <button ref={ref}>{props.label}</button>;\n}));\n",
    )]);
    assert!(react.components.iter().any(|c| c.name == "Button"));
}

#[test]
fn forward_ref_first_param_is_still_props_not_ref() {
    let (_program, react) = analyze_react(&[(
        "Input.jsx",
        "const Input = forwardRef(({ value, onChange }, ref) => {\n    return <input ref={ref} value={value} onChange={onChange} />;\n});\n",
    )]);
    let input = react.components.iter().find(|c| c.name == "Input").unwrap();
    let mut props = input.declared_props.clone();
    props.sort();
    assert_eq!(props, vec!["onChange", "value"]);
}

#[test]
fn wrapper_call_to_unrelated_function_is_not_a_component() {
    // A random call wrapping an arrow function that ISN'T forwardRef/memo
    // must not be treated as a component wrapper.
    let (_program, react) = analyze_react(&[(
        "helpers.jsx",
        "const Wrapped = debounce(() => {\n    return <div/>;\n});\n",
    )]);
    assert!(!react.components.iter().any(|c| c.name == "Wrapped"));
}

#[test]
fn hook_calls_inside_forward_ref_component_are_attributed_correctly() {
    let (program, react) = analyze_react(&[(
        "Input.jsx",
        r#"import { useState } from 'react';

const Input = forwardRef((props, ref) => {
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
        "App.jsx",
        r#"const Input = forwardRef((props, ref) => <input ref={ref} />);
function App() {
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
        "App.jsx",
        "function App({ children }) {\n    return <>{children}</>;\n}\n",
    )]);
    assert!(react.components.iter().any(|c| c.name == "App"));
}

#[test]
fn namespaced_jsx_tag_usage_is_captured() {
    let (_program, react) = analyze_react(&[(
        "App.jsx",
        r#"function App() {
    return <Menu.Item label="hi" />;
}
"#,
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
        "App.jsx",
        "function App() {\n    return <Nav.Menu.Item/>;\n}\n",
    )]);
    assert!(react
        .jsx_usages
        .iter()
        .any(|u| u.component_name == "Nav.Menu.Item"));
}

#[test]
fn namespaced_and_intrinsic_tags_mixed_are_distinguished_correctly() {
    let (_program, react) = analyze_react(&[(
        "App.jsx",
        r#"function App() {
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
