//! Tests for Vue SFC (`.vue`) support on the TypeScript side: `.vue` files
//! with a `lang="ts"` script block are discovered, materialized at their
//! true line positions, and flow through the ordinary TS parse/resolve
//! pipeline -- including `.vue` module resolution, Vue component detection
//! (Options API and `<script setup>`, including TypeScript's generic
//! `defineProps<T>()`/`defineEmits<T>()` form), template-usage reachability
//! edges, prop/emit mismatch checking, and `defineAsyncComponent`
//! lazy-loading edges. Mirrors `spectograph-js/tests/vue.rs`'s
//! plain-JavaScript coverage.

use tempfile::tempdir;

use spectograph_analyzer::{AnalyzeError, Analyzer};
use spectograph_core::analysis;
use spectograph_core::cost::{CostModel, CostSource};
use spectograph_core::ir::{Program, RefKind, SymbolKind};
use spectograph_js::VueAnalysis;
use spectograph_ts::TypeScriptAnalyzer;

fn analyze_ts(files: &[(&str, &str)]) -> Program {
    let dir = tempdir().unwrap();
    for (name, content) in files {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }
    TypeScriptAnalyzer.analyze(dir.path()).unwrap()
}

fn analyze_vue(files: &[(&str, &str)]) -> (Program, VueAnalysis) {
    let dir = tempdir().unwrap();
    for (name, content) in files {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }
    TypeScriptAnalyzer.analyze_vue(dir.path()).unwrap()
}

fn vue_mismatches(files: &[(&str, &str)]) -> Vec<spectograph_js::vue::VueBindingMismatch> {
    let dir = tempdir().unwrap();
    for (name, content) in files {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }
    TypeScriptAnalyzer
        .analyze_vue_mismatches(dir.path())
        .unwrap()
        .1
}

#[test]
fn vue_file_with_typescript_script_setup_is_parsed_with_correct_line_numbers() {
    let program = analyze_ts(&[(
        "App.vue",
        "<template>\n  <div>{{ count }}</div>\n</template>\n\n<script setup lang=\"ts\">\nfunction double(n: number): number {\n  return n * 2\n}\n</script>\n",
    )]);

    let double = program
        .symbols
        .iter()
        .find(|s| s.name == "double" && s.kind == SymbolKind::Function)
        .expect("double function symbol");
    assert!(double.file.to_string_lossy().ends_with("App.vue"));
    // `function double(...)` is on line 6 of the source file above.
    assert_eq!(double.span.start_line, 6);
}

#[test]
fn vue_file_with_combined_script_and_script_setup_parses_both_blocks() {
    let program = analyze_ts(&[(
        "Widget.vue",
        "<script lang=\"ts\">\nexport const NAME = 'Widget'\n</script>\n\n<script setup lang=\"ts\">\nconst count: number = 1\n</script>\n",
    )]);

    assert!(program
        .symbols
        .iter()
        .any(|s| s.name == "NAME" && s.kind == SymbolKind::Const));
    assert!(program
        .symbols
        .iter()
        .any(|s| s.name == "count" && s.kind == SymbolKind::Const));
}

#[test]
fn vue_to_vue_import_resolves_with_lang_ts() {
    let program = analyze_ts(&[
        (
            "math.vue",
            "<script lang=\"ts\">\nexport function double(n: number): number {\n  return n * 2\n}\n</script>\n",
        ),
        (
            "main.ts",
            "import { double } from './math.vue'\ndouble(1)\n",
        ),
    ]);

    let double = program
        .symbols
        .iter()
        .find(|s| s.name == "double" && s.kind == SymbolKind::Function)
        .expect("double function symbol");
    let import = program
        .symbols
        .iter()
        .find(|s| s.name == "double" && s.kind == SymbolKind::Import)
        .expect("double import symbol");
    assert!(program
        .references
        .iter()
        .any(|r| r.from == import.id && r.to == double.id && r.kind == RefKind::ReExport));
}

