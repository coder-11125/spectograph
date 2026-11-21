//! Vue patterns that used to be documented as out of scope or silently mishandled:
//! route components (eager, lazy, nested, named views), file-based routing plugins, bundler
//! globs (`import.meta.glob`, `require.context`), non-identifier `:is`, `components:` key
//! remaps, Vue 2 (`Vue.extend`, mixins, filters, `new Vue({ el })`), store modules,
//! `defineComponent`, `defineEmits` type forms, directive modifiers, and unused scoped CSS.

use tempfile::tempdir;

use spectograph_analyzer::Analyzer;
use spectograph_core::analysis::{self, BloatTier};
use spectograph_core::cost::{CostModel, CostSource};
use spectograph_core::diagnostic::VUE_UNUSED_CSS_CLASS;
use spectograph_js::vue::{VueBindingKind, VueBindingMismatch};
use spectograph_ts::TypeScriptAnalyzer;

fn write(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempdir().unwrap();
    for (name, content) in files {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }
    dir
}

/// Names reported as guaranteed bloat.
fn dead(files: &[(&str, &str)]) -> Vec<String> {
    let dir = write(files);
    let mut program = TypeScriptAnalyzer.analyze(dir.path()).unwrap();
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
    TypeScriptAnalyzer
        .analyze_vue_mismatches(dir.path())
        .unwrap()
        .1
}

const MAIN: &str = "import { createApp } from 'vue'\nimport App from './App.vue'\nimport router from './router'\ncreateApp(App).use(router).mount('#app')\n";
const APP: &str = "<template><router-view/></template>\n<script setup lang=\"ts\">\n</script>\n";
const PAGE: &str = "<template><div>page</div></template>\n<script setup lang=\"ts\">\n</script>\n";
const PKG: &str = r#"{"main":"main.ts"}"#;

fn not_dead(d: &[String], names: &[&str]) {
    for n in names {
        assert!(!d.contains(&n.to_string()), "{n} wrongly reported: {d:?}");
    }
}

// --- Vue Router ---------------------------------------------------------------------------

#[test]
fn route_component_by_identifier_is_reachable() {
    let d = dead(&[
        ("package.json", PKG),
        ("main.ts", MAIN),
        ("App.vue", APP),
        ("Home.vue", PAGE),
        (
            "router.ts",
            "import { createRouter } from 'vue-router'\nimport Home from './Home.vue'\nexport default createRouter({ routes: [{ path: '/', component: Home }] })\n",
        ),
    ]);
    not_dead(&d, &["Home"]);
}

#[test]
fn lazy_route_components_nested_and_named_views_are_reachable() {
    let d = dead(&[
        ("package.json", PKG),
        ("main.ts", MAIN),
        ("App.vue", APP),
        ("Layout.vue", PAGE),
        ("Child.vue", PAGE),
        ("Side.vue", PAGE),
        (
            "router.ts",
            "import { createRouter } from 'vue-router'\nconst routes = [{ path: '/', component: () => import('./Layout.vue'), children: [{ path: 'c', component: () => import('./Child.vue') }, { path: 's', components: { default: () => import('./Layout.vue'), side: () => import('./Side.vue') } }] }]\nexport default createRouter({ routes })\n",
        ),
    ]);
    not_dead(&d, &["Layout", "Child", "Side"]);
}

