//! Regression tests for the accuracy fixes found by running `check` on real projects
//! (a Next.js app, a Vue 3 app, an ESM TypeScript library): each test is a minimal project
//! reproducing one class of false positive -- or a true positive that must keep being caught.
//!
//! A test names the symbols it expects to be reported as guaranteed bloat; anything else in
//! the project must come back clean.

use tempfile::tempdir;

use spectograph_analyzer::Analyzer;
use spectograph_core::analysis::{self, BloatTier};
use spectograph_core::cost::{CostModel, CostSource};
use spectograph_ts::TypeScriptAnalyzer;

/// Names of every symbol reported as guaranteed bloat in the project made of `files`.
fn dead(files: &[(&str, &str)]) -> Vec<String> {
    let dir = tempdir().unwrap();
    for (name, content) in files {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }
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

fn no_dead(files: &[(&str, &str)]) {
    assert_eq!(dead(files), Vec::<String>::new());
}

#[test]
fn exports_of_a_package_entry_are_its_public_api() {
    no_dead(&[
        ("package.json", r#"{"name": "lib", "main": "dist/index.js"}"#),
        (
            "src/index.ts",
            "export function run(): void {}\nexport const VERSION = '1';\nexport interface Options { a: number }\nexport class Client { go(): void {} }\n",
        ),
    ]);
}

#[test]
fn an_unimported_export_in_an_ordinary_module_is_still_caught() {
    let found = dead(&[
        ("package.json", r#"{"main": "src/index.ts"}"#),
        (
            "src/index.ts",
            "import { used } from './util';\nexport function run(): void { used(); }\n",
        ),
        (
            "src/util.ts",
            "export function used(): void {}\nexport function neverImported(): void {}\n",
        ),
    ]);
    assert_eq!(found, vec!["neverImported"]);
}

#[test]
fn imports_written_with_a_js_extension_resolve_to_ts_sources() {
    // TypeScript's ESM convention: `./util.js` names `util.ts`.
    no_dead(&[
        ("package.json", r#"{"main": "src/index.ts"}"#),
        (
            "src/index.ts",
            "import { used } from './util.js';\nexport function run(): void { used(); }\n",
        ),
        ("src/util.ts", "export function used(): void {}\n"),
    ]);
}

#[test]
fn type_annotations_keep_interfaces_and_aliases_alive() {
    let found = dead(&[
        ("package.json", r#"{"main": "src/index.ts"}"#),
        (
            "src/index.ts",
            "import type { Props, Id } from './types';\nexport function C(p: Props): Id { return String(p.a); }\n",
        ),
        (
            "src/types.ts",
            "export interface Props { a: number }\nexport type Id = string;\nexport type Unused = number;\n",
        ),
    ]);
    assert_eq!(found, vec!["Unused"]);
}

#[test]
fn shorthand_properties_defaults_and_expression_arrows_count_as_uses() {
    no_dead(&[
        ("package.json", r#"{"main": "src/index.ts"}"#),
        (
            "src/index.ts",
            r#"import { a, b, c } from './m';
export const cfg = { a };
export function f(x = b()) { return x; }
export const g = () => c;
"#,
        ),
        (
            "src/m.ts",
            "export const a = 1;\nexport const b = () => 2;\nexport const c = 3;\n",
        ),
    ]);
}

#[test]
fn destructuring_defaults_reference_their_constants() {
    // `{ ttl = DEFAULT_TTL }` binds `ttl`, not `DEFAULT_TTL`.
    no_dead(&[
        ("package.json", r#"{"main": "src/index.ts"}"#),
        (
            "src/index.ts",
            r#"const DEFAULT_TTL = 60;
export function cache({ ttl = DEFAULT_TTL }: { ttl?: number } = {}) { return ttl; }
"#,
        ),
    ]);
}

#[test]
fn extends_and_implements_clauses_are_uses() {
    no_dead(&[
        ("package.json", r#"{"main": "src/index.ts"}"#),
        (
            "src/index.ts",
            "import { Base, Contract } from './base';\nexport class Child extends Base implements Contract { go(): void {} }\n",
        ),
        (
            "src/base.ts",
            "export abstract class Base { abstract go(): void }\nexport interface Contract { go(): void }\n",
        ),
    ]);
}

#[test]
fn member_access_keeps_methods_and_enum_members_alive() {
    let found = dead(&[
        ("package.json", r#"{"main": "src/index.ts"}"#),
        (
            "src/index.ts",
            r#"import { Service, Color } from './m';
export function run() { new Service().work(); return Color.Red; }
"#,
        ),
        (
            "src/m.ts",
            r#"export class Service { work() { return 1; } neverCalled() { return 2; } }
export enum Color { Red, Blue }
"#,
        ),
    ]);
    // `Red` and `work` are accessed; the others are not.
    assert_eq!(found, vec!["Color.Blue", "Service.neverCalled"]);
}

#[test]
fn star_reexport_barrels_expose_their_modules() {
    no_dead(&[
        ("package.json", r#"{"main": "src/index.ts"}"#),
        (
            "src/index.ts",
            "export * from './a';\nexport * from './b';\n",
        ),
        ("src/a.ts", "export function fa(): void {}\n"),
        ("src/b.ts", "export const fb = 1;\n"),
    ]);
}

#[test]
fn namespace_imports_use_every_export_of_the_module() {
    no_dead(&[
        ("package.json", r#"{"main": "src/index.ts"}"#),
        (
            "src/index.ts",
            "import * as utils from './utils';\nexport function run() { return utils.one(); }\n",
        ),
        (
            "src/utils.ts",
            "export function one() { return 1; }\nexport function two() { return 2; }\n",
        ),
    ]);
}

#[test]
fn dynamic_import_with_a_literal_specifier_reaches_the_module() {
    no_dead(&[
        ("package.json", r#"{"main": "src/index.ts"}"#),
        (
            "src/index.ts",
            "export const load = () => import('./page');\n",
        ),
        (
            "src/page.ts",
            "export default function Page() { return helper(); }\nfunction helper() { return 1; }\n",
        ),
    ]);
}

#[test]
fn a_side_effect_import_runs_the_module_and_what_it_calls() {
    no_dead(&[
        ("package.json", r#"{"main": "src/index.ts"}"#),
        ("src/index.ts", "import './setup';\nexport const x = 1;\n"),
        ("src/setup.ts", "function register() {}\nregister();\n"),
    ]);
}

#[test]
fn default_import_of_an_anonymous_default_export_reaches_the_module() {
    no_dead(&[
        ("package.json", r#"{"main": "src/index.ts"}"#),
        (
            "src/index.ts",
            "import config from './config';\nexport const port = config.port;\n",
        ),
        (
            "src/config.ts",
            "const port = 80;\nexport default { port };\n",
        ),
    ]);
}

#[test]
fn next_app_router_route_files_are_entrypoints() {
    let found = dead(&[
        ("package.json", r#"{"dependencies": {"next": "14.0.0"}}"#),
        (
            "app/page.tsx",
            "import { Hero } from '../components/hero';\nexport default function Page() { return <Hero />; }\nexport const metadata = { title: 'x' };\n",
        ),
        (
            "components/hero.tsx",
            "export function Hero() { return <div />; }\n",
        ),
        (
            "components/unused.tsx",
            "export function Unused() { return <div />; }\n",
        ),
    ]);
    assert_eq!(found, vec!["Unused"]);
}

#[test]
fn test_files_and_tool_configs_are_roots_of_usage() {
    no_dead(&[
        ("package.json", r#"{"main": "src/index.ts"}"#),
        ("src/index.ts", "export function run(): void {}\n"),
        ("src/helper.ts", "export function helper(): void {}\n"),
        (
            "src/run.test.ts",
            "import { helper } from './helper';\nhelper();\n",
        ),
    ]);
}

#[test]
fn vite_html_entry_makes_the_module_graph_live() {
    let found = dead(&[
        (
            "index.html",
            r#"<script type="module" src="/src/bootstrap.ts"></script>"#,
        ),
        (
            "src/bootstrap.ts",
            "import { start } from './app';\nstart();\n",
        ),
        ("src/app.ts", "export function start(): void {}\n"),
        ("src/orphan.ts", "export function orphan(): void {}\n"),
    ]);
    assert_eq!(found, vec!["orphan"]);
}

#[test]
fn monorepo_workspace_packages_resolve_to_source() {
    no_dead(&[
        ("package.json", r#"{"name": "root", "private": true}"#),
        (
            "packages/ui/package.json",
            r#"{"name": "@acme/ui", "main": "dist/index.js"}"#,
        ),
        (
            "packages/ui/src/index.ts",
            "export function Button(): void {}\n",
        ),
        (
            "apps/web/package.json",
            r#"{"name": "web", "main": "src/main.ts"}"#,
        ),
        (
            "apps/web/src/main.ts",
            "import { Button } from '@acme/ui';\nButton();\n",
        ),
    ]);
}

#[test]
fn ambient_declaration_files_are_not_reported() {
    no_dead(&[
        ("package.json", r#"{"main": "src/index.ts"}"#),
        ("src/index.ts", "export const x = import.meta.env;\n"),
        (
            "src/global.d.ts",
            "interface ImportMeta { readonly env: Record<string, string> }\n",
        ),
    ]);
}

#[test]
fn constructors_and_members_of_externally_extended_classes_are_used() {
    no_dead(&[
        ("package.json", r#"{"main": "src/index.ts"}"#),
        (
            "src/index.ts",
            r#"import { ClientRequest } from 'node:http';
export class Req extends ClientRequest {
  constructor() { super(); }
  setNoDelay() {}
  _write() {}
}
export class Plain { constructor(public a: number) {} }
"#,
        ),
    ]);
}

#[test]
fn local_variables_shadow_module_symbols() {
    // The inner `helper` is a local; the module-level one is genuinely unused.
    let found = dead(&[
        ("package.json", r#"{"main": "src/index.ts"}"#),
        (
            "src/index.ts",
            r#"function helper() { return 1; }
export function run() {
  const helper = () => 2;
  return helper();
}
"#,
        ),
    ]);
    assert_eq!(found, vec!["helper"]);
}

#[test]
fn vue_slot_templates_do_not_hide_later_component_usage() {
    // `<template #fallback>` nested in the root template used to end it early, hiding
    // `<ArticleComments />` and reporting its import as dead.
    no_dead(&[
        ("package.json", r#"{"dependencies": {"vue": "3"}}"#),
        (
            "index.html",
            r#"<script type="module" src="/src/main.ts"></script>"#,
        ),
        (
            "src/main.ts",
            "import { createApp } from 'vue';\nimport Page from './Page.vue';\ncreateApp(Page).mount('#app');\n",
        ),
        (
            "src/Page.vue",
            r#"<template>
  <Suspense>
    <ArticleBody />
    <template #fallback>loading</template>
  </Suspense>
  <Suspense>
    <ArticleComments />
    <template #fallback>loading</template>
  </Suspense>
  <button @click="onSave">save</button>
</template>
<script setup lang="ts">
import ArticleBody from './ArticleBody.vue'
import ArticleComments from './ArticleComments.vue'
function onSave() {}
</script>
"#,
        ),
        (
            "src/ArticleBody.vue",
            "<template><p>b</p></template>\n<script setup lang=\"ts\">\n</script>\n",
        ),
        (
            "src/ArticleComments.vue",
            "<template><p>c</p></template>\n<script setup lang=\"ts\">\n</script>\n",
        ),
    ]);
}

#[test]
fn template_only_script_bindings_are_used_but_unreferenced_ones_are_not() {
    let found = dead(&[
        ("package.json", r#"{"dependencies": {"vue": "3"}}"#),
        (
            "index.html",
            r#"<script type="module" src="/src/main.ts"></script>"#,
        ),
        (
            "src/main.ts",
            "import { createApp } from 'vue';\nimport Page from './Page.vue';\ncreateApp(Page).mount('#app');\n",
        ),
        (
            "src/Page.vue",
            r#"<template><button @click="onSave">{{ label }}</button></template>
<script setup lang="ts">
const label = 'save'
function onSave() {}
function neverUsedAnywhere() {}
</script>
"#,
        ),
    ]);
    assert_eq!(found, vec!["neverUsedAnywhere"]);
}