#[test]
fn vue_file_with_untyped_script_is_not_picked_up_by_ts_analyzer() {
    // A plain `<script>` (no `lang="ts"`) block belongs to the JavaScript
    // analyzer (see `spectograph_js`'s equivalent positive-case test); with
    // no other TS-family source in the project, the TS analyzer must find
    // nothing to parse rather than silently swallow the untyped content.
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("App.vue"),
        "<script>\nconst x = 1\n</script>\n",
    )
    .unwrap();

    let result = TypeScriptAnalyzer.analyze(dir.path());
    assert!(matches!(result, Err(AnalyzeError::NoSourceFiles(_))));
}

// --- component detection -------------------------------------------------

#[test]
fn options_api_component_with_typed_props_is_detected() {
    let (_program, vue) = analyze_vue(&[(
        "Widget.vue",
        "<script lang=\"ts\">\nexport default {\n  name: 'MyWidget',\n  props: ['label', 'count'],\n  emits: ['change'],\n}\n</script>\n",
    )]);

    assert_eq!(vue.components.len(), 1);
    let c = &vue.components[0];
    assert_eq!(c.name, "MyWidget");
    assert_eq!(c.declared_props, vec!["label", "count"]);
    assert_eq!(c.declared_emits, vec!["change"]);
}

#[test]
fn script_setup_component_with_generic_define_props_and_define_emits_is_detected() {
    let (_program, vue) = analyze_vue(&[(
        "Counter.vue",
        "<script setup lang=\"ts\">\nconst props = defineProps<{ start: number; label?: string }>()\nconst emit = defineEmits<{ increment: []; reset: [] }>()\n</script>\n",
    )]);

    assert_eq!(vue.components.len(), 1);
    let c = &vue.components[0];
    assert_eq!(c.name, "Counter");
    assert_eq!(c.declared_props, vec!["start", "label"]);
    assert_eq!(c.declared_emits, vec!["increment", "reset"]);
}

#[test]
fn script_setup_component_with_runtime_define_props_is_still_detected_in_ts() {
    // The plain-JS runtime-argument form must keep working under the TS
    // grammar too, not just the generic-type form.
    let (_program, vue) = analyze_vue(&[(
        "Simple.vue",
        "<script setup lang=\"ts\">\nconst props = defineProps({ label: String })\n</script>\n",
    )]);

    assert_eq!(vue.components.len(), 1);
    assert_eq!(vue.components[0].declared_props, vec!["label"]);
}

#[test]
fn plain_ts_file_without_vue_extension_never_produces_a_vue_component() {
    let (_program, vue) = analyze_vue(&[(
        "config.ts",
        "export default {\n  name: 'not-a-component',\n  props: ['a', 'b'],\n}\n",
    )]);
    assert!(vue.components.is_empty());
}