#[test]
fn file_based_routing_plugins_make_pages_and_layouts_roots() {
    for (dep, cfg, plugin) in [
        (
            "vite-plugin-pages",
            "vite.config.ts",
            "import Pages from 'vite-plugin-pages'\nexport default { plugins: [Pages()] }\n",
        ),
        (
            "unplugin-vue-router",
            "vite.config.ts",
            "import VueRouter from 'unplugin-vue-router/vite'\nexport default { plugins: [VueRouter()] }\n",
        ),
    ] {
        let pkg = format!(r#"{{"main":"main.ts","devDependencies":{{"vite":"5","{dep}":"1"}}}}"#);
        let d = dead(&[
            ("package.json", pkg.as_str()),
            (cfg, plugin),
            (
                "main.ts",
                "import { createApp } from 'vue'\nimport App from './App.vue'\ncreateApp(App).mount('#app')\n",
            ),
            ("App.vue", APP),
            ("src/pages/index.vue", PAGE),
            ("src/pages/about.vue", PAGE),
            ("src/layouts/default.vue", PAGE),
        ]);
        not_dead(&d, &["index", "about", "default"]);
    }
}

#[test]
fn pages_folder_is_ordinary_without_a_routing_plugin() {
    let d = dead(&[
        ("package.json", r#"{"main":"main.ts","devDependencies":{"vite":"5"}}"#),
        (
            "main.ts",
            "import { createApp } from 'vue'\nimport App from './App.vue'\ncreateApp(App).mount('#app')\n",
        ),
        ("App.vue", APP),
        ("src/pages/orphan.vue", PAGE),
    ]);
    assert!(d.contains(&"orphan".to_string()), "{d:?}");
}

// --- Bundler globs -------------------------------------------------------------------------

#[test]
fn import_meta_glob_makes_matched_modules_reachable() {
    let d = dead(&[
        ("package.json", PKG),
        ("main.ts", MAIN),
        ("App.vue", APP),
        ("views/Home.vue", PAGE),
        ("views/About.vue", PAGE),
        ("other/Unmatched.vue", PAGE),
        (
            "router.ts",
            "import { createRouter } from 'vue-router'\nconst pages = import.meta.glob('./views/*.vue')\nexport default createRouter({ routes: Object.keys(pages).map(p => ({ path: p, component: pages[p] })) })\n",
        ),
    ]);
    not_dead(&d, &["Home", "About"]);
    assert!(
        d.contains(&"Unmatched".to_string()),
        "a non-matching file stays dead: {d:?}"
    );
}

#[test]
fn require_context_makes_matched_modules_reachable() {
    let d = dead(&[
        ("package.json", PKG),
        (
            "main.ts",
            "import Vue from 'vue'\nimport App from './App.vue'\nconst ctx = require.context('./components', true, /\\.vue$/)\nctx.keys().forEach((k: string) => Vue.component(k, ctx(k).default))\nnew Vue({ render: h => h(App) }).$mount('#app')\n",
        ),
        ("App.vue", APP),
        ("components/Btn.vue", PAGE),
        ("components/helper.ts", "export const h = 1\n"),
    ]);
    not_dead(&d, &["Btn"]);
    assert!(
        d.contains(&"h".to_string()),
        "regex excludes non-.vue: {d:?}"
    );
}

// --- Template/scope resolution -------------------------------------------------------------

#[test]
fn dynamic_is_expression_keeps_both_branches() {
    let d = dead(&[
        ("package.json", PKG),
        (
            "main.ts",
            "import { createApp } from 'vue'\nimport App from './App.vue'\ncreateApp(App).mount('#app')\n",
        ),
        (
            "App.vue",
            "<template><component :is=\"flag ? A : B\"/></template>\n<script setup lang=\"ts\">\nimport A from './A.vue'\nimport B from './B.vue'\nconst flag = true\n</script>\n",
        ),
        ("A.vue", PAGE),
        ("B.vue", PAGE),
    ]);
    not_dead(&d, &["A", "B"]);
}

#[test]
fn options_components_key_remap_is_resolved_for_mismatch_checks() {
    let m = mismatches(&[
        ("package.json", PKG),
        (
            "App.vue",
            "<template><fancy-card :label=\"1\" :bad=\"2\"/></template>\n<script lang=\"ts\">\nimport Card from './Card.vue'\nexport default { components: { FancyCard: Card } }\n</script>\n",
        ),
        (
            "Card.vue",
            "<template><div/></template>\n<script lang=\"ts\">\nexport default { props: ['label'] }\n</script>\n",
        ),
    ]);
    assert!(
        m.iter()
            .any(|m| m.binding == "bad" && m.kind == VueBindingKind::Prop),
        "{m:?}"
    );
    assert!(!m.iter().any(|m| m.binding == "label"), "{m:?}");
}

#[test]
fn default_import_of_options_api_component_reaches_the_component() {
    // Regression: the default import used to link only to the generic `default` export
    // symbol, leaving the component itself unreachable.
    let d = dead(&[
        ("package.json", PKG),
        (
            "main.ts",
            "import Vue from 'vue'\nimport Root from './App.vue'\nnew Vue({ render: h => h(Root) }).$mount('#app')\n",
        ),
        (
            "App.vue",
            "<template><div/></template>\n<script lang=\"ts\">\nexport default { name: 'App' }\n</script>\n",
        ),
    ]);
    not_dead(&d, &["App"]);
}

// --- Vue 2 ---------------------------------------------------------------------------------

#[test]
fn vue2_extend_mixins_filters_and_global_registration() {
    let d = dead(&[
        ("package.json", PKG),
        (
            "main.ts",
            "import Vue from 'vue'\nimport App from './App.vue'\nimport Btn from './Btn.vue'\nimport plugin from './plugin'\nVue.component('app-btn', Btn)\nVue.use(plugin)\nnew Vue({ render: h => h(App) }).$mount('#app')\n",
        ),
        (
            "App.vue",
            "<template><div>{{ n | money }}<app-btn/></div></template>\n<script lang=\"ts\">\nimport Vue from 'vue'\nimport { shared } from './shared'\nimport { money } from './filters'\nexport default Vue.extend({ mixins: [shared], filters: { money }, data() { return { n: 1 } } })\n</script>\n",
        ),
        ("Btn.vue", PAGE),
        ("plugin.ts", "export default { install() {} }\n"),
        ("shared.ts", "export const shared = { created() {} }\n"),
        ("filters.ts", "export function money(v: number) { return v }\n"),
    ]);
    assert!(d.is_empty(), "{d:?}");
}

#[test]
fn new_vue_with_el_option_is_a_bootstrap_entrypoint() {
    // `boot.ts` isn't named like an entry and isn't package.json's `main`; only the
    // `new Vue({ el })` call marks it.
    let d = dead(&[
        ("package.json", r#"{"main":"index.ts"}"#),
        ("index.ts", "export const lib = 1\n"),
        (
            "boot.ts",
            "import Vue from 'vue'\nimport App from './App.vue'\nnew Vue({ el: '#app', render: h => h(App) })\n",
        ),
        ("App.vue", APP),
    ]);
    not_dead(&d, &["App"]);
}

// --- Stores --------------------------------------------------------------------------------

#[test]
fn pinia_and_vuex_store_modules_are_ordinary_modules() {
    let d = dead(&[
        ("package.json", PKG),
        (
            "main.ts",
            "import { createApp } from 'vue'\nimport App from './App.vue'\nimport { createPinia } from 'pinia'\nimport store from './store'\ncreateApp(App).use(createPinia()).use(store).mount('#app')\n",
        ),
        (
            "App.vue",
            "<template><div>{{ counter.count }}</div></template>\n<script setup lang=\"ts\">\nimport { useCounter } from './stores/counter'\nconst counter = useCounter()\n</script>\n",
        ),
        (
            "stores/counter.ts",
            "import { defineStore } from 'pinia'\nexport const useCounter = defineStore('counter', { state: () => ({ count: 0 }), actions: { inc() { this.count++ } } })\n",
        ),
        (
            "store.ts",
            "import { createStore } from 'vuex'\nfunction increment(state: any) { state.n++ }\nexport default createStore({ state: { n: 0 }, mutations: { increment } })\n",
        ),
    ]);
    not_dead(&d, &["useCounter", "increment"]);
}

// --- Component declarations ----------------------------------------------------------------

#[test]
fn define_component_and_vue_extend_declare_props_and_emits() {
    let dir = write(&[
        (
            "A.vue",
            "<script lang=\"ts\">\nimport { defineComponent } from 'vue'\nexport default defineComponent({ name: 'A', props: ['x'], emits: ['go'] })\n</script>\n",
        ),
        (
            "B.vue",
            "<script lang=\"ts\">\nimport Vue from 'vue'\nexport default Vue.extend({ props: { y: String } })\n</script>\n",
        ),
    ]);
    let (_p, vue) = TypeScriptAnalyzer.analyze_vue(dir.path()).unwrap();
    let a = vue.components.iter().find(|c| c.name == "A").expect("A");
    assert_eq!(
        (a.declared_props.as_slice(), a.declared_emits.as_slice()),
        (&["x".to_string()][..], &["go".to_string()][..])
    );
    let b = vue.components.iter().find(|c| c.name == "B").expect("B");
    assert_eq!(b.declared_props, vec!["y"]);
}

fn emits_of(script: &str) -> Vec<String> {
    let dir = write(&[(
        "C.vue",
        &format!("<script setup lang=\"ts\">\n{script}\n</script>\n"),
    )]);
    let (_p, vue) = TypeScriptAnalyzer.analyze_vue(dir.path()).unwrap();
    vue.components[0].declared_emits.clone()
}

#[test]
fn define_emits_call_signature_function_type_and_interface_forms() {
    assert_eq!(
        emits_of(
            "const emit = defineEmits<{ (e: 'change', v: string): void; (e: 'close'): void }>()"
        ),
        vec!["change", "close"]
    );
    assert_eq!(
        emits_of("const emit = defineEmits<(e: 'a' | 'b') => void>()"),
        vec!["a", "b"]
    );
    assert_eq!(
        emits_of("interface E { (e: 'x'): void; y: [n: number] }\nconst emit = defineEmits<E>()"),
        vec!["y", "x"]
    );
    // The 3.3 property form still works.
    assert_eq!(
        emits_of("const emit = defineEmits<{ change: [v: string] }>()"),
        vec!["change"]
    );
}

#[test]
fn define_props_resolves_interfaces_but_not_ones_that_extend() {
    let props = |script: &str| {
        let dir = write(&[(
            "C.vue",
            &format!("<script setup lang=\"ts\">\n{script}\n</script>\n"),
        )]);
        TypeScriptAnalyzer
            .analyze_vue(dir.path())
            .unwrap()
            .1
            .components[0]
            .declared_props
            .clone()
    };
    assert_eq!(
        props("interface P { a: string; b?: number }\nconst p = defineProps<P>()"),
        vec!["a", "b"]
    );
    assert_eq!(
        props("interface P { a: string }\nconst p = withDefaults(defineProps<P>(), {})"),
        vec!["a"]
    );
    // A partial list would flag inherited props as mismatches, so it stays unknown (empty).
    assert!(props("interface B { z: number }\ninterface P extends B { a: string }\nconst p = defineProps<P>()").is_empty());
}

// --- Template directive handling -----------------------------------------------------------

const CHILD_EMITS: &str = "<template><div/></template>\n<script lang=\"ts\">\nimport { defineComponent } from 'vue'\nexport default defineComponent({ props: ['userId', 'modelValue'], emits: ['change-value', 'update:modelValue'] })\n</script>\n";

fn mismatch_bindings(parent_template: &str) -> Vec<String> {
    let parent = format!(
        "<template>{parent_template}</template>\n<script setup lang=\"ts\">\nimport Child from './Child.vue'\n</script>\n"
    );
    mismatches(&[
        ("package.json", PKG),
        ("Parent.vue", &parent),
        ("Child.vue", CHILD_EMITS),
    ])
    .into_iter()
    .map(|m| m.binding)
    .collect()
}

#[test]
fn kebab_and_camel_spellings_match_declared_names() {
    assert!(mismatch_bindings("<Child :user-id=\"1\" @change-value=\"f\"/>").is_empty());
    assert!(mismatch_bindings("<Child :userId=\"1\" @changeValue=\"f\"/>").is_empty());
}

#[test]
fn modifiers_do_not_hide_or_invent_bindings() {
    // `.native` is a DOM listener, `.once`/`.sync`/`.trim` are modifiers of real bindings.
    assert!(mismatch_bindings(
        "<Child v-model.trim=\"x\" :user-id.sync=\"1\" @change-value.once=\"f\" @click.native=\"g\"/>"
    )
    .is_empty());
    // An undeclared event with a modifier is still caught (the modifier used to hide it).
    assert_eq!(
        mismatch_bindings("<Child @bogus.once=\"f\"/>"),
        vec!["bogus"]
    );
}

// --- Unused scoped CSS ---------------------------------------------------------------------

#[test]
fn unused_scoped_css_classes_are_reported_as_diagnostics() {
    let dir = write(&[
        ("package.json", PKG),
        (
            "main.ts",
            "import { createApp } from 'vue'\nimport App from './App.vue'\ncreateApp(App).mount('#app')\n",
        ),
        (
            "App.vue",
            "<template><div class=\"used\"/></template>\n<script setup lang=\"ts\">\n</script>\n<style scoped>\n.used { color: red }\n.dead { color: blue }\n</style>\n",
        ),
    ]);
    let (_p, diagnostics) = TypeScriptAnalyzer
        .analyze_with_diagnostics(dir.path())
        .unwrap();
    let css: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.code == VUE_UNUSED_CSS_CLASS)
        .collect();
    assert_eq!(css.len(), 1, "{diagnostics:?}");
    assert!(css[0].message.contains(".dead"));
    assert_eq!((css[0].file.as_str(), css[0].line), ("App.vue", 6));
}

#[test]
fn unused_css_is_only_reported_by_the_analyzer_owning_the_script_language() {
    // A plain-script component is the JavaScript analyzer's; the TS one must not report it.
    let dir = write(&[
        ("package.json", PKG),
        ("main.ts", "export const x = 1\n"),
        (
            "Plain.vue",
            "<template><div/></template>\n<script>\nexport default {}\n</script>\n<style scoped>\n.dead { x: 1 }\n</style>\n",
        ),
    ]);
    let (_p, diagnostics) = TypeScriptAnalyzer
        .analyze_with_diagnostics(dir.path())
        .unwrap();
    assert!(
        diagnostics.iter().all(|d| d.code != VUE_UNUSED_CSS_CLASS),
        "{diagnostics:?}"
    );
}

#[test]
fn components_with_mixins_or_extends_have_unknown_bindings() {
    // The mixin supplies `shared`, which the component's own `props` doesn't list; passing it
    // must not be reported as undeclared.
    let m = mismatches(&[
        ("package.json", PKG),
        (
            "Parent.vue",
            "<template><Child :shared=\"1\" @inherited=\"f\"/></template>\n<script setup lang=\"ts\">\nimport Child from './Child.vue'\n</script>\n",
        ),
        (
            "Child.vue",
            "<template><div/></template>\n<script lang=\"ts\">\nimport { mixin } from './mixin'\nexport default { mixins: [mixin], props: ['own'], emits: ['own-event'] }\n</script>\n",
        ),
        ("mixin.ts", "export const mixin = { props: ['shared'] }\n"),
    ]);
    assert!(m.is_empty(), "{m:?}");
}

// --- tsconfig include forms ---------------------------------------------------------------

#[test]
fn tsconfig_include_dot_and_dot_slash_forms_match_everything_beneath() {
    // Regression (found on vuejs/core's `packages-private/tsconfig.json`): `"include": ["."]`
    // matched nothing, silently dropping every `.ts` file and orphaning the `.vue` files.
    for include in [r#"["."]"#, r#"["./"]"#, r#"["./app"]"#, r#"["app"]"#] {
        let tsconfig = format!(r#"{{"include": {include}}}"#);
        let d = dead(&[
            ("tsconfig.json", tsconfig.as_str()),
            ("app/package.json", r#"{"name":"app"}"#),
            (
                "app/src/main.ts",
                "import { createApp } from 'vue'\nimport App from './App.vue'\ncreateApp(App).mount('#app')\n",
            ),
            (
                "app/src/App.vue",
                "<template><div/></template>\n<script setup lang=\"ts\">\nimport { ref } from 'vue'\nconst n = ref(0)\nn.value++\n</script>\n",
            ),
        ]);
        assert!(d.is_empty(), "include {include}: {d:?}");
    }
}

// --- Fallthrough attributes ---------------------------------------------------------------

fn bindings_against_root(root_markup: &str, usage: &str) -> Vec<String> {
    let child = format!(
        "<template>{root_markup}</template>\n<script setup lang=\"ts\">\ndefineProps<{{ name: string }}>()\ndefineEmits<{{ change: [] }}>()\n</script>\n"
    );
    let parent = format!(
        "<template>{usage}</template>\n<script setup lang=\"ts\">\nimport Child from './Child.vue'\n</script>\n"
    );
    mismatches(&[
        ("package.json", PKG),
        ("Parent.vue", &parent),
        ("Child.vue", &child),
    ])
    .into_iter()
    .map(|m| m.binding)
    .collect()
}

#[test]
fn undeclared_attrs_are_checked_only_against_plain_element_roots() {
    let usage = "<Child name=\"n\" :extra=\"1\" aria-label=\"a\" data-id=\"1\" id=\"x\" @click=\"f\" @bogus=\"g\"/>";
    // Plain element root: `extra` and `bogus` are real mismatches; aria/data/id/@click fall
    // through legitimately.
    let mut found = bindings_against_root("<div/>", usage);
    found.sort();
    assert_eq!(found, vec!["bogus", "extra"]);
    // A component root, a `<slot/>` root or a `<component :is>` root forwards attrs to
    // something we can't see: nothing to check.
    for root in [
        "<RouterLink/>",
        "<router-link/>",
        "<slot/>",
        "<component :is=\"c\"/>",
    ] {
        assert!(bindings_against_root(root, usage).is_empty(), "{root}");
    }
}

#[test]
fn nested_tsconfig_include_keeps_files_the_root_config_leaves_out() {
    // vuejs/core shape: the root tsconfig lists only `packages/*/src`, while a nested
    // `tools/tsconfig.json` (`include: ["."]`) covers `tools/app/**`. Dropping `main.ts`
    // there orphaned the `.vue` file the (unfiltered) `.vue` discovery still found.
    let d = dead(&[
        ("tsconfig.json", r#"{"include": ["packages/*/src"]}"#),
        ("tools/tsconfig.json", r#"{"include": ["."]}"#),
        ("tools/app/package.json", r#"{"name":"app"}"#),
        (
            "tools/app/src/main.ts",
            "import { createApp } from 'vue'\nimport App from './App.vue'\ncreateApp(App).mount('#app')\n",
        ),
        (
            "tools/app/src/App.vue",
            "<template><div/></template>\n<script setup lang=\"ts\">\nimport { ref } from 'vue'\nconst n = ref(0)\nn.value++\n</script>\n",
        ),
        ("packages/a/src/index.ts", "export const a = 1\n"),
    ]);
    assert!(
        !d.iter().any(|n| matches!(n.as_str(), "ref" | "n" | "App")),
        "{d:?}"
    );
    // A directory covered by neither config stays excluded, as before.
    let d = dead(&[
        ("tsconfig.json", r#"{"include": ["packages/*/src"]}"#),
        ("packages/a/src/index.ts", "export const a = 1\n"),
        ("other/orphan.ts", "export const orphan = 1\n"),
    ]);
    assert!(
        !d.contains(&"orphan".to_string()),
        "excluded file must not be analyzed: {d:?}"
    );
}

#[test]
fn define_model_declares_its_prop_and_update_event() {
    let child = |script: &str| {
        format!("<template><div/></template>\n<script setup lang=\"ts\">\n{script}\n</script>\n")
    };
    let parent = "<template><Child v-model=\"a\" v-model:title=\"t\" :extra=\"1\"/></template>\n<script setup lang=\"ts\">\nimport Child from './Child.vue'\n</script>\n";
    let run = |script: &str| -> Vec<String> {
        mismatches(&[
            ("package.json", PKG),
            ("Parent.vue", parent),
            ("Child.vue", &child(script)),
        ])
        .into_iter()
        .map(|m| format!("{}:{:?}", m.binding, m.kind))
        .collect()
    };
    // Only models: `modelValue`/`title` are declared, `extra` genuinely isn't.
    let found = run("const a = defineModel()\nconst t = defineModel('title')");
    assert_eq!(found, vec!["extra:Prop"], "{found:?}");
    // Models next to enumerable macros merge with them.
    assert_eq!(
        run("defineProps<{ extra: number }>()\nconst a = defineModel()\nconst t = defineModel('title')\ndefineEmits<{ go: [] }>()"),
        Vec::<String>::new()
    );
    // A macro we can't enumerate keeps the list unknown rather than half-known.
    let found = run(
        "const p = defineProps(someExternalPropsObject)\nconst a = defineModel()\nconst t = defineModel('title')",
    );
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn type_level_test_files_are_secondary_roots() {
    let d = dead(&[
        ("package.json", PKG),
        ("main.ts", "export const lib = 1\n"),
        (
            "dts-test/thing.test-d.ts",
            "import { lib } from '../main'\nexport const x = lib\n",
        ),
    ]);
    assert!(d.is_empty(), "{d:?}");
}

#[test]
fn resolve_component_strings_reach_components_by_name() {
    // `Dyn.vue` is never imported or used in a template; only a string lookup names it.
    let d = dead(&[
        ("package.json", PKG),
        (
            "main.ts",
            "import { createApp } from 'vue'\nimport App from './App.vue'\ncreateApp(App).mount('#app')\n",
        ),
        (
            "App.vue",
            "<template><div/></template>\n<script setup lang=\"ts\">\nimport { resolveComponent } from 'vue'\nconst C = resolveComponent('Dyn')\nconsole.log(C)\n</script>\n",
        ),
        ("Dyn.vue", "<template><i/></template>\n<script lang=\"ts\">\nexport default { name: 'Dyn' }\n</script>\n"),
        ("Other.vue", PAGE),
    ]);
    not_dead(&d, &["Dyn"]);
    assert!(
        d.contains(&"Other".to_string()),
        "unrelated component stays dead: {d:?}"
    );
}
