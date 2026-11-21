//! Plain-JavaScript mirror of `spectograph-ts/tests/vue_support.rs`: route components,
//! file-based routing plugins, bundler globs, Vue 2 patterns, `defineComponent`, directive
//! modifiers, `components:` key remaps and unused scoped CSS.

use tempfile::tempdir;

use spectograph_analyzer::Analyzer;
use spectograph_core::analysis::{self, BloatTier};
use spectograph_core::cost::{CostModel, CostSource};
use spectograph_core::diagnostic::VUE_UNUSED_CSS_CLASS;
use spectograph_js::vue::{VueBindingKind, VueBindingMismatch};
use spectograph_js::JavaScriptAnalyzer;

fn write(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempdir().unwrap();
    for (name, content) in files {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }
    dir
}

fn dead(files: &[(&str, &str)]) -> Vec<String> {
    let dir = write(files);
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

fn mismatches(files: &[(&str, &str)]) -> Vec<VueBindingMismatch> {
    let dir = write(files);
    JavaScriptAnalyzer
        .analyze_vue_mismatches(dir.path())
        .unwrap()
        .1
}

const MAIN: &str = "import { createApp } from 'vue'\nimport App from './App.vue'\nimport router from './router'\ncreateApp(App).use(router).mount('#app')\n";
const APP: &str = "<template><router-view/></template>\n<script>\nexport default {}\n</script>\n";
const PAGE: &str = "<template><div>page</div></template>\n<script>\nexport default {}\n</script>\n";
const PKG: &str = r#"{"main":"main.js"}"#;

fn not_dead(d: &[String], names: &[&str]) {
    for n in names {
        assert!(!d.contains(&n.to_string()), "{n} wrongly reported: {d:?}");
    }
}

#[test]
fn route_components_eager_lazy_nested_and_named_are_reachable() {
    let d = dead(&[
        ("package.json", PKG),
        ("main.js", MAIN),
        ("App.vue", APP),
        ("Home.vue", PAGE),
        ("About.vue", PAGE),
        ("Child.vue", PAGE),
        ("Side.vue", PAGE),
        (
            "router.js",
            "import { createRouter } from 'vue-router'\nimport Home from './Home.vue'\nexport default createRouter({ routes: [{ path: '/', component: Home }, { path: '/about', component: () => import('./About.vue'), children: [{ path: 'c', component: () => import('./Child.vue') }, { path: 's', components: { side: () => import('./Side.vue') } }] }] })\n",
        ),
    ]);
    not_dead(&d, &["Home", "About", "Child", "Side"]);
}

#[test]
fn file_based_routing_plugin_makes_pages_roots() {
    let d = dead(&[
        (
            "package.json",
            r#"{"main":"main.js","devDependencies":{"vite-plugin-pages":"1"}}"#,
        ),
        (
            "main.js",
            "import { createApp } from 'vue'\nimport App from './App.vue'\ncreateApp(App).mount('#app')\n",
        ),
        ("App.vue", APP),
        ("src/pages/index.vue", PAGE),
    ]);
    not_dead(&d, &["index"]);
}

#[test]
fn bundler_globs_make_matched_modules_reachable() {
    let d = dead(&[
        ("package.json", PKG),
        ("main.js", MAIN),
        ("App.vue", APP),
        ("views/Home.vue", PAGE),
        ("views/Unrelated.vue", PAGE),
        ("components/Btn.vue", PAGE),
        (
            "router.js",
            "const pages = import.meta.glob('./views/Home.vue')\nconst ctx = require.context('./components', false, /\\.vue$/)\nexport default { pages, ctx }\n",
        ),
    ]);
    not_dead(&d, &["Home", "Btn"]);
    assert!(d.contains(&"Unrelated".to_string()), "{d:?}");
}

#[test]
fn new_vue_el_bootstraps_and_default_import_name_may_differ() {
    let d = dead(&[
        ("package.json", r#"{"main":"index.js"}"#),
        ("index.js", "export const lib = 1\n"),
        (
            "boot.js",
            "import Vue from 'vue'\nimport Root from './App.vue'\nnew Vue({ el: '#app', render: h => h(Root) })\n",
        ),
        (
            "App.vue",
            "<template><div/></template>\n<script>\nexport default { name: 'App' }\n</script>\n",
        ),
    ]);
    not_dead(&d, &["App"]);
}

#[test]
fn vue2_extend_is_a_component_with_declared_props_and_emits() {
    let dir = write(&[(
        "B.vue",
        "<script>\nimport Vue from 'vue'\nexport default Vue.extend({ props: { y: String }, emits: ['go'] })\n</script>\n",
    )]);
    let (_p, vue) = JavaScriptAnalyzer.analyze_vue(dir.path()).unwrap();
    assert_eq!(vue.components[0].declared_props, vec!["y"]);
    assert_eq!(vue.components[0].declared_emits, vec!["go"]);
}

#[test]
fn components_key_remap_modifiers_and_kebab_names_in_mismatch_checks() {
    let child = "<template><div/></template>\n<script>\nexport default { props: ['userId', 'label'], emits: ['change-value'] }\n</script>\n";
    let parent = |tpl: &str| {
        format!(
            "<template>{tpl}</template>\n<script>\nimport Card from './Child.vue'\nexport default {{ components: {{ FancyCard: Card }} }}\n</script>\n"
        )
    };
    let bindings = |tpl: &str| -> Vec<(String, VueBindingKind)> {
        mismatches(&[
            ("package.json", PKG),
            ("Parent.vue", &parent(tpl)),
            ("Child.vue", child),
        ])
        .into_iter()
        .map(|m| (m.binding, m.kind))
        .collect()
    };
    // Remapped tag resolves, kebab/camel and modifiers all match the declarations.
    assert!(bindings(
        "<fancy-card :user-id.sync=\"1\" :label=\"2\" @change-value.once=\"f\" @click.native=\"g\"/>"
    )
    .is_empty());
    // Undeclared ones are still caught, modifier or not.
    let found = bindings("<fancy-card :nope=\"1\" @bogus.once=\"f\"/>");
    assert!(
        found.contains(&("nope".to_string(), VueBindingKind::Prop)),
        "{found:?}"
    );
    assert!(
        found.contains(&("bogus".to_string(), VueBindingKind::Event)),
        "{found:?}"
    );
}

#[test]
fn unused_scoped_css_is_reported_for_plain_script_components() {
    let dir = write(&[
        ("package.json", PKG),
        (
            "main.js",
            "import { createApp } from 'vue'\nimport App from './App.vue'\ncreateApp(App).mount('#app')\n",
        ),
        (
            "App.vue",
            "<template><div class=\"used\"/></template>\n<script>\nexport default {}\n</script>\n<style scoped lang=\"scss\">\n.used { color: red }\n.dead { color: blue }\n</style>\n",
        ),
    ]);
    let (_p, diagnostics) = JavaScriptAnalyzer
        .analyze_with_diagnostics(dir.path())
        .unwrap();
    let css: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.code == VUE_UNUSED_CSS_CLASS)
        .collect();
    assert_eq!(css.len(), 1, "{diagnostics:?}");
    assert!(css[0].message.contains(".dead"));
    assert_eq!((css[0].file.as_str(), css[0].line), ("App.vue", 7));
}
