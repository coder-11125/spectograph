//! Vue support: splitting a Single-File Component (`.vue`) into its
//! top-level blocks and materializing a parseable script from them,
//! detecting Options-API and `<script setup>` components (props/emits),
//! matching template usage to components for reachability edges and
//! prop/emit mismatch checking, and recognizing the `createApp(...).mount(
//! ...)` bootstrap convention for entrypoint detection.
//!
//! A `.vue` file is an XML-ish container -- `<template>`, `<script>`/
//! `<script setup>`, `<style>` -- not valid JS/TS on its own, so nothing in
//! [`crate::parse`] (or `spectograph_ts::parse`) can run against it directly.
//! [`parse_sfc`]/[`materialize_script`] are the split step that makes the
//! rest of the pipeline apply unchanged: they extract the script block(s)
//! as a single source string positioned at their true line numbers in the
//! original file (padded with blank lines everywhere else), so tree-sitter
//! spans over the result equal real `.vue`-file line numbers. Detection
//! itself (component/global-registration/async-component/app-mount) lives
//! in each of `spectograph_js::parse` and `spectograph_ts::parse` where it
//! needs the tree-sitter AST shapes those grammars produce, except where a
//! shape is identical between the two grammars (template scanning, the
//! app-mount check below) -- those live here, shared.
//!
//! ## Coverage beyond components and templates
//!
//! Handled elsewhere, each with tests in `tests/vue_support.rs` of both analyzer crates:
//!
//! - **Routing:** route components (`component: Home`, lazy `() => import('./X.vue')`,
//!   `children`, named `components: {...}`) need no special handling -- they are ordinary
//!   imports and `import()` calls. File-based routing plugins (`vite-plugin-pages`,
//!   `unplugin-vue-router`, `vite-plugin-vue-layouts`) make `src/pages/**` and
//!   `src/layouts/**` entrypoints (`entrypoints.rs`); `import.meta.glob` and
//!   `require.context` load every matched module (`globs.rs`).
//! - **Vue 2:** `Vue.extend({...})`/`defineComponent({...})` default exports are components;
//!   `new Vue({ el })` and `.$mount()` mark a bootstrap file; mixins and filters are ordinary
//!   references; `.native`/`.sync` and other directive modifiers are understood.
//! - **Stores (Pinia/Vuex):** a store is an ordinary module, so its reachability already
//!   follows from imports and calls; nothing store-specific is modelled.
//! - **`<component :is="expr">`:** the template word-matching in `template_bindings.rs` keeps
//!   every identifier in any expression alive.
//! - **Options API `components: { Tag: Ident }`** remaps are resolved for prop/emit checks.
//! - **`defineProps<T>()` / `defineEmits<T>()`:** inline object types, call-signature and
//!   function-type emits, same-file interfaces (without `extends`) and `withDefaults(...)`.
//! - **Scoped CSS:** unused classes in `<style scoped>` are a diagnostic (`vue_css.rs`).
//! - **`resolveComponent('name')`** / `resolveDynamicComponent('name')` in a script count as a
//!   template tag for that name (`resolve_component_tags`).
//! - **`defineModel()`** declares its prop and `update:<name>` event.
//! - **Fallthrough attributes:** prop/emit mismatches are only reported against a component
//!   whose template root is a plain element, and never for `aria-*`/`data-*`/`id`/... or native
//!   DOM events; a component or `<slot>`/`<component>` root forwards them to something unseen.
//!
//! ## Known limitations
//!
//! Deliberately out of scope, rather than partially/unreliably supported:
//!
//! - **Class-style components** (`vue-class-component`, `vue-property-decorator`:
//!   `export default class X extends Vue`) are not detected as components, so their
//!   props/emits aren't checked; reachability still works through the whole-module use of a
//!   default import.
//! - **Mixins/`extends`:** a component using them has *unknown* props and emits (they may come
//!   from the mixin), so bindings passed to it are never reported as undeclared.
//! - **`defineProps<T>()`/`defineEmits<T>()` where `T` is imported, intersected, or
//!   `extends` another interface** yields no declared bindings (unknown), not partial ones.
//! - **Render-function string lookups other than `resolveComponent`/`resolveDynamicComponent`**
//!   (e.g. `h('name')` against a global registration) aren't matched to a component.
//! - **Unused-CSS detection is conservative:** it skips any file with a non-enumerable class
//!   binding (`:class="cls"`, ternaries, computed keys, `v-bind="obj"`), SCSS/Less class
//!   concatenation (`&-x`) or interpolation, and `:deep`/`:global`/`:slotted` selectors, and
//!   only inspects `<style scoped>` in CSS/SCSS/Less. A class used anywhere in the project's
//!   templates, or named in the component's script, counts as used.
//! - **Plugin folder options** (`routesFolder`, custom `dirs`) for the file-routing plugins are
//!   not read; only the default `pages`/`layouts` folders are roots.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use spectograph_core::ir::SymbolId;

/// A Vue component detected in a `.vue` file's script -- either the Options
/// API's `export default { ... }` object, or the implicit component a
/// `<script setup>` block *is*. Layered on top of the regular symbol/
/// reference graph the same way `react::ReactComponent` is: `symbol_id`
/// points at an ordinary `Const` symbol (existing reachability/bloat
/// analysis applies to it unchanged), and this struct just adds the
/// Vue-specific facts -- declared props/emits -- that the shared IR has no
/// vocabulary for. Detection itself lives in each of `spectograph_js::parse`
/// and `spectograph_ts::parse` (not here), since it needs the tree-sitter
/// AST shapes those grammars produce -- the same JS-vs-TS split as
/// `detect_react` in each crate.
#[derive(Debug, Clone)]
pub struct VueComponent {
    pub symbol_id: SymbolId,
    pub name: String,
    pub file: PathBuf,
    pub line: usize,
    /// Prop names declared via Options API `props: [...]`/`props: {...}`, or
    /// `<script setup>`'s `defineProps(...)`/`defineProps<T>()`. Empty means
    /// "nothing statically enumerable" (e.g. `props` referencing an external
    /// variable), not "declares no props" -- same convention as
    /// `react::ReactComponent::declared_props`.
    pub declared_props: Vec<String>,
    /// Event names declared via Options API `emits: [...]`/`emits: {...}`,
    /// or `<script setup>`'s `defineEmits(...)`/`defineEmits<T>()`. Same
    /// "empty means unknown, not none" convention as `declared_props`.
    pub declared_emits: Vec<String>,
    /// Options API `components: { Tag: LocalIdent }` entries as `(tag, local identifier)`,
    /// including the shorthand `{ LocalIdent }` (tag == identifier). The tag is the name a
    /// template uses, which differs from the import's local name when the key is remapped.
    pub registered_components: Vec<(String, String)>,
}

/// The Options API object behind a default export's value: the literal itself
/// (`export default { ... }`), or the argument of the wrappers that exist only for typing and
/// Vue 2 subclassing -- `defineComponent({ ... })`, `defineNuxtComponent({ ... })`,
/// `Vue.extend({ ... })`. `None` for anything else (a class, an identifier, a call to
/// something unrelated), which can't be enumerated.
pub fn component_options_object<'a>(
    value: tree_sitter::Node<'a>,
    src: &[u8],
) -> Option<tree_sitter::Node<'a>> {
    match value.kind() {
        "object" => Some(value),
        "call_expression" => {
            let func = value.child_by_field_name("function")?;
            let name = match func.kind() {
                "identifier" => func.utf8_text(src).ok()?,
                "member_expression" => func.child_by_field_name("property")?.utf8_text(src).ok()?,
                _ => return None,
            };
            if !matches!(name, "defineComponent" | "defineNuxtComponent" | "extend") {
                return None;
            }
            value
                .child_by_field_name("arguments")?
                .named_child(0)
                .filter(|a| a.kind() == "object")
        }
        _ => None,
    }
}

