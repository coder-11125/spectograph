//! Tests for Vue SFC (`.vue`) support: file discovery, script-block
//! extraction/materialization (correct line numbers, resolved through the
//! ordinary parse/resolve pipeline), Vue component detection (Options API
//! and `<script setup>`, props/emits), template-usage reachability edges
//! (the Vue analogue of Angular's template scanner), template prop/emit
//! mismatch checking, and `defineAsyncComponent` lazy-loading edges.

use tempfile::tempdir;

use spectograph_analyzer::{AnalyzeError, Analyzer};
use spectograph_core::analysis;
use spectograph_core::cost::{CostModel, CostSource};
use spectograph_core::ir::{Program, RefKind, SymbolKind};
use spectograph_js::{JavaScriptAnalyzer, VueAnalysis};

fn analyze_js(files: &[(&str, &str)]) -> Program {
    let dir = tempdir().unwrap();
    for (name, content) in files {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }
    JavaScriptAnalyzer.analyze(dir.path()).unwrap()
}

fn analyze_vue(files: &[(&str, &str)]) -> (Program, VueAnalysis) {
    let dir = tempdir().unwrap();
    for (name, content) in files {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }
    JavaScriptAnalyzer.analyze_vue(dir.path()).unwrap()
}

fn vue_mismatches(files: &[(&str, &str)]) -> Vec<spectograph_js::vue::VueBindingMismatch> {
    let dir = tempdir().unwrap();
    for (name, content) in files {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }
    JavaScriptAnalyzer
        .analyze_vue_mismatches(dir.path())
        .unwrap()
        .1
}

#[test]
fn vue_file_with_plain_script_is_parsed_with_correct_line_numbers() {
    let program = analyze_js(&[(
        "App.vue",
        "<template>\n  <div>{{ msg }}</div>\n</template>\n\n<script>\nexport function greet() {\n  return 'hi'\n}\n</script>\n",
    )]);

    let greet = program
        .symbols
        .iter()
        .find(|s| s.name == "greet" && s.kind == SymbolKind::Function)
        .expect("greet function symbol");
    assert!(greet.file.to_string_lossy().ends_with("App.vue"));
    // `export function greet() {` is on line 6 of the source file above.
    assert_eq!(greet.span.start_line, 6);
}

#[test]
fn vue_to_vue_import_resolves() {
    let program = analyze_js(&[
        (
            "Greeter.vue",
            "<script>\nexport function greet() {\n  return 'hi'\n}\n</script>\n",
        ),
        (
            "main.js",
            "import { greet } from './Greeter.vue'\ngreet()\n",
        ),
    ]);

    let greet = program
        .symbols
        .iter()
        .find(|s| s.name == "greet" && s.kind == SymbolKind::Function)
        .expect("greet function symbol");
    let import = program
        .symbols
        .iter()
        .find(|s| s.name == "greet" && s.kind == SymbolKind::Import)
        .expect("greet import symbol");
    assert!(program
        .references
        .iter()
        .any(|r| r.from == import.id && r.to == greet.id && r.kind == RefKind::ReExport));
}

#[test]
fn vue_file_with_typescript_script_setup_is_not_picked_up_by_js_analyzer() {
    // A `<script setup lang="ts">` block belongs to the TypeScript analyzer
    // (see `spectograph_ts` for the equivalent positive-case test); with no
    // other JS-family source in the project, the JS analyzer must find
    // nothing to parse rather than silently swallow the TS content.
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("App.vue"),
        "<script setup lang=\"ts\">\nconst x: number = 1\n</script>\n",
    )
    .unwrap();

    let result = JavaScriptAnalyzer.analyze(dir.path());
    assert!(matches!(result, Err(AnalyzeError::NoSourceFiles(_))));
}

// --- component detection -------------------------------------------------