#[test]
fn vue_component_symbol_participates_in_the_ordinary_symbol_graph() {
    let program = analyze_ts(&[(
        "Widget.vue",
        "<script lang=\"ts\">\nexport default {\n  name: 'Widget',\n}\n</script>\n",
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
    let program = analyze_ts(&[
        (
            "App.vue",
            "<template>\n  <user-card/>\n</template>\n<script setup lang=\"ts\">\nimport UserCard from './UserCard.vue'\n</script>\n",
        ),
        (
            "UserCard.vue",
            "<template><div/></template>\n<script setup lang=\"ts\">\nconst label = 'hi'\n</script>\n",
        ),
    ]);
    let app = const_id(&program, "App");
    let user_card = const_id(&program, "UserCard");
    assert!(has_static_edge(&program, app, user_card));
}

#[test]
fn dynamic_is_binding_creates_template_edge() {
    let program = analyze_ts(&[
        (
            "App.vue",
            "<template>\n  <component :is=\"UserCard\"/>\n</template>\n<script setup lang=\"ts\">\nimport UserCard from './UserCard.vue'\n</script>\n",
        ),
        (
            "UserCard.vue",
            "<template><div/></template>\n<script setup lang=\"ts\">\nconst label = 'hi'\n</script>\n",
        ),
    ]);
    let app = const_id(&program, "App");
    let user_card = const_id(&program, "UserCard");
    assert!(has_static_edge(&program, app, user_card));
}

#[test]
fn options_api_components_shorthand_usage_creates_template_edge() {
    let program = analyze_ts(&[
        (
            "App.vue",
            "<template>\n  <user-card/>\n</template>\n<script lang=\"ts\">\nimport UserCard from './UserCard.vue'\nexport default {\n  components: { UserCard },\n}\n</script>\n",
        ),
        (
            "UserCard.vue",
            "<script lang=\"ts\">\nexport default { name: 'UserCard' }\n</script>\n",
        ),
    ]);
    let app = const_id(&program, "App");
    let user_card = const_id(&program, "UserCard");
    assert!(has_static_edge(&program, app, user_card));
}

#[test]
fn global_registration_makes_component_available_in_any_template() {
    let program = analyze_ts(&[
        (
            "main.ts",
            "import { createApp } from 'vue'\nimport Widget from './Widget.vue'\nconst app = createApp({})\napp.component('global-widget', Widget)\n",
        ),
        (
            "Widget.vue",
            "<script setup lang=\"ts\">\nconst x = 1\n</script>\n",
        ),
        (
            "Another.vue",
            "<template>\n  <global-widget/>\n</template>\n<script setup lang=\"ts\">\nconst y = 2\n</script>\n",
        ),
    ]);
    let another = const_id(&program, "Another");
    let widget = const_id(&program, "Widget");
    assert!(has_static_edge(&program, another, widget));
}

#[test]
fn full_vue_app_template_usage_makes_child_component_reachable_but_orphan_is_bloat() {
    let mut program = analyze_ts(&[
        (
            "main.ts",
            "import { createApp } from 'vue'\nimport App from './App.vue'\ncreateApp(App).mount('#app')\n",
        ),
        (
            "App.vue",
            "<template>\n  <user-card/>\n</template>\n<script setup lang=\"ts\">\nimport UserCard from './UserCard.vue'\n</script>\n",
        ),
        (
            "UserCard.vue",
            "<template><div/></template>\n<script setup lang=\"ts\">\nconst label = 'hi'\n</script>\n",
        ),
        (
            "Orphan.vue",
            "<template><div/></template>\n<script setup lang=\"ts\">\nconst unused = true\n</script>\n",
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
            "<template>\n  <user-card label=\"hi\" :extra=\"1\" @change=\"f()\" @bogus=\"g()\"/>\n</template>\n<script setup lang=\"ts\">\nimport UserCard from './UserCard.vue'\n</script>\n",
        ),
        (
            "UserCard.vue",
            "<script lang=\"ts\">\nexport default {\n  props: ['label'],\n  emits: ['change'],\n}\n</script>\n",
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
    let mismatches = vue_mismatches(&[
        (
            "App.vue",
            "<template>\n  <user-card :user-id=\"1\"/>\n</template>\n<script setup lang=\"ts\">\nimport UserCard from './UserCard.vue'\n</script>\n",
        ),
        (
            "UserCard.vue",
            "<script setup lang=\"ts\">\nconst props = defineProps<{ userId: number }>()\n</script>\n",
        ),
    ]);
    assert!(mismatches.is_empty());
}

// --- defineAsyncComponent --------------------------------------------------

#[test]
fn define_async_component_makes_target_template_usable_and_creates_dynamic_edge() {
    let program = analyze_ts(&[
        (
            "App.vue",
            "<template>\n  <lazy-widget/>\n</template>\n<script setup lang=\"ts\">\nimport { defineAsyncComponent } from 'vue'\nconst LazyWidget = defineAsyncComponent(() => import('./Widget.vue'))\n</script>\n",
        ),
        (
            "Widget.vue",
            "<script setup lang=\"ts\">\nconst x = 1\n</script>\n",
        ),
    ]);
    let app = const_id(&program, "App");
    let widget = const_id(&program, "Widget");
    let lazy_widget = program
        .symbols
        .iter()
        .find(|s| s.name == "LazyWidget" && s.kind == SymbolKind::Const)
        .expect("LazyWidget const symbol");

    assert!(program
        .references
        .iter()
        .any(|r| r.from == lazy_widget.id && r.to == widget && r.kind == RefKind::Dynamic));
    assert!(has_static_edge(&program, app, widget));
}

#[test]
fn define_async_component_target_is_reachable_through_template_usage() {
    let mut program = analyze_ts(&[
        (
            "main.ts",
            "import { createApp } from 'vue'\nimport App from './App.vue'\ncreateApp(App).mount('#app')\n",
        ),
        (
            "App.vue",
            "<template>\n  <lazy-widget/>\n</template>\n<script setup lang=\"ts\">\nimport { defineAsyncComponent } from 'vue'\nconst LazyWidget = defineAsyncComponent(() => import('./Widget.vue'))\n</script>\n",
        ),
        (
            "Widget.vue",
            "<template><div/></template>\n<script setup lang=\"ts\">\nconst x = 1\n</script>\n",
        ),
    ]);
    let widget_id = const_id(&program, "Widget");
    let model = CostModel::build(&program, CostSource::Estimated);
    analysis::analyze(&mut program, &model);
    assert!(program.symbol(widget_id).unwrap().reachable);
}

// --- app-mount entrypoint & environment tagging ---------------------------

#[test]
fn app_mount_file_module_symbol_is_tagged_browser_environment() {
    let program = analyze_ts(&[
        (
            "main.ts",
            "import { createApp } from 'vue'\nimport App from './App.vue'\ncreateApp(App).mount('#app')\n",
        ),
        ("App.vue", "<script setup lang=\"ts\">\nconst x = 1\n</script>\n"),
    ]);
    let module = program
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Module && s.file.ends_with("main.ts"))
        .expect("main.ts module symbol");
    assert_eq!(
        module.environment,
        spectograph_core::ir::Environment::Browser
    );
}

#[test]
fn file_without_app_mount_is_not_tagged_browser_environment() {
    let program = analyze_ts(&[("util.ts", "export function helper(): void {}\n")]);
    let module = program
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Module && s.file.ends_with("util.ts"))
        .expect("util.ts module symbol");
    assert_eq!(
        module.environment,
        spectograph_core::ir::Environment::Universal
    );
}

#[test]
fn non_conventionally_named_app_mount_file_becomes_an_entrypoint() {
    // TS's own entrypoint detection is far more restrictive than JS's
    // permissive "every module" fallback: without an app-mount signal, a
    // bootstrap file that's neither `package.json`'s `main` nor one of the
    // conventional `index.ts`/`main.ts` names would never be treated as an
    // entrypoint, and everything only reachable through it -- the whole
    // app -- would look like 100% bloat. `package.json`'s `main` here
    // points at an unrelated library entry, and the real app bootstrap is
    // named `bootstrap.ts`, matching neither convention.
    let mut program = analyze_ts(&[
        (
            "package.json",
            r#"{"main": "src/lib.ts"}"#,
        ),
        ("src/lib.ts", "export function publicApi(): void {}\n"),
        (
            "bootstrap.ts",
            "import { createApp } from 'vue'\nimport App from './App.vue'\ncreateApp(App).mount('#app')\n",
        ),
        (
            "App.vue",
            "<template>\n  <user-card/>\n</template>\n<script setup lang=\"ts\">\nimport UserCard from './UserCard.vue'\n</script>\n",
        ),
        (
            "UserCard.vue",
            "<template><div/></template>\n<script setup lang=\"ts\">\nconst label = 'hi'\n</script>\n",
        ),
    ]);

    let bootstrap_module = program
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Module && s.file.ends_with("bootstrap.ts"))
        .expect("bootstrap.ts module symbol");
    assert!(
        program.entrypoints.contains(&bootstrap_module.id),
        "bootstrap.ts must be an entrypoint despite matching no filename/package.json convention"
    );

    let user_card_id = const_id(&program, "UserCard");
    let model = CostModel::build(&program, CostSource::Estimated);
    analysis::analyze(&mut program, &model);
    assert!(
        program.symbol(user_card_id).unwrap().reachable,
        "UserCard is only reachable via bootstrap.ts's app-mount entrypoint"
    );
}