/// `(tag, identifier)` pairs from an Options API `components: { ... }` object: `{ Card }`,
/// `{ FancyCard: Card }`, `{ 'fancy-card': Card }`. Entries whose value isn't a bare
/// identifier (an inline definition, a call) are skipped.
pub fn extract_component_registrations(
    node: tree_sitter::Node,
    src: &[u8],
) -> Vec<(String, String)> {
    let text = |n: tree_sitter::Node| n.utf8_text(src).unwrap_or("").trim().to_string();
    let mut out = Vec::new();
    if node.kind() != "object" {
        return out;
    }
    let mut c = node.walk();
    for prop in node.named_children(&mut c) {
        match prop.kind() {
            "shorthand_property_identifier" => {
                let name = text(prop);
                out.push((name.clone(), name));
            }
            "pair" => {
                let (Some(key), Some(value)) = (
                    prop.child_by_field_name("key"),
                    prop.child_by_field_name("value"),
                ) else {
                    continue;
                };
                if value.kind() != "identifier" {
                    continue;
                }
                let tag = if key.kind() == "string" {
                    key.named_child(0).map(text).unwrap_or_default()
                } else {
                    text(key)
                };
                out.push((tag, text(value)));
            }
            _ => {}
        }
    }
    out
}

/// The component name Vue's own tooling (devtools, warnings) falls back to
/// when a component declares no explicit `name:`/`defineOptions({ name })`:
/// the `.vue` file's own base name, e.g. `components/UserCard.vue` ->
/// `"UserCard"`. Used as the default here for the same reason, and because
/// it's also what makes `import UserCard from './UserCard.vue'` resolve
/// through the analyzers' default-import name-matching fallback -- that's
/// the overwhelmingly common import-alias convention for SFCs.
pub fn component_name_from_path(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "default".to_string())
}

/// Extract plain names from a Vue `props`/`emits` value, which Vue accepts
/// in two equivalent runtime forms: an array of string literals (`['foo',
/// 'bar']`) or an object whose keys are the names (`{ foo: String, bar: {
/// type: Number, default: 0 } }`). Returns empty for any other shape (a
/// bare identifier referencing an external array, a spread, ...) -- can't
/// statically enumerate those, same "don't guess" outcome as an
/// un-destructured React `props` parameter.
pub fn extract_names_from_array_or_object(node: tree_sitter::Node, src: &[u8]) -> Vec<String> {
    let text = |n: tree_sitter::Node| n.utf8_text(src).unwrap_or("").trim().to_string();
    match node.kind() {
        "array" => {
            let mut c = node.walk();
            node.named_children(&mut c)
                .filter(|el| el.kind() == "string")
                .filter_map(|el| el.named_child(0))
                .map(text)
                .collect()
        }
        "object" => {
            let mut c = node.walk();
            node.named_children(&mut c)
                .filter_map(|prop| match prop.kind() {
                    "pair" => prop.child_by_field_name("key").map(|k| {
                        if k.kind() == "string" {
                            k.named_child(0).map(text).unwrap_or_default()
                        } else {
                            text(k)
                        }
                    }),
                    "shorthand_property_identifier" => Some(text(prop)),
                    _ => None,
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

/// Which language a `<script>` block's content is written in, from its `lang`
/// attribute (`lang="ts"` / `lang="tsx"` -> TypeScript; anything else,
/// including no `lang` attribute at all, -> JavaScript).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptLang {
    JavaScript,
    TypeScript,
}

/// One `<script>` or `<script setup>` block: its content, plus enough
/// position info to map any span found within it back to the original
/// `.vue` file.
#[derive(Debug, Clone)]
pub struct ScriptBlock {
    pub content: String,
    pub lang: ScriptLang,
    pub setup: bool,
    /// 1-based line, in the original file, of the opening tag
    /// (`<script>`/`<script setup ...>`) itself -- not of `content`'s first
    /// full line. `content` starts right after that tag's `>`, so its own
    /// line 0 is whatever (usually nothing) follows the tag on that same
    /// source line, and `content`'s line *1* is the original file's line
    /// `start_line + 1`. This is deliberate, not an off-by-one: it's exactly
    /// the alignment [`materialize_script`] needs to place `content`'s lines
    /// back at their true position with a single `lines[start_line - 1 + i]`
    /// write, with no separate adjustment for the tag's own line.
    pub start_line: usize,
}

/// The `<template>` block: its raw markup, plus its position in the original
/// file (for future template-usage scanning -- see `spectograph_ts::angular`
/// for the analogous Angular pass this will mirror). `start_line` follows
/// the same "line of the opening tag" convention as [`ScriptBlock::start_line`].
#[derive(Debug, Clone)]
pub struct TemplateBlock {
    pub content: String,
    pub start_line: usize,
}

/// A `.vue` file split into its top-level blocks. `script` and
/// `script_setup` are both populated when the SFC combines them (Vue allows
/// a plain `<script>` for `export default { name: ... }`-style metadata
/// alongside a `<script setup>` for the component body) -- see
/// [`materialize_script`] for how the two get combined for parsing.
/// `<style>` blocks aren't tracked: nothing downstream reads them yet.
#[derive(Debug, Clone, Default)]
pub struct SfcFile {
    pub template: Option<TemplateBlock>,
    pub script: Option<ScriptBlock>,
    pub script_setup: Option<ScriptBlock>,
}

/// Matches a `<template ...>` opening tag. Only the opening tag is a regex:
/// where the block *ends* depends on nesting (`<template #fallback>` /
/// `<template v-if>` inside the root template), which a regex can't count --
/// see [`find_template_close`].
fn template_open_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"<template((?:\s+[^>]*)?)>").unwrap())
}

/// Byte offset of the `</template>` that closes the root `<template>` whose body starts at
/// `body_start`, counting nested `<template ...>` elements (slots, `v-if`/`v-for`
/// wrappers) so the first inner `</template>` doesn't end the block early. Self-closing
/// `<template ... />` doesn't open a level. `None` if the block is never closed.
fn find_template_close(src: &str, body_start: usize) -> Option<usize> {
    let mut depth = 1usize;
    let mut i = body_start;
    while let Some(rel) = src[i..].find('<') {
        let at = i + rel;
        let rest = &src[at..];
        if let Some(after) = rest.strip_prefix("</template") {
            if after.starts_with(|c: char| c == '>' || c.is_whitespace()) {
                depth -= 1;
                if depth == 0 {
                    return Some(at);
                }
            }
            i = at + "</template".len();
        } else if let Some(after) = rest.strip_prefix("<template") {
            if after.starts_with(|c: char| c == '>' || c == '/' || c.is_whitespace()) {
                // Find this tag's end and see whether it self-closes.
                let tag_end = rest.find('>').map(|e| at + e)?;
                if !src[..tag_end].ends_with('/') {
                    depth += 1;
                }
                i = tag_end + 1;
            } else {
                i = at + "<template".len();
            }
        } else {
            i = at + 1;
        }
    }
    None
}

/// Matches one `<script ...>...</script>` block. Non-greedy body
/// capture, `(?s)` so `.` crosses lines. Script blocks don't nest, so a
/// straightforward "up to the first matching close tag" match is sufficient
/// -- same "good enough for the actual shape in practice" tradeoff as
/// `spectograph_ts::angular`'s regex-based template scanner, rather than
/// pulling in a full HTML/XML parser.
fn script_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<script((?:\s+[^>]*)?)>(.*?)</script>").unwrap())
}