#[test]
fn options_api_component_with_array_props_and_explicit_name_is_detected() {
    let (_program, vue) = analyze_vue(&[(
        "Widget.vue",
        "<script>\nexport default {\n  name: 'MyWidget',\n  props: ['label', 'count'],\n  emits: ['change'],\n}\n</script>\n",
    )]);

    assert_eq!(vue.components.len(), 1);
    let c = &vue.components[0];
    assert_eq!(c.name, "MyWidget");
    assert_eq!(c.declared_props, vec!["label", "count"]);
    assert_eq!(c.declared_emits, vec!["change"]);
}

#[test]
fn options_api_component_with_object_props_falls_back_to_filename_for_name() {
    let (_program, vue) = analyze_vue(&[(
        "UserCard.vue",
        "<script>\nexport default {\n  props: {\n    userId: String,\n    active: { type: Boolean, default: false },\n  },\n}\n</script>\n",
    )]);

    assert_eq!(vue.components.len(), 1);
    let c = &vue.components[0];
    assert_eq!(c.name, "UserCard", "no `name:` property -> filename stem");
    assert_eq!(c.declared_props, vec!["userId", "active"]);
    assert!(c.declared_emits.is_empty());
}

#[test]
fn script_setup_component_with_define_props_and_define_emits_is_detected() {
    let (_program, vue) = analyze_vue(&[(
        "Counter.vue",
        "<script setup>\nconst props = defineProps({ start: Number })\nconst emit = defineEmits(['increment'])\n</script>\n",
    )]);

    assert_eq!(vue.components.len(), 1);
    let c = &vue.components[0];
    assert_eq!(c.name, "Counter");
    assert_eq!(c.declared_props, vec!["start"]);
    assert_eq!(c.declared_emits, vec!["increment"]);
}

#[test]
fn script_setup_component_with_bare_define_props_statement_is_detected() {
    let (_program, vue) = analyze_vue(&[(
        "Simple.vue",
        "<script setup>\ndefineProps(['label'])\n</script>\n",
    )]);

    assert_eq!(vue.components.len(), 1);
    assert_eq!(vue.components[0].declared_props, vec!["label"]);
}

#[test]
fn combined_script_and_script_setup_component_uses_name_from_plain_script() {
    let (_program, vue) = analyze_vue(&[(
        "Widget.vue",
        "<script>\nexport default {\n  name: 'ExplicitName',\n}\n</script>\n<script setup>\nconst props = defineProps(['size'])\n</script>\n",
    )]);

    assert_eq!(vue.components.len(), 1);
    let c = &vue.components[0];
    assert_eq!(c.name, "ExplicitName");
    assert_eq!(c.declared_props, vec!["size"]);
}

#[test]
fn plain_js_file_without_vue_extension_never_produces_a_vue_component() {
    // An Options-API-shaped object literal isn't a reliable signal outside
    // an actual `.vue` file -- e.g. a config object that happens to have a
    // `props`/`emits` key must not be mistaken for a component.
    let (_program, vue) = analyze_vue(&[(
        "config.js",
        "export default {\n  name: 'not-a-component',\n  props: ['a', 'b'],\n}\n",
    )]);
    assert!(vue.components.is_empty());
}

#[test]
fn vue_component_symbol_participates_in_the_ordinary_symbol_graph() {
    let program = analyze_js(&[(
        "Widget.vue",
        "<script>\nexport default {\n  name: 'Widget',\n}\n</script>\n",
    )]);
    assert!(program
        .symbols
        .iter()
        .any(|s| s.name == "Widget" && s.kind == SymbolKind::Const));
}

// --- template-usage reachability edges -----------------------------------

fn const_id(program: &Program, name: &str) -> spectograph_core::ir::SymbolId {
    program
        .symbols
        .iter()
        .find(|s| s.name == name && s.kind == SymbolKind::Const)
        .unwrap_or_else(|| panic!("no Const symbol named {name}"))
        .id
}

fn has_static_edge(
    program: &Program,
    from: spectograph_core::ir::SymbolId,
    to: spectograph_core::ir::SymbolId,
) -> bool {
    program
        .references
        .iter()
        .any(|r| r.from == from && r.to == to && r.kind == RefKind::Static)
}

