//! Regression tests for the JavaScript accuracy fixes found by running `check` on real
//! projects (an Express-style CommonJS library, a CRA React/Redux app, a Vue app): each test
//! is a minimal project reproducing one class of false positive -- or a true positive that
//! must keep being caught.

use tempfile::tempdir;

use spectograph_analyzer::Analyzer;
use spectograph_core::analysis::{self, BloatTier};
use spectograph_core::cost::{CostModel, CostSource};
use spectograph_js::JavaScriptAnalyzer;

/// Names of every symbol reported as guaranteed bloat in the project made of `files`.
fn dead(files: &[(&str, &str)]) -> Vec<String> {
    let dir = tempdir().unwrap();
    for (name, content) in files {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }
    let mut program = JavaScriptAnalyzer.analyze(dir.path()).unwrap();
    let model = CostModel::build(&program, CostSource::Estimated);
    let mut names: Vec<String> = analysis::analyze(&mut program, &model)
        .findings
        .into_iter()
        .filter(|f| f.tier == BloatTier::Guaranteed)
        .map(|f| f.name)
        .collect();
    names.sort();
    names
}

fn no_dead(files: &[(&str, &str)]) {
    assert_eq!(dead(files), Vec::<String>::new());
}

#[test]
fn exports_assigned_from_a_local_binding_do_not_shadow_it() {
    // `exports.Router = Router` registers a *property*, not a binding: the `Router` on the
    // right-hand side is still the imported one.
    no_dead(&[
        ("package.json", r#"{"main": "index.js"}"#),
        (
            "index.js",
            "var Router = require('./router');\nexports = module.exports = function create() {};\nexports.Router = Router;\n",
        ),
        ("router.js", "module.exports = function router() {};\n"),
    ]);
}

#[test]
fn destructured_require_links_each_binding_and_flags_the_unused_one() {
    let found = dead(&[
        ("package.json", r#"{"main": "index.js"}"#),
        (
            "index.js",
            "const { used, unused } = require('./util');\nused();\n",
        ),
        (
            "util.js",
            "exports.used = function used() {};\nexports.unused = function unused() {};\n",
        ),
    ]);
    assert!(found.contains(&"unused".to_string()), "{found:?}");
    assert!(!found.contains(&"used".to_string()), "{found:?}");
}

#[test]
fn require_of_a_module_object_uses_its_members() {
    no_dead(&[
        ("package.json", r#"{"main": "index.js"}"#),
        (
            "index.js",
            "var agent = require('./agent');\nagent.Articles.all();\n",
        ),
        (
            "agent.js",
            "const requests = { get: () => 1 };\nconst Articles = { all: () => requests.get() };\nmodule.exports = { Articles };\n",
        ),
    ]);
}

#[test]
fn default_export_of_an_object_literal_reaches_its_helpers() {
    no_dead(&[
        ("package.json", r#"{"main": "index.js"}"#),
        (
            "index.js",
            "import agent from './agent';\nagent.Articles.all();\n",
        ),
        (
            "agent.js",
            "const requests = { get: () => 1 };\nconst Articles = { all: () => requests.get() };\nexport default { Articles };\n",
        ),
    ]);
}

#[test]
fn reexports_from_a_barrel_link_through() {
    no_dead(&[
        ("package.json", r#"{"main": "index.js"}"#),
        (
            "index.js",
            "import { Button } from './components';\nexport const b = Button;\n",
        ),
        (
            "components/index.js",
            "export { Button } from './Button';\n",
        ),
        ("components/Button.js", "export function Button() {}\n"),
    ]);
}

#[test]
fn import_aliases_bind_the_alias_not_the_original_name() {
    no_dead(&[
        ("package.json", r#"{"main": "index.js"}"#),
        (
            "index.js",
            "import { original as renamed } from './m';\nexport const x = renamed();\n",
        ),
        ("m.js", "export function original() {}\n"),
    ]);
}

#[test]
fn jsx_extension_resolves_without_being_spelled() {
    no_dead(&[
        ("package.json", r#"{"main": "index.js"}"#),
        (
            "index.js",
            "import App from './App';\nexport const root = App;\n",
        ),
        (
            "App.jsx",
            "export default function App() { return null; }\n",
        ),
    ]);
}

#[test]
fn react_import_is_used_by_jsx_and_lifecycle_methods_by_react() {
    no_dead(&[
        ("package.json", r#"{"main": "index.js"}"#),
        (
            "index.js",
            "import App from './App';\nexport const root = App;\n",
        ),
        (
            "App.js",
            r#"import React from 'react';
export default class App extends React.Component {
  constructor(props) { super(props); }
  componentWillMount() {}
  render() { return <div>hi</div>; }
}
"#,
        ),
    ]);
}

#[test]
fn lifecycle_methods_are_recognized_through_a_project_base_class() {
    no_dead(&[
        ("package.json", r#"{"main": "index.js"}"#),
        (
            "index.js",
            "import Child from './Child';\nexport const c = Child;\n",
        ),
        (
            "Base.js",
            "import React from 'react';\nexport class Base extends React.Component { render() { return <i />; } }\n",
        ),
        (
            "Child.js",
            "import React from 'react';\nimport { Base } from './Base';\nexport default class Child extends Base { componentDidMount() {} render() { return <b />; } }\n",
        ),
    ]);
}

#[test]
fn a_non_component_class_does_not_get_react_lifecycle_protection() {
    let found = dead(&[
        ("package.json", r#"{"main": "index.js"}"#),
        (
            "index.js",
            "class Plain { render() { return 1; } }\nexport const p = new Plain();\n",
        ),
    ]);
    assert_eq!(found, vec!["Plain.render"]);
}

#[test]
fn dynamic_import_of_a_page_reaches_it() {
    no_dead(&[
        ("package.json", r#"{"main": "index.js"}"#),
        ("index.js", "export const load = () => import('./page');\n"),
        (
            "page.js",
            "export default function Page() { return helper(); }\nfunction helper() { return 1; }\n",
        ),
    ]);
}

#[test]
fn unused_imports_and_dead_modules_are_still_caught() {
    let found = dead(&[
        ("package.json", r#"{"main": "index.js"}"#),
        (
            "index.js",
            "import { used, forgotten } from './m';\nexport const x = used();\n",
        ),
        (
            "m.js",
            "export function used() {}\nexport function forgotten() {}\n",
        ),
        ("orphan.js", "export function orphaned() {}\n"),
    ]);
    assert!(found.contains(&"forgotten".to_string()), "{found:?}");
    assert!(found.contains(&"orphaned".to_string()), "{found:?}");
    assert!(!found.contains(&"used".to_string()), "{found:?}");
}

#[test]
fn package_json_scripts_name_entry_files() {
    let found = dead(&[
        (
            "package.json",
            r#"{"scripts": {"seed": "node scripts/seed.js"}}"#,
        ),
        (
            "scripts/seed.js",
            "const { seed } = require('../lib/db');\nseed();\n",
        ),
        ("lib/db.js", "exports.seed = function seed() {};\n"),
        ("lib/dead.js", "exports.dead = function dead() {};\n"),
    ]);
    assert!(!found.contains(&"seed".to_string()), "{found:?}");
    assert!(found.contains(&"dead".to_string()), "{found:?}");
}