fn lang_attr_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"\blang\s*=\s*["']([\w-]+)["']"#).unwrap())
}

fn setup_attr_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // Bare `setup` attribute, or `setup="..."` (Vue never gives it a value,
    // but tolerate one rather than mis-parse). Boundary on both sides so it
    // doesn't match inside another attribute's name (e.g. a hypothetical
    // `data-setup`).
    RE.get_or_init(|| Regex::new(r#"(?:^|\s)setup(?:\s|=|$)"#).unwrap())
}

/// 1-based line number of the first character at byte offset `at` in `src`.
fn line_at(src: &str, at: usize) -> usize {
    1 + src.as_bytes()[..at].iter().filter(|&&b| b == b'\n').count()
}

/// Parse a `.vue` file's source into its top-level `<template>`/`<script>`/
/// `<script setup>` blocks. Blocks not present in the source are `None`; a
/// malformed or empty file just yields an all-`None` [`SfcFile`] rather than
/// an error -- callers already treat "nothing to parse" as a normal,
/// unexceptional outcome (mirrors `resolve_file_candidate`'s `Option`-based
/// "not found" convention elsewhere in these analyzers).
pub fn parse_sfc(src: &str) -> SfcFile {
    let mut sfc = SfcFile::default();

    if let Some(open) = template_open_re().find(src) {
        let body_start = open.end();
        if let Some(body_end) = find_template_close(src, body_start) {
            sfc.template = Some(TemplateBlock {
                content: src[body_start..body_end].to_string(),
                start_line: line_at(src, body_start),
            });
        }
    }

    for caps in script_re().captures_iter(src) {
        let attrs = &caps[1];
        let body = caps.get(2).unwrap();
        let lang = match lang_attr_re()
            .captures(attrs)
            .map(|c| c[1].to_ascii_lowercase())
        {
            Some(l) if l == "ts" || l == "tsx" => ScriptLang::TypeScript,
            _ => ScriptLang::JavaScript,
        };
        let setup = setup_attr_re().is_match(attrs);
        let block = ScriptBlock {
            content: body.as_str().to_string(),
            lang,
            setup,
            start_line: line_at(src, body.start()),
        };
        if setup {
            sfc.script_setup = Some(block);
        } else {
            sfc.script = Some(block);
        }
    }

    sfc
}

/// Combine a `.vue` file's `<script>` and `<script setup>` blocks (whichever
/// are present) into a single parseable source string, with each block's
/// lines placed at their true position in the original file and every other
/// line left blank. Feeding the result to the ordinary tree-sitter
/// `parse_file` (in either `spectograph_js::parse` or `spectograph_ts::parse`,
/// chosen by the returned [`ScriptLang`]) therefore produces spans that
/// equal real line numbers in the `.vue` file, with no changes needed to
/// that parser. Returns `None` if the file has neither block.
///
/// When both blocks are present, the returned language is TypeScript if
/// either block is (a `<script lang="ts">` alongside a plain
/// `<script setup>`, or vice versa, still needs the TS grammar to parse
/// without errors).
pub fn materialize_script(sfc: &SfcFile) -> Option<(String, ScriptLang)> {
    let blocks: Vec<&ScriptBlock> = [&sfc.script, &sfc.script_setup]
        .into_iter()
        .flatten()
        .collect();
    if blocks.is_empty() {
        return None;
    }
    let lang = if blocks.iter().any(|b| b.lang == ScriptLang::TypeScript) {
        ScriptLang::TypeScript
    } else {
        ScriptLang::JavaScript
    };

    let last_line = blocks
        .iter()
        .map(|b| b.start_line + b.content.lines().count().saturating_sub(1))
        .max()
        .unwrap_or(0);
    let mut lines = vec![String::new(); last_line];
    for b in &blocks {
        for (i, line) in b.content.lines().enumerate() {
            lines[b.start_line - 1 + i] = line.to_string();
        }
    }
    Some((lines.join("\n"), lang))
}

/// Discover `.vue` files under `root`, respecting `.gitignore` -- the same
/// walk as [`spectograph_analyzer::discover::discover_files`], but not
/// driven by [`spectograph_core::ir::Language::extensions`]: a `.vue`
/// file's *script block* decides whether its content is JavaScript or
/// TypeScript (see [`ScriptLang`]), not its file extension, so it can't be
/// owned by one `Language` the way `.jsx` or `.tsx` are. Callers pick which
/// of these files to actually process based on `materialize_script`'s
/// returned language.
pub fn discover_vue_files(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .build();
    for entry in walker {
        let entry = entry.map_err(|e| std::io::Error::other(format!("walk error: {e}")))?;
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("vue") {
            files.push(path.to_path_buf());
        }
    }
    files.sort();
    Ok(files)
}

fn node_text<'a>(node: tree_sitter::Node, src: &'a str) -> &'a str {
    node.utf8_text(src.as_bytes()).unwrap_or("").trim()
}

/// True if `root` contains a Vue app-mount call: `createApp(...).mount(
/// ...)` chained directly, or via a local binding (`const app =
/// createApp(...); ...; app.mount(...)`) -- Vue's canonical convention for
/// "this file boots the app", independent of the file's own name (so it
/// catches a bootstrap file that isn't named `main.js`/`index.js` and isn't
/// `package.json`'s `main`, both of which entrypoint detection otherwise
/// relies on). Walks the whole tree, not just top-level statements: a real
/// app's mount call is very often top-level, but nothing prevents (and it's
/// not rare to see) an environment check or an async bootstrap wrapping it.
///
/// Detected structurally (by call shape), not by proving `createApp` really
/// comes from the `vue` package -- same "pattern, not proof" tradeoff as
/// [`VueGlobalRegistration`]'s detection.
pub fn has_vue_app_mount(root: tree_sitter::Node, src: &str) -> bool {
    let mut app_bindings: HashSet<String> = HashSet::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if !matches!(child.kind(), "lexical_declaration" | "variable_declaration") {
            continue;
        }
        let mut dc = child.walk();
        for d in child.named_children(&mut dc) {
            if d.kind() != "variable_declarator" {
                continue;
            }
            let (Some(name_node), Some(value)) = (
                d.child_by_field_name("name"),
                d.child_by_field_name("value"),
            ) else {
                continue;
            };
            if name_node.kind() == "identifier"
                && (is_create_app_call(value, src) || is_new_vue(value, src))
            {
                app_bindings.insert(node_text(name_node, src).to_string());
            }
        }
    }
    find_mount_call(root, src, &app_bindings)
}

fn is_create_app_call(node: tree_sitter::Node, src: &str) -> bool {
    node.kind() == "call_expression"
        && node
            .child_by_field_name("function")
            .map(|f| f.kind() == "identifier" && node_text(f, src) == "createApp")
            .unwrap_or(false)
}