#[test]
fn kebab_case_tag_usage_creates_template_edge() {
    let program = analyze_js(&[
        (
            "App.vue",
            "<template>\n  <user-card/>\n</template>\n<script setup>\nimport UserCard from './UserCard.vue'\n</script>\n",
        ),
        (
            "UserCard.vue",
            "<template><div/></template>\n<script setup>\nconst label = 'hi'\n</script>\n",
        ),
    ]);
    let app = const_id(&program, "App");
    let user_card = const_id(&program, "UserCard");
    assert!(has_static_edge(&program, app, user_card));
}

#[test]
fn pascal_case_tag_usage_also_creates_template_edge() {
    let program = analyze_js(&[
        (
            "App.vue",
            "<template>\n  <UserCard/>\n</template>\n<script setup>\nimport UserCard from './UserCard.vue'\n</script>\n",
        ),
        (
            "UserCard.vue",
            "<template><div/></template>\n<script setup>\nconst label = 'hi'\n</script>\n",
        ),
    ]);
    let app = const_id(&program, "App");
    let user_card = const_id(&program, "UserCard");
    assert!(has_static_edge(&program, app, user_card));
}

#[test]
fn dynamic_is_binding_creates_template_edge() {
    let program = analyze_js(&[
        (
            "App.vue",
            "<template>\n  <component :is=\"UserCard\"/>\n</template>\n<script setup>\nimport UserCard from './UserCard.vue'\n</script>\n",
        ),
        (
            "UserCard.vue",
            "<template><div/></template>\n<script setup>\nconst label = 'hi'\n</script>\n",
        ),
    ]);
    let app = const_id(&program, "App");
    let user_card = const_id(&program, "UserCard");
    assert!(has_static_edge(&program, app, user_card));
}

#[test]
fn options_api_components_shorthand_usage_creates_template_edge() {
    let program = analyze_js(&[
        (
            "App.vue",
            "<template>\n  <user-card/>\n</template>\n<script>\nimport UserCard from './UserCard.vue'\nexport default {\n  components: { UserCard },\n}\n</script>\n",
        ),
        (
            "UserCard.vue",
            "<script>\nexport default { name: 'UserCard' }\n</script>\n",
        ),
    ]);
    let app = const_id(&program, "App");
    let user_card = const_id(&program, "UserCard");
    assert!(has_static_edge(&program, app, user_card));
}

#[test]
fn global_registration_makes_component_available_in_any_template() {
    // `Another.vue` never imports `Widget` itself -- only `main.js`'s
    // `app.component('global-widget', Widget)` call does, and that alone
    // must be enough for `<global-widget>` to resolve project-wide.
    let program = analyze_js(&[
        (
            "main.js",
            "import { createApp } from 'vue'\nimport Widget from './Widget.vue'\nconst app = createApp({})\napp.component('global-widget', Widget)\n",
        ),
        (
            "Widget.vue",
            "<script setup>\nconst x = 1\n</script>\n",
        ),
        (
            "Another.vue",
            "<template>\n  <global-widget/>\n</template>\n<script setup>\nconst y = 2\n</script>\n",
        ),
    ]);
    let another = const_id(&program, "Another");
    let widget = const_id(&program, "Widget");
    assert!(has_static_edge(&program, another, widget));
}

#[test]
fn unmatched_html_tags_create_no_template_edge() {
    let program = analyze_js(&[(
        "App.vue",
        "<template>\n  <div><span/></div>\n</template>\n<script setup>\nconst x = 1\n</script>\n",
    )]);
    let app = const_id(&program, "App");
    // No Const symbol named "Div"/"Span" exists, so there is nothing for
    // these tags to have resolved to -- the app's own component must have
    // no outgoing Static edges at all besides (at most) itself.
    assert!(program
        .references
        .iter()
        .all(|r| !(r.from == app && r.kind == RefKind::Static && r.to != app)));
}