/// `new Vue(...)`: the Vue 2 root-instance constructor.
fn is_new_vue(node: tree_sitter::Node, src: &str) -> bool {
    node.kind() == "new_expression"
        && node
            .child_by_field_name("constructor")
            .map(|c| c.kind() == "identifier" && node_text(c, src) == "Vue")
            .unwrap_or(false)
}

/// `new Vue({ el: '#app', ... })`: Vue 2 mounts itself through the `el` option, no `.mount()`
/// call needed.
fn is_new_vue_with_el(node: tree_sitter::Node, src: &str) -> bool {
    if !is_new_vue(node, src) {
        return false;
    }
    let Some(options) = node
        .child_by_field_name("arguments")
        .and_then(|a| a.named_child(0))
        .filter(|o| o.kind() == "object")
    else {
        return false;
    };
    let mut c = options.walk();
    let found = options.named_children(&mut c).any(|p| {
        p.kind() == "pair"
            && p.child_by_field_name("key")
                .map(|k| node_text(k, src) == "el")
                .unwrap_or(false)
    });
    found
}

fn find_mount_call(node: tree_sitter::Node, src: &str, app_bindings: &HashSet<String>) -> bool {
    if is_new_vue_with_el(node, src) {
        return true;
    }
    if node.kind() == "call_expression" {
        if let Some(func) = node.child_by_field_name("function") {
            if func.kind() == "member_expression"
                && func
                    .child_by_field_name("property")
                    .map(|p| matches!(node_text(p, src), "mount" | "$mount"))
                    .unwrap_or(false)
            {
                if let Some(object) = func.child_by_field_name("object") {
                    let is_app = is_create_app_call(object, src)
                        || is_new_vue(object, src)
                        || (object.kind() == "identifier"
                            && app_bindings.contains(node_text(object, src)));
                    if is_app {
                        return true;
                    }
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if find_mount_call(child, src, app_bindings) {
            return true;
        }
    }
    false
}

/// One `<app>.component('Name', LocalIdent)` global-registration call:
/// Vue's API for making a component usable by `tag_name` in *any* template
/// project-wide, not just the file that imports it (unlike a plain local
/// import or Options API `components:` entry, both scoped to their own
/// file). `local_identifier` still needs resolving to an actual component
/// symbol *within the registering file* -- this only records the call site;
/// see `collect_template_edges`'s caller for that resolution. Detected
/// structurally (by method name and argument shape) rather than by proving
/// the receiver is really a Vue `App` instance -- same "pattern, not proof"
/// tradeoff as Angular decorator matching elsewhere, and it's *not* gated on
/// `.vue` file extension, since this call almost always lives in a plain
/// `.js`/`.ts` bootstrap file (`main.js`), not a component file.
#[derive(Debug, Clone)]
pub struct VueGlobalRegistration {
    pub tag_name: String,
    pub local_identifier: String,
}

/// One `.vue` file's `<template>` block, plus which symbol reachability
/// edges found in it should be attributed to: that file's own detected
/// component when it has one, or its module symbol as a fallback (a
/// template with no detected component is a degenerate/malformed SFC, but
/// scanning it is still harmless).
pub struct TemplateUsageSite {
    pub file: PathBuf,
    pub template: String,
    pub using_symbol: SymbolId,
}

/// `"user-card"` -> `"UserCard"`; `"UserCard"` (no hyphens) passes through
/// unchanged (already the form Vue also allows writing directly in a
/// template). Doesn't special-case plain HTML tags (`"div"` -> `"Div"`) --
/// nothing named "Div" is a real component in practice, so this never
/// produces a false match, matching `collect_template_edges`'s general
/// "attempt it, no match just means no edge" approach.
///
/// Public because callers building a [`collect_template_edges`] name map
/// from something other than a scanned template -- e.g. a
/// `VueGlobalRegistration`'s `tag_name`, which can itself be written in
/// kebab-case (`app.component('user-card', UserCard)`) -- need to apply the
/// exact same normalization before inserting, or a same-cased registration
/// silently never matches the PascalCase form the scanner always looks up.
pub fn kebab_to_pascal(tag: &str) -> String {
    tag.split('-')
        .filter(|s| !s.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

fn template_tag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // Mirrors `spectograph_ts::angular::scan_template`'s tag regex: only an
    // opening tag counts (`/` right after `<` -- a closing tag -- never
    // matches `[a-zA-Z]`), and attributes on the same tag are ignored here.
    RE.get_or_init(|| Regex::new(r"<\s*([a-zA-Z][\w-]*)").unwrap())
}

fn dynamic_is_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // `<component :is="Foo">` -- Vue's dynamic-component binding. Only the
    // bare-identifier form is matched (an expression like `:is="cond ? A :
    // B"` isn't statically a single name); the identifier is used exactly as
    // written, no kebab-case normalization, since it's a JS expression
    // referencing an in-scope binding, not a tag name.
    RE.get_or_init(|| Regex::new(r#":is\s*=\s*["']([A-Za-z_$][\w$]*)["']"#).unwrap())
}

/// Extract candidate component names from a Vue `<template>` block: every
/// opening tag (kebab-case normalized to PascalCase) plus every
/// `<component :is="Identifier">` dynamic-component binding's bare
/// identifier. Candidates that don't resolve to an actual component are the
/// caller's problem to discard, not this function's -- same split as
/// `spectograph_ts::angular::scan_template`.
pub fn scan_template_component_candidates(template: &str) -> Vec<String> {
    let mut names: Vec<String> = template_tag_re()
        .captures_iter(template)
        .map(|c| kebab_to_pascal(&c[1]))
        .collect();
    names.extend(
        dynamic_is_re()
            .captures_iter(template)
            .map(|c| c[1].to_string()),
    );
    names
}

/// Match every `.vue` file's template against the project-wide name maps,
/// producing `(from, to)` reachability edges: the site's `using_symbol` to
/// the component symbol its tag/`:is` binding resolves to. A candidate name
/// is checked, in order: `local_scope` (this file's own imports/`components:`
/// bindings -- the common, unambiguous case), then `global_components`
/// (`app.component(...)` registrations), then `canonical_names` (a
/// project-wide fallback keyed by each component's own declared/filename
/// name, catching e.g. an aliased import whose local name doesn't match
/// anything in scope). Self-edges (a recursive component referencing its
/// own tag in its own template) are left in, same as Angular's edge
/// collector -- harmless for a reachability walk.
pub fn collect_template_edges(
    sites: &[TemplateUsageSite],
    local_scope: &HashMap<(PathBuf, String), SymbolId>,
    global_components: &HashMap<String, SymbolId>,
    canonical_names: &HashMap<String, SymbolId>,
) -> Vec<(SymbolId, SymbolId)> {
    let mut edges = Vec::new();
    for site in sites {
        for name in scan_template_component_candidates(&site.template) {
            let target = local_scope
                .get(&(site.file.clone(), name.clone()))
                .or_else(|| global_components.get(&name))
                .or_else(|| canonical_names.get(&name));
            if let Some(&target) = target {
                edges.push((site.using_symbol, target));
            }
        }
    }
    edges.sort_unstable();
    edges.dedup();
    edges
}

/// One `const X = defineAsyncComponent(() => import('./X.vue'))` binding:
/// Vue's lazy-loaded/code-split component pattern. `X` already gets an
/// ordinary `Const` symbol from the regular top-level declaration walk
/// (this doesn't mint one) -- this only records the module specifier, so
/// the analyzer can add a `RefKind::Dynamic` edge to whatever component
/// that resolves to and register `X` as template-usable, the same as a
/// regular static import.
#[derive(Debug, Clone)]
pub struct VueAsyncComponent {
    pub local_name: String,
    pub source: String,
}

/// `"user-id"` -> `"userId"`; single-word/no-hyphen input passes through
/// unchanged. Distinct from [`kebab_to_pascal`]: a declared prop/emit name
/// is always a camelCase JS identifier (`defineProps({ userId: String })`),
/// never PascalCase, so matching a template attribute against it needs the
/// lowercase-first form.
fn kebab_to_camel(name: &str) -> String {
    let mut parts = name.split('-').filter(|s| !s.is_empty());
    let Some(first) = parts.next() else {
        return String::new();
    };
    let mut out = first.to_string();
    for part in parts {
        let mut chars = part.chars();
        if let Some(f) = chars.next() {
            out.push(f.to_ascii_uppercase());
            out.push_str(chars.as_str());
        }
    }
    out
}

/// One component tag's own name and the prop/event bindings passed to it at
/// that specific usage site (as opposed to
/// [`scan_template_component_candidates`], which flattens every tag found
/// anywhere in the whole template into one unassociated list -- fine for
/// "is X used at all", not enough to check "did usage site Y pass X a
/// binding it doesn't declare"). Mirrors
/// `spectograph_ts::angular::TemplateComponentUsage`.
#[derive(Debug, PartialEq, Eq)]
pub struct TemplateComponentUsage {
    pub tag: String,
    pub props: Vec<String>,
    pub events: Vec<String>,
}

fn open_tag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // Group 2 (everything up to the closing `>`) breaks if an attribute
    // *value* contains a literal `>` -- same rare-enough-to-accept tradeoff
    // as `spectograph_ts::angular::open_tag_re`.
    RE.get_or_init(|| Regex::new(r"<\s*([a-zA-Z][\w-]*)([^>]*)>").unwrap())
}

fn bind_attr_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // `:label="x"` / `v-bind:label="x"` -- a bound (dynamic-value) prop.
    // Modifiers (`:title.sync`, `:foo.camel`, `:foo.prop`) follow the name.
    RE.get_or_init(|| Regex::new(r"(?:^|\s)(?::|v-bind:)([\w-]+)(?:\.[\w-]+)*=").unwrap())
}

fn on_attr_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // `@click="fn"` / `v-on:click="fn"` -- an event listener. The name may carry a `:`
    // (`@update:modelValue`) and modifiers (`@close.once`, `@click.native`, captured in
    // group 2 so `.native` -- a DOM listener, not a component event -- can be dropped).
    RE.get_or_init(|| Regex::new(r"(?:^|\s)(?:@|v-on:)([\w:-]+)((?:\.[\w-]+)*)=").unwrap())
}

fn vmodel_attr_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // `v-model="x"` (implies prop `modelValue` + event `update:modelValue`)
    // or `v-model:label="x"` (implies prop `label` + event `update:label`); modifiers
    // (`v-model.trim`, `v-model:label.number`) are ignored.
    RE.get_or_init(|| Regex::new(r"(?:^|\s)v-model(?::([\w-]+))?(?:\.[\w-]+)*=").unwrap())
}

fn static_attr_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // A plain attribute (`label="hi"`): no `:`/`@` prefix. Also matches a
    // bare directive name written without one of those prefixes (`v-if=`,
    // `v-model=`, ...) since `v` is a letter too -- the caller filters
    // anything starting with `"v-"` back out, since those aren't props.
    RE.get_or_init(|| Regex::new(r"(?:^|\s)([a-zA-Z][\w-]*)=").unwrap())
}

/// Scan `template` for each component tag and the prop/event bindings
/// passed to it at that specific usage site. Doesn't handle `v-bind="obj"`/
/// `v-on="handlers"` spreads (unenumerable, same "can't statically tell,
/// don't guess" convention as elsewhere) or the `<component :is="...">`
/// dynamic-component form (its target varies at runtime, so a static
/// prop/emit check doesn't cleanly apply -- [`collect_template_edges`]
/// still creates its reachability edge, just not a mismatch check).
pub fn scan_template_component_usages(template: &str) -> Vec<TemplateComponentUsage> {
    open_tag_re()
        .captures_iter(template)
        .map(|caps| {
            let tag = kebab_to_pascal(&caps[1]);
            let attrs_blob = &caps[2];

            let mut props: Vec<String> = bind_attr_re()
                .captures_iter(attrs_blob)
                .map(|c| kebab_to_camel(&c[1]))
                .collect();
            let mut events: Vec<String> = on_attr_re()
                .captures_iter(attrs_blob)
                // `@click.native` listens on the component's root DOM element, not for an
                // emitted event.
                .filter(|c| !c[2].split('.').any(|m| m == "native"))
                .map(|c| kebab_to_camel(&c[1]))
                .collect();
            for c in vmodel_attr_re().captures_iter(attrs_blob) {
                match c.get(1) {
                    Some(name) => {
                        let camel = kebab_to_camel(name.as_str());
                        events.push(format!("update:{camel}"));
                        props.push(camel);
                    }
                    None => {
                        props.push("modelValue".to_string());
                        events.push("update:modelValue".to_string());
                    }
                }
            }
            for c in static_attr_re().captures_iter(attrs_blob) {
                let name = &c[1];
                if name.starts_with("v-") {
                    continue;
                }
                props.push(kebab_to_camel(name));
            }

            TemplateComponentUsage { tag, props, events }
        })
        .collect()
}

/// Attributes Vue treats specially and never shows up in a component's own
/// declared props -- `key`/`ref` are consumed by Vue itself, `class`/`style`
/// are "fallthrough attributes" merged onto the root element automatically
/// even when undeclared. Mirrors `react::IMPLICIT_PROPS`'s role for JSX's
/// `key`/`ref`/`children`.
const IMPLICIT_PROPS: &[&str] = &["key", "ref", "class", "style"];

/// Which kind of binding a [`VueBindingMismatch`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VueBindingKind {
    Prop,
    Event,
}

/// A prop or event bound at a template usage site that the target
/// component doesn't declare via `props`/`defineProps` or
/// `emits`/`defineEmits`.
#[derive(Debug, Clone)]
pub struct VueBindingMismatch {
    pub target_component: String,
    pub binding: String,
    pub kind: VueBindingKind,
    /// Approximate: the *using* component's own declaration location, not
    /// the exact template line -- same convention as
    /// `spectograph_ts::angular::BindingMismatch::file`/`line`.
    pub file: PathBuf,
    pub line: usize,
}

fn resolve_component_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"\bresolve(?:Dynamic)?Component\(\s*['"]([A-Za-z][\w:-]*)['"]"#).unwrap()
    })
}

/// A synthetic template fragment with one `<Name/>` per `resolveComponent('name')` call in the
/// SFC's script blocks. Vue resolves those strings against the same registrations a template
/// tag would, so appending these to the template lets the existing tag matching connect them.
pub fn resolve_component_tags(sfc: &SfcFile) -> String {
    let mut out = String::new();
    for block in [&sfc.script, &sfc.script_setup].into_iter().flatten() {
        for caps in resolve_component_re().captures_iter(&block.content) {
            out.push_str(&format!("<{}/>\n", &caps[1]));
        }
    }
    out
}