#[test]
fn full_vue_app_template_usage_makes_child_component_reachable_but_orphan_is_bloat() {
    // End-to-end: a root component imported and used by main.js's bootstrap
    // call, which itself imports and uses a child component in its
    // template -- both must be reachable. A third component that's never
    // imported or referenced anywhere must be flagged as bloat.
    let mut program = analyze_js(&[
        (
            "main.js",
            "import { createApp } from 'vue'\nimport App from './App.vue'\ncreateApp(App).mount('#app')\n",
        ),
        (
            "App.vue",
            "<template>\n  <user-card/>\n</template>\n<script setup>\nimport UserCard from './UserCard.vue'\n</script>\n",
        ),
        (
            "UserCard.vue",
            "<template><div/></template>\n<script setup>\nconst label = 'hi'\n</script>\n",
        ),
        (
            "Orphan.vue",
            "<template><div/></template>\n<script setup>\nconst unused = true\n</script>\n",
        ),
    ]);

    let user_card_id = const_id(&program, "UserCard");
    let orphan_id = const_id(&program, "Orphan");

    let model = CostModel::build(&program, CostSource::Estimated);
    let report = analysis::analyze(&mut program, &model);

    assert!(
        program.symbol(user_card_id).unwrap().reachable,
        "UserCard is used in App's template and must be reachable"
    );
    assert!(
        !program.symbol(orphan_id).unwrap().reachable,
        "Orphan.vue is never imported or used anywhere"
    );
    assert!(report.findings.iter().any(|f| f.id == orphan_id));
}

// --- prop/emit mismatch checking ------------------------------------------

#[test]
fn undeclared_prop_and_event_are_flagged() {
    let mismatches = vue_mismatches(&[
        (
            "App.vue",
            "<template>\n  <user-card label=\"hi\" :extra=\"1\" @change=\"f()\" @bogus=\"g()\"/>\n</template>\n<script setup>\nimport UserCard from './UserCard.vue'\n</script>\n",
        ),
        (
            "UserCard.vue",
            "<template><div/></template>\n<script>\nexport default {\n  props: ['label'],\n  emits: ['change'],\n}\n</script>\n",
        ),
    ]);
    assert!(mismatches
        .iter()
        .any(|m| m.binding == "extra" && m.kind == spectograph_js::vue::VueBindingKind::Prop));
    assert!(mismatches
        .iter()
        .any(|m| m.binding == "bogus" && m.kind == spectograph_js::vue::VueBindingKind::Event));
    assert!(!mismatches.iter().any(|m| m.binding == "label"));
    assert!(!mismatches.iter().any(|m| m.binding == "change"));
}

#[test]
fn kebab_case_prop_attribute_matches_camel_case_declared_prop() {
    // `:user-id="1"` must match a `userId` declared prop -- no mismatch.
    let mismatches = vue_mismatches(&[
        (
            "App.vue",
            "<template>\n  <user-card :user-id=\"1\"/>\n</template>\n<script setup>\nimport UserCard from './UserCard.vue'\n</script>\n",
        ),
        (
            "UserCard.vue",
            "<script setup>\nconst props = defineProps({ userId: Number })\n</script>\n",
        ),
    ]);
    assert!(mismatches.is_empty());
}

#[test]
fn component_with_no_declared_props_or_emits_is_not_checked() {
    let mismatches = vue_mismatches(&[
        (
            "App.vue",
            "<template>\n  <user-card :anything=\"1\"/>\n</template>\n<script setup>\nimport UserCard from './UserCard.vue'\n</script>\n",
        ),
        (
            "UserCard.vue",
            "<script setup>\nconst x = 1\n</script>\n",
        ),
    ]);
    assert!(mismatches.is_empty());
}

// --- defineAsyncComponent --------------------------------------------------