/// The tag name of a template's first element (its root, or the first of several).
fn root_tag(template: &str) -> Option<String> {
    open_tag_re().captures(template).map(|c| c[1].to_string())
}

/// True for an ordinary HTML/SVG element name (`div`, `button`, `svg`): lowercase letters and
/// digits only. Components (PascalCase or kebab-case), `<component>`, `<slot>`,
/// `<template>`, `<transition>` and the other built-ins are not.
fn is_plain_element(tag: &str) -> bool {
    tag.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && !matches!(
            tag,
            "component"
                | "slot"
                | "template"
                | "transition"
                | "transitiongroup"
                | "teleport"
                | "suspense"
                | "keepalive"
        )
}

/// Props every element accepts as a plain attribute, so passing one to a component whose root
/// is an element is fallthrough, not a mistake.
fn is_global_attribute(prop: &str) -> bool {
    let camel_prefixed = |prefix: &str| {
        prop.strip_prefix(prefix)
            .and_then(|rest| rest.chars().next())
            .is_some_and(|c| c.is_ascii_uppercase())
    };
    matches!(
        prop,
        "id" | "title" | "role" | "tabindex" | "lang" | "dir" | "hidden"
    ) || camel_prefixed("aria")
        || camel_prefixed("data")
}

/// DOM events a component's root element receives as a fallthrough listener.
const NATIVE_EVENTS: &[&str] = &[
    "click",
    "dblclick",
    "contextmenu",
    "mousedown",
    "mouseup",
    "mousemove",
    "mouseenter",
    "mouseleave",
    "mouseover",
    "mouseout",
    "keydown",
    "keyup",
    "keypress",
    "focus",
    "blur",
    "focusin",
    "focusout",
    "input",
    "submit",
    "scroll",
    "wheel",
    "touchstart",
    "touchmove",
    "touchend",
    "pointerdown",
    "pointerup",
    "pointermove",
    "dragstart",
    "dragover",
    "drop",
    "load",
    "error",
];

/// Flag template usage sites that pass a prop/emit the target component
/// doesn't declare. Only checks targets whose props/emits were statically
/// enumerable (a non-empty `declared_props`/`declared_emits`); a target
/// with none is skipped entirely for that half of the check -- can't tell
/// "declares none" from "we didn't look", same reasoning as
/// `react::find_prop_mismatches`.
pub fn find_binding_mismatches(
    components: &[VueComponent],
    sites: &[TemplateUsageSite],
    local_scope: &HashMap<(PathBuf, String), SymbolId>,
    global_components: &HashMap<String, SymbolId>,
    canonical_names: &HashMap<String, SymbolId>,
) -> Vec<VueBindingMismatch> {
    let by_symbol: HashMap<SymbolId, &VueComponent> =
        components.iter().map(|c| (c.symbol_id, c)).collect();
    let template_by_component: HashMap<SymbolId, &str> = sites
        .iter()
        .map(|s| (s.using_symbol, s.template.as_str()))
        .collect();

    let mut out = Vec::new();
    for site in sites {
        let (using_file, using_line) = by_symbol
            .get(&site.using_symbol)
            .map(|c| (c.file.clone(), c.line))
            .unwrap_or_else(|| (site.file.clone(), 1));

        for usage in scan_template_component_usages(&site.template) {
            let target = local_scope
                .get(&(site.file.clone(), usage.tag.clone()))
                .or_else(|| global_components.get(&usage.tag))
                .or_else(|| canonical_names.get(&usage.tag))
                .and_then(|id| by_symbol.get(id));
            let Some(target) = target else {
                continue;
            };
            // Undeclared attributes and listeners fall through to the target's root. When that
            // root is another component (or unknowable) we can't tell what it accepts, so
            // there is nothing to check; when it is a plain element, DOM-global attributes
            // and native events are legitimate even though undeclared.
            let root_is_plain_element = match template_by_component.get(&target.symbol_id) {
                Some(t) => root_tag(t).is_some_and(|tag| is_plain_element(&tag)),
                None => false,
            };
            if !root_is_plain_element {
                continue;
            }

            if !target.declared_props.is_empty() {
                for prop in &usage.props {
                    if IMPLICIT_PROPS.contains(&prop.as_str()) || is_global_attribute(prop) {
                        continue;
                    }
                    // `defineProps({ 'user-id': ... })` is bound as `:user-id` or `:userId`
                    // alike, and usage names are already camelCased.
                    if !target
                        .declared_props
                        .iter()
                        .any(|p| p == prop || kebab_to_camel(p) == *prop)
                    {
                        out.push(VueBindingMismatch {
                            target_component: target.name.clone(),
                            binding: prop.clone(),
                            kind: VueBindingKind::Prop,
                            file: using_file.clone(),
                            line: using_line,
                        });
                    }
                }
            }
            if !target.declared_emits.is_empty() {
                for event in &usage.events {
                    if NATIVE_EVENTS.contains(&event.as_str()) {
                        continue;
                    }
                    // `emits: ['change-value']` is listened for as `@change-value` or
                    // `@changeValue` alike, and usage names are already camelCased.
                    if !target
                        .declared_emits
                        .iter()
                        .any(|e| e == event || kebab_to_camel(e) == *event)
                    {
                        out.push(VueBindingMismatch {
                            target_component: target.name.clone(),
                            binding: event.clone(),
                            kind: VueBindingKind::Event,
                            file: using_file.clone(),
                            line: using_line,
                        });
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_name_from_path_uses_file_stem() {
        assert_eq!(
            component_name_from_path(Path::new("components/UserCard.vue")),
            "UserCard"
        );
    }

    fn parse_js(src: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_javascript::LANGUAGE.into())
            .unwrap();
        parser.parse(src, None).unwrap()
    }

    #[test]
    fn extract_names_from_array_literal() {
        let src = "const props = ['foo', 'bar']";
        let tree = parse_js(src);
        let decl = tree.root_node().named_child(0).unwrap();
        let declarator = decl.named_child(0).unwrap();
        let value = declarator.child_by_field_name("value").unwrap();
        assert_eq!(value.kind(), "array");
        let names = extract_names_from_array_or_object(value, src.as_bytes());
        assert_eq!(names, vec!["foo", "bar"]);
    }

    #[test]
    fn extract_names_from_object_literal_with_typed_and_shorthand_keys() {
        let src = "const props = { foo: String, bar: { type: Number }, baz }";
        let tree = parse_js(src);
        let decl = tree.root_node().named_child(0).unwrap();
        let declarator = decl.named_child(0).unwrap();
        let value = declarator.child_by_field_name("value").unwrap();
        assert_eq!(value.kind(), "object");
        let names = extract_names_from_array_or_object(value, src.as_bytes());
        assert_eq!(names, vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn extract_names_from_unrecognized_shape_is_empty() {
        let src = "const props = externalPropsList";
        let tree = parse_js(src);
        let decl = tree.root_node().named_child(0).unwrap();
        let declarator = decl.named_child(0).unwrap();
        let value = declarator.child_by_field_name("value").unwrap();
        assert!(extract_names_from_array_or_object(value, src.as_bytes()).is_empty());
    }

    #[test]
    fn parse_sfc_splits_template_and_script() {
        let src = "<template>\n  <div>{{ msg }}</div>\n</template>\n\n<script>\nexport default {\n  name: 'Foo',\n}\n</script>\n";
        let sfc = parse_sfc(src);
        let template = sfc.template.expect("template block");
        assert_eq!(template.content.trim(), "<div>{{ msg }}</div>");
        assert_eq!(template.start_line, 1, "line of the <template> tag itself");

        let script = sfc.script.expect("script block");
        assert!(script.content.contains("name: 'Foo'"));
        assert_eq!(script.lang, ScriptLang::JavaScript);
        assert!(!script.setup);
        assert_eq!(script.start_line, 5, "line of the <script> tag itself");
        assert!(sfc.script_setup.is_none());
    }

    #[test]
    fn nested_template_elements_do_not_end_the_root_template_early() {
        // Slots (`<template #fallback>`) and `v-if`/`v-for` wrappers are nested
        // `<template>` elements; the root block ends at the *matching* close tag, so
        // everything after the first inner `</template>` is still part of it.
        let src = "<template>\n  <Suspense>\n    <A />\n    <template #fallback>loading</template>\n  </Suspense>\n  <Suspense>\n    <B />\n    <template #fallback><template v-if=\"x\">deep</template></template>\n  </Suspense>\n</template>\n<script setup>\n</script>\n";
        let sfc = parse_sfc(src);
        let template = sfc.template.expect("template block").content;
        assert!(template.contains("<A />"));
        assert!(
            template.contains("<B />"),
            "content after the first nested </template> was cut off: {template:?}"
        );
        assert!(template.trim_end().ends_with("</Suspense>"));
        // And the script block after it is still found.
        assert!(sfc.script_setup.is_some());
    }

    #[test]
    fn self_closing_template_does_not_open_a_nesting_level() {
        let src = "<template>\n  <template v-if=\"x\" />\n  <C />\n</template>\n";
        let template = parse_sfc(src).template.expect("template block").content;
        assert!(template.contains("<C />"));
    }

    #[test]
    fn unclosed_template_yields_no_block() {
        assert!(parse_sfc("<template>\n  <div/>\n").template.is_none());
    }

    #[test]
    fn parse_sfc_detects_typescript_and_setup() {
        let src = "<script setup lang=\"ts\">\nconst x: number = 1\n</script>\n";
        let sfc = parse_sfc(src);
        let script = sfc.script_setup.expect("script setup block");
        assert_eq!(script.lang, ScriptLang::TypeScript);
        assert!(script.setup);
        assert_eq!(
            script.start_line, 1,
            "line of the <script setup lang=\"ts\"> tag itself"
        );
        assert!(sfc.script.is_none());
    }

    #[test]
    fn parse_sfc_handles_combined_script_and_script_setup() {
        let src = "<script>\nexport default { name: 'Foo' }\n</script>\n<script setup>\nconst count = 1\n</script>\n";
        let sfc = parse_sfc(src);
        assert!(sfc.script.is_some());
        assert!(sfc.script_setup.is_some());
        assert_eq!(sfc.script.unwrap().start_line, 1);
        assert_eq!(sfc.script_setup.unwrap().start_line, 4);
    }

    #[test]
    fn materialize_script_preserves_original_line_numbers() {
        let src = "<template>\n  <div/>\n</template>\n\n<script setup>\nfunction one() {}\n\nfunction two() {}\n</script>\n";
        let sfc = parse_sfc(src);
        let (materialized, lang) = materialize_script(&sfc).expect("script present");
        assert_eq!(lang, ScriptLang::JavaScript);
        let out_lines: Vec<&str> = materialized.lines().collect();
        // `function one() {}` was on line 6 of the original file, and must
        // land on the same line number in the materialized output.
        assert_eq!(out_lines[5].trim(), "function one() {}");
        assert_eq!(out_lines[7].trim(), "function two() {}");
    }

    #[test]
    fn materialize_script_combines_script_and_script_setup_without_collision() {
        let src = "<script>\nexport default {\n  name: 'Foo',\n}\n</script>\n\n<script setup>\nconst count = 1\n</script>\n";
        let sfc = parse_sfc(src);
        let (materialized, _lang) = materialize_script(&sfc).expect("script present");
        let out_lines: Vec<&str> = materialized.lines().collect();
        assert!(out_lines[1].contains("export default"));
        assert!(out_lines[7].contains("const count = 1"));
    }

    #[test]
    fn materialize_script_prefers_typescript_when_either_block_is_ts() {
        let src = "<script lang=\"ts\">\nexport default {}\n</script>\n<script setup>\nconst x = 1\n</script>\n";
        let sfc = parse_sfc(src);
        let (_materialized, lang) = materialize_script(&sfc).expect("script present");
        assert_eq!(lang, ScriptLang::TypeScript);
    }

    #[test]
    fn materialize_script_returns_none_when_no_script_block() {
        let sfc = parse_sfc("<template><div/></template>\n");
        assert!(materialize_script(&sfc).is_none());
    }

    #[test]
    fn discover_vue_files_finds_vue_and_skips_others() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("App.vue"), "<template/>").unwrap();
        std::fs::write(dir.path().join("main.ts"), "").unwrap();
        std::fs::create_dir(dir.path().join("components")).unwrap();
        std::fs::write(dir.path().join("components/Child.vue"), "<template/>").unwrap();

        let found = discover_vue_files(dir.path()).unwrap();
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|p| p.extension().unwrap() == "vue"));
    }

    #[test]
    fn kebab_to_pascal_converts_hyphenated_tag_names() {
        assert_eq!(kebab_to_pascal("user-card"), "UserCard");
        assert_eq!(kebab_to_pascal("UserCard"), "UserCard");
        assert_eq!(kebab_to_pascal("div"), "Div");
    }

    #[test]
    fn scan_template_component_candidates_finds_kebab_pascal_and_dynamic_tags() {
        let template = r#"<div><user-card/><UserProfile/><component :is="Dynamic"/></div>"#;
        let names = scan_template_component_candidates(template);
        assert!(names.contains(&"UserCard".to_string()));
        assert!(names.contains(&"UserProfile".to_string()));
        assert!(names.contains(&"Dynamic".to_string()));
        // The `<component>` tag itself is also a candidate (kebab_to_pascal
        // gives "Component") -- harmless, since nothing will name a real
        // component that.
        assert!(names.contains(&"Div".to_string()));
    }

    #[test]
    fn scan_template_component_candidates_ignores_expression_is_bindings() {
        // `:is="cond ? A : B"` isn't a single static name -- must not match
        // the bare-identifier regex at all.
        let template = r#"<component :is="cond ? A : B"/>"#;
        let names = scan_template_component_candidates(template);
        assert!(!names.contains(&"A".to_string()));
        assert!(!names.contains(&"B".to_string()));
    }

    fn site(file: &str, template: &str, using_symbol: SymbolId) -> TemplateUsageSite {
        TemplateUsageSite {
            file: PathBuf::from(file),
            template: template.to_string(),
            using_symbol,
        }
    }

    #[test]
    fn collect_template_edges_prefers_local_scope_over_global_and_canonical() {
        let sites = vec![site("App.vue", "<UserCard/>", 0)];
        let mut local_scope = HashMap::new();
        local_scope.insert((PathBuf::from("App.vue"), "UserCard".to_string()), 1);
        let mut global_components = HashMap::new();
        global_components.insert("UserCard".to_string(), 2);
        let mut canonical_names = HashMap::new();
        canonical_names.insert("UserCard".to_string(), 3);

        let edges =
            collect_template_edges(&sites, &local_scope, &global_components, &canonical_names);
        assert_eq!(edges, vec![(0, 1)]);
    }

    #[test]
    fn collect_template_edges_falls_back_to_global_then_canonical() {
        let sites = vec![site("A.vue", "<Foo/>", 0), site("B.vue", "<Bar/>", 10)];
        let local_scope = HashMap::new();
        let mut global_components = HashMap::new();
        global_components.insert("Foo".to_string(), 100);
        let mut canonical_names = HashMap::new();
        canonical_names.insert("Bar".to_string(), 200);

        let edges =
            collect_template_edges(&sites, &local_scope, &global_components, &canonical_names);
        assert!(edges.contains(&(0, 100)));
        assert!(edges.contains(&(10, 200)));
    }

    #[test]
    fn collect_template_edges_recursive_self_reference_is_allowed() {
        let sites = vec![site("Tree.vue", "<Tree/>", 0)];
        let mut canonical_names = HashMap::new();
        canonical_names.insert("Tree".to_string(), 0);
        let edges =
            collect_template_edges(&sites, &HashMap::new(), &HashMap::new(), &canonical_names);
        assert_eq!(edges, vec![(0, 0)]);
    }

    #[test]
    fn collect_template_edges_no_match_produces_no_edge() {
        let sites = vec![site("App.vue", "<div><span/></div>", 0)];
        let edges =
            collect_template_edges(&sites, &HashMap::new(), &HashMap::new(), &HashMap::new());
        assert!(edges.is_empty());
    }

    #[test]
    fn kebab_to_camel_converts_hyphenated_attr_names() {
        assert_eq!(kebab_to_camel("user-id"), "userId");
        assert_eq!(kebab_to_camel("label"), "label");
        assert_eq!(kebab_to_camel("a-b-c"), "aBC");
    }

    #[test]
    fn scan_template_component_usages_collects_static_bound_and_vmodel_bindings() {
        let template = r#"<user-card label="hi" :user-id="1" @click="go()" v-model="x" v-model:size="s" class="c"/>"#;
        let usages = scan_template_component_usages(template);
        assert_eq!(usages.len(), 1);
        let usage = &usages[0];
        assert_eq!(usage.tag, "UserCard");
        assert!(usage.props.contains(&"label".to_string()));
        assert!(usage.props.contains(&"userId".to_string()));
        assert!(usage.props.contains(&"modelValue".to_string()));
        assert!(usage.props.contains(&"size".to_string()));
        assert!(usage.props.contains(&"class".to_string()));
        assert!(usage.events.contains(&"click".to_string()));
        assert!(usage.events.contains(&"update:modelValue".to_string()));
        assert!(usage.events.contains(&"update:size".to_string()));
    }

    #[test]
    fn scan_template_component_usages_ignores_structural_directives() {
        let template = r#"<user-card v-if="cond" v-for="x in xs" v-show="cond"/>"#;
        let usages = scan_template_component_usages(template);
        assert_eq!(usages.len(), 1);
        assert!(usages[0].props.is_empty());
        assert!(usages[0].events.is_empty());
    }

    fn test_component(
        symbol_id: SymbolId,
        name: &str,
        props: &[&str],
        emits: &[&str],
    ) -> VueComponent {
        VueComponent {
            symbol_id,
            name: name.to_string(),
            file: PathBuf::from("test.vue"),
            line: 1,
            declared_props: props.iter().map(|s| s.to_string()).collect(),
            declared_emits: emits.iter().map(|s| s.to_string()).collect(),
            registered_components: Vec::new(),
        }
    }

    #[test]
    fn find_binding_mismatches_flags_undeclared_prop_and_event() {
        let using = test_component(0, "App", &[], &[]);
        let target = test_component(1, "UserCard", &["label"], &["change"]);
        let sites = vec![
            site(
                "App.vue",
                r#"<user-card label="hi" :extra="1" @change="f()" @bogus="g()"/>"#,
                0,
            ),
            // The target's own template: a plain `<div>` root, so attributes fall through
            // to an element and undeclared ones are checkable.
            site("UserCard.vue", "<div/>", 1),
        ];
        let mut local_scope = HashMap::new();
        local_scope.insert((PathBuf::from("App.vue"), "UserCard".to_string()), 1);

        let mismatches = find_binding_mismatches(
            &[using, target],
            &sites,
            &local_scope,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(mismatches
            .iter()
            .any(|m| m.binding == "extra" && m.kind == VueBindingKind::Prop));
        assert!(mismatches
            .iter()
            .any(|m| m.binding == "bogus" && m.kind == VueBindingKind::Event));
        assert!(!mismatches.iter().any(|m| m.binding == "label"));
        assert!(!mismatches.iter().any(|m| m.binding == "change"));
    }

    #[test]
    fn find_binding_mismatches_skips_target_with_no_declared_props_or_emits() {
        let using = test_component(0, "App", &[], &[]);
        let target = test_component(1, "UserCard", &[], &[]);
        let sites = vec![site("App.vue", r#"<user-card :anything="1"/>"#, 0)];
        let mut local_scope = HashMap::new();
        local_scope.insert((PathBuf::from("App.vue"), "UserCard".to_string()), 1);

        let mismatches = find_binding_mismatches(
            &[using, target],
            &sites,
            &local_scope,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(mismatches.is_empty());
    }

    #[test]
    fn find_binding_mismatches_ignores_implicit_props() {
        let using = test_component(0, "App", &[], &[]);
        let target = test_component(1, "UserCard", &["label"], &[]);
        let sites = vec![site(
            "App.vue",
            r#"<user-card key="1" ref="r" class="c" style="s"/>"#,
            0,
        )];
        let mut local_scope = HashMap::new();
        local_scope.insert((PathBuf::from("App.vue"), "UserCard".to_string()), 1);

        let mismatches = find_binding_mismatches(
            &[using, target],
            &sites,
            &local_scope,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(
            mismatches.is_empty(),
            "key/ref/class/style are framework bindings, not UserCard's own: {:?}",
            mismatches
        );
    }

    #[test]
    fn has_vue_app_mount_detects_direct_chain() {
        let src = "createApp(App).mount('#app')\n";
        let tree = parse_js(src);
        assert!(has_vue_app_mount(tree.root_node(), src));
    }

    #[test]
    fn has_vue_app_mount_detects_via_local_binding() {
        let src = "const app = createApp(App)\napp.mount('#app')\n";
        let tree = parse_js(src);
        assert!(has_vue_app_mount(tree.root_node(), src));
    }

    #[test]
    fn has_vue_app_mount_ignores_unrelated_mount_calls() {
        let src = "wrapper.mount()\nconst x = createApp(App)\nother.mount('#app')\n";
        let tree = parse_js(src);
        assert!(
            !has_vue_app_mount(tree.root_node(), src),
            "a .mount() call on something other than the createApp(...) result/binding must not match"
        );
    }

    #[test]
    fn has_vue_app_mount_is_false_with_no_mount_call() {
        let src = "const app = createApp(App)\n";
        let tree = parse_js(src);
        assert!(!has_vue_app_mount(tree.root_node(), src));
    }
}