#[test]
fn define_async_component_makes_target_template_usable_and_creates_dynamic_edge() {
    let program = analyze_js(&[
        (
            "App.vue",
            "<template>\n  <lazy-widget/>\n</template>\n<script setup>\nimport { defineAsyncComponent } from 'vue'\nconst LazyWidget = defineAsyncComponent(() => import('./Widget.vue'))\n</script>\n",
        ),
        (
            "Widget.vue",
            "<script setup>\nconst x = 1\n</script>\n",
        ),
    ]);
    let app = const_id(&program, "App");
    let widget = const_id(&program, "Widget");
    let lazy_widget = program
        .symbols
        .iter()
        .find(|s| s.name == "LazyWidget" && s.kind == SymbolKind::Const)
        .expect("LazyWidget const symbol");

    // The async binding itself gets a Dynamic edge to the target component.
    assert!(program
        .references
        .iter()
        .any(|r| r.from == lazy_widget.id && r.to == widget && r.kind == RefKind::Dynamic));
    // And the template usage resolves through it as a Static reachability edge.
    assert!(has_static_edge(&program, app, widget));
}

#[test]
fn define_async_component_target_is_reachable_through_template_usage() {
    let mut program = analyze_js(&[
        (
            "main.js",
            "import { createApp } from 'vue'\nimport App from './App.vue'\ncreateApp(App).mount('#app')\n",
        ),
        (
            "App.vue",
            "<template>\n  <lazy-widget/>\n</template>\n<script setup>\nimport { defineAsyncComponent } from 'vue'\nconst LazyWidget = defineAsyncComponent(() => import('./Widget.vue'))\n</script>\n",
        ),
        (
            "Widget.vue",
            "<template><div/></template>\n<script setup>\nconst x = 1\n</script>\n",
        ),
    ]);
    let widget_id = const_id(&program, "Widget");
    let model = CostModel::build(&program, CostSource::Estimated);
    analysis::analyze(&mut program, &model);
    assert!(program.symbol(widget_id).unwrap().reachable);
}

// --- app-mount entrypoint & environment tagging ---------------------------

#[test]
fn app_mount_file_is_marked_as_an_entrypoint() {
    let program = analyze_js(&[(
        "main.js",
        "import { createApp } from 'vue'\nimport App from './App.vue'\ncreateApp(App).mount('#app')\n",
        ),
        (
            "App.vue",
            "<script setup>\nconst x = 1\n</script>\n",
        ),
    ]);
    let module = program
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Module && s.file.ends_with("main.js"))
        .expect("main.js module symbol");
    assert!(program.entrypoints.contains(&module.id));
}

#[test]
fn app_mount_via_local_binding_is_also_detected_as_entrypoint() {
    let program = analyze_js(&[(
        "main.js",
        "import { createApp } from 'vue'\nimport App from './App.vue'\nconst app = createApp(App)\napp.mount('#app')\n",
        ),
        (
            "App.vue",
            "<script setup>\nconst x = 1\n</script>\n",
        ),
    ]);
    let module = program
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Module && s.file.ends_with("main.js"))
        .expect("main.js module symbol");
    assert!(program.entrypoints.contains(&module.id));
}

#[test]
fn app_mount_file_module_symbol_is_tagged_browser_environment() {
    let program = analyze_js(&[(
        "main.js",
        "import { createApp } from 'vue'\nimport App from './App.vue'\ncreateApp(App).mount('#app')\n",
        ),
        (
            "App.vue",
            "<script setup>\nconst x = 1\n</script>\n",
        ),
    ]);
    let module = program
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Module && s.file.ends_with("main.js"))
        .expect("main.js module symbol");
    assert_eq!(
        module.environment,
        spectograph_core::ir::Environment::Browser
    );
}

#[test]
fn file_without_app_mount_is_not_tagged_browser_environment() {
    let program = analyze_js(&[("util.js", "export function helper() {}\n")]);
    let module = program
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Module && s.file.ends_with("util.js"))
        .expect("util.js module symbol");
    assert_eq!(
        module.environment,
        spectograph_core::ir::Environment::Universal
    );
}
