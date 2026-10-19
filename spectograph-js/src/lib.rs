//! Spectograph JavaScript analyzer.
//!
//! Parses JavaScript source with tree-sitter into the shared [`Program`] IR.

pub mod aliases;
pub mod diagnostics;
pub mod entrypoints;
pub mod exports;
pub mod implicit;
pub mod jsonc;
pub mod parse;
pub mod pathutil;
pub mod react;
pub mod template_bindings;
pub mod tsconfig;
pub mod vue;

/// The `typescript-language-server`-backed type-aware resolution backend
/// lives in `spectograph-tsserver` since `spectograph-ts` also shares it (and
/// this crate can't depend on `spectograph-ts`, which already depends on this
/// crate for its React support) -- re-exported here for ergonomic access.
pub use spectograph_tsserver as tsserver;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use aliases::ProjectResolver;
use entrypoints::detect_entries;
use exports::ExportedByModule;
use parse::parse_file;
use pathutil::normalize_path;
use serde_json::Value;
use spectograph_analyzer::{discover, AnalyzeError, Analyzer};
use spectograph_core::diagnostic::Diagnostic;
use spectograph_core::ir::{
    Environment, Language, Program, RefKind, Reference, SymbolId, SymbolKind,
};

/// Analyzer for the JavaScript language.
pub struct JavaScriptAnalyzer;

/// Which reference-resolution backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionMode {
    /// Hand-rolled static resolver (scope, module resolution, CommonJS/ESM
    /// import handling).
    Static,
    /// `typescript-language-server` type-aware resolver (via LSP
    /// find-references), running in `allowJs`/`checkJs` mode.
    Tsserver,
}

/// A curated set of very common npm packages. Used to recognize bare import
/// specifiers as legitimate external dependencies even when `node_modules` isn't
/// present on disk (e.g. analyzing a repo before `npm install`), or when a package
/// simply doesn't declare an entry point we can resolve.
pub const KNOWN_NPM_PACKAGES: &[&str] = &[
    "react",
    "react-dom",
    "react-router",
    "react-router-dom",
    "next",
    "vue",
    "svelte",
    "lodash",
    "underscore",
    "ramda",
    "moment",
    "dayjs",
    "date-fns",
    "express",
    "koa",
    "fastify",
    "hapi",
    "@nestjs/core",
    "axios",
    "node-fetch",
    "got",
    "request",
    "superagent",
    "webpack",
    "vite",
    "rollup",
    "esbuild",
    "parcel",
    "@babel/core",
    "typescript",
    "eslint",
    "prettier",
    "jest",
    "vitest",
    "mocha",
    "chai",
    "sinon",
    "redux",
    "@reduxjs/toolkit",
    "mobx",
    "rxjs",
    "zustand",
    "recoil",
    "chalk",
    "commander",
    "yargs",
    "dotenv",
    "uuid",
    "nanoid",
    "classnames",
    "prop-types",
    "styled-components",
    "tailwindcss",
    "sass",
    "less",
    "socket.io",
    "ws",
    "graphql",
    "apollo-server",
    "@apollo/client",
    "mongoose",
    "sequelize",
    "prisma",
    "knex",
    "pg",
    "mysql",
    "mysql2",
    "sqlite3",
    "jsonwebtoken",
    "bcrypt",
    "bcryptjs",
    "passport",
    "cors",
    "helmet",
    "morgan",
    "body-parser",
    "multer",
    "joi",
    "zod",
    "yup",
    "ajv",
    "immutable",
    "lru-cache",
    "debug",
    "semver",
    "glob",
    "fs-extra",
    "chokidar",
    "jquery",
    "d3",
    "three",
    "chart.js",
    "leaflet",
];

/// The package name a bare (non-relative, non-absolute) import specifier belongs to:
/// `"lodash/debounce"` -> `"lodash"`, `"@scope/pkg/sub"` -> `"@scope/pkg"`,
/// `"react"` -> `"react"`. A scoped package name is two `/`-separated segments;
/// anything else is one.
pub fn npm_package_root(specifier: &str) -> &str {
    let segments_in_name = if specifier.starts_with('@') { 2 } else { 1 };
    match specifier
        .char_indices()
        .filter(|(_, c)| *c == '/')
        .nth(segments_in_name - 1)
    {
        Some((i, _)) => &specifier[..i],
        None => specifier,
    }
}

/// True if `specifier` (a bare, non-relative import specifier) is a recognized
/// third-party package: either a well-known name, or declared in the project's
/// package.json.
pub fn is_recognized_npm_package(specifier: &str, declared_deps: &HashSet<String>) -> bool {
    let root = npm_package_root(specifier);
    KNOWN_NPM_PACKAGES.contains(&root) || declared_deps.contains(root)
}

/// Read all dependency names declared in the root package.json, across
/// dependencies/devDependencies/peerDependencies/optionalDependencies.
pub fn read_declared_dependencies(root: &Path) -> HashSet<String> {
    let Some(json) = std::fs::read_to_string(root.join("package.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
    else {
        return HashSet::new();
    };
    [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ]
    .iter()
    .filter_map(|key| json.get(*key).and_then(|v| v.as_object()))
    .flat_map(|m| m.keys().cloned())
    .collect()
}

/// Read the `browser` field of the project's root `package.json`, if it's an object map
/// (bundlers use this to substitute browser-specific modules for Node ones, e.g.
/// `{"fs": false, "./server.js": "./client.js"}`). A string-form `browser` field is
/// handled separately as an entrypoint (see [`entrypoints::detect_from_package_json`]).
fn read_browser_map(root: &Path) -> HashMap<String, Value> {
    std::fs::read_to_string(root.join("package.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("browser").cloned())
        .and_then(|b| b.as_object().cloned())
        .map(|m| m.into_iter().collect())
        .unwrap_or_default()
}

/// Read a source file as text, replacing invalid UTF-8 rather than failing: a single
/// latin-1 or binary-ish file in a repo (vendored libraries, fixtures) must not abort the
/// analysis of everything else.
fn read_source(path: &Path) -> std::io::Result<String> {
    std::fs::read(path).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// Extensions a bare `require('./x')`/`import './x'` may omit.
const SOURCE_EXTENSIONS: &[&str] = &["js", "jsx", "mjs", "cjs", "vue"];

/// Try `path` as-is, then with each JS source extension, then as a directory's index file
/// (Node.js's extension and directory-index resolution rules), returning the first that
/// exists as a file.
fn resolve_file_candidate(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    // Append, don't replace: `Path::with_extension` treats the text after the
    // *last* dot in the file name as "the extension" and replaces it, which
    // silently breaks any specifier whose real file name contains a dot
    // before its actual extension (`./foo.model` -> `foo.model.js`,
    // `./foo.test` -> `foo.test.js`, ...). `with_extension("js")` on
    // `foo.model` produces `foo.js`, not `foo.model.js`, so the guessed
    // candidate never exists and resolution fails outright.
    for ext in SOURCE_EXTENSIONS {
        let candidate = PathBuf::from(format!("{}.{ext}", path.to_string_lossy()));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    if path.is_dir() {
        for ext in SOURCE_EXTENSIONS {
            let index_path = path.join(format!("index.{ext}"));
            if index_path.is_file() {
                return Some(index_path);
            }
        }
    }
    None
}

/// Pick an entry-point file from a package.json `exports` condition object
/// (`{"require": ..., "import": ..., "default": ...}`), preferring `require` since
/// this analyzer treats CommonJS and ESM specifiers uniformly.
fn pick_export_condition(map: &serde_json::Map<String, Value>) -> Option<String> {
    ["require", "node", "import", "default"]
        .iter()
        .find_map(|key| map.get(*key).and_then(|v| v.as_str()).map(String::from))
}

/// Resolve a package.json `exports` field to a single entry-point path, handling the
/// common shapes: a bare string, `{".": "..."}`, `{".": {"require": ..., ...}}`, and a
/// condition object with no `"."` key.
fn exports_entry(exports: &Value) -> Option<String> {
    match exports {
        Value::String(s) => Some(s.clone()),
        Value::Object(map) => match map.get(".") {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Object(cond)) => pick_export_condition(cond),
            _ => pick_export_condition(map),
        },
        _ => None,
    }
}

/// Resolve a package directory (one that has been found under `node_modules`) to its
/// entry-point file, preferring the modern `exports` field over `main`, and falling
/// back to `index.js` if neither resolves to a real file.
fn resolve_package_entry(pkg_dir: &Path) -> Option<PathBuf> {
    if let Ok(content) = std::fs::read_to_string(pkg_dir.join("package.json")) {
        if let Ok(json) = serde_json::from_str::<Value>(&content) {
            let entry = json
                .get("exports")
                .and_then(exports_entry)
                .or_else(|| json.get("main").and_then(|m| m.as_str()).map(String::from));
            if let Some(entry) = entry {
                if let Some(resolved) =
                    resolve_file_candidate(&normalize_path(&pkg_dir.join(&entry)))
                {
                    return Some(resolved);
                }
            }
        }
    }
    resolve_file_candidate(&pkg_dir.join("index.js"))
}

/// Node.js-style module resolution helper, with optional bundler `browser`-field
/// remapping and project-level aliases (`jsconfig` `paths`, `package.json` `imports`,
/// workspace packages, `@/`-style conventions). Returns the resolved file path plus the
/// [`Environment`] the resolution implies (`Browser` only when a `browser` field remap was
/// applied).
fn resolve_module_path(
    base: &Path,
    module_path: &str,
    browser_map: &HashMap<String, Value>,
    resolver: &ProjectResolver,
) -> (PathBuf, Environment) {
    if let Some(mapped) = browser_map.get(module_path).and_then(|v| v.as_str()) {
        let (resolved, _) = resolve_module_path(base, mapped, &HashMap::new(), resolver);
        return (resolved, Environment::Browser);
    }

    // Handle absolute paths
    if module_path.starts_with('/') || module_path.starts_with("\\") {
        return (PathBuf::from(module_path), Environment::Universal);
    }

    // Handle relative paths (starting with '.' or '..')
    if module_path.starts_with('.') {
        let path = normalize_path(&base.join(module_path));
        if let Some(resolved) = resolve_file_candidate(&path) {
            return (resolved, Environment::Universal);
        }
        return (path, Environment::Universal);
    }

    // Project aliases and workspace packages win over `node_modules`, the way a bundler
    // configured with them would resolve.
    for candidate in resolver.candidates(module_path) {
        if let Some(resolved) = resolve_file_candidate(&candidate) {
            return (resolved, Environment::Universal);
        }
    }

    // Handle node_modules packages (non-relative imports), including package
    // sub-paths (`lodash/debounce`) and scoped packages (`@scope/pkg`).
    // Walk up the directory tree looking for node_modules.
    let mut current = base;
    loop {
        let candidate = current.join("node_modules").join(module_path);

        if candidate.exists() {
            if candidate.is_dir() {
                if let Some(entry) = resolve_package_entry(&candidate) {
                    return (entry, Environment::Universal);
                }
            } else {
                return (candidate, Environment::Universal);
            }
        } else if let Some(resolved) = resolve_file_candidate(&candidate) {
            // A sub-path import without an extension, e.g. `require('pkg/sub')`.
            return (resolved, Environment::Universal);
        }

        // Move up to parent directory
        if let Some(parent) = current.parent() {
            current = parent;
        } else {
            break;
        }
    }

    // Fallback: just return the path as-is (will fail later)
    (base.join(module_path), Environment::Universal)
}

/// React-specific facts gathered alongside the regular [`Program`] IR by
/// [`JavaScriptAnalyzer::analyze_react`]. See [`react`] for what each field means.
#[derive(Debug, Clone, Default)]
pub struct ReactAnalysis {
    pub components: Vec<react::ReactComponent>,
    pub jsx_usages: Vec<react::JsxUsage>,
    pub hook_calls: Vec<react::HookCall>,
}

/// Vue-specific facts gathered alongside the regular [`Program`] IR by
/// [`JavaScriptAnalyzer::analyze_vue`]. See [`vue::VueComponent`] for what
/// each field means. Template-usage reachability edges (the Vue analogue of
/// JSX usage sites) are folded directly into `Program.references` rather
/// than returned here -- see `analyze_impl`'s Vue passes; prop/emit
/// mismatches specifically are returned separately by
/// [`JavaScriptAnalyzer::analyze_vue_mismatches`], mirroring how React's
/// `find_prop_mismatches` and Angular's `analyze_angular` are both kept out
/// of their respective main analysis result too.
#[derive(Debug, Clone, Default)]
pub struct VueAnalysis {
    pub components: Vec<vue::VueComponent>,
}

/// The three maps [`build_vue_scope_maps`] produces: local per-file scope,
/// project-wide `app.component(...)` registrations, and canonical names.
type VueScopeMaps = (
    HashMap<(PathBuf, String), SymbolId>,
    HashMap<String, SymbolId>,
    HashMap<String, SymbolId>,
);

/// Build the Vue template-usage-matching maps: local per-file scope
/// (resolved imports plus `defineAsyncComponent` bindings -- both already
/// present in `program.references` as ReExport/Dynamic edges by the time
/// this runs), project-wide `app.component(...)` registrations, and each
/// component's own canonical name. Shared by the reachability-edge pass in
/// `analyze_impl` and [`JavaScriptAnalyzer::analyze_vue_mismatches`], so the
/// two can't silently drift apart -- same role as `spectograph_ts`'s
/// `build_angular_selectors`.
fn build_vue_scope_maps(program: &Program, parsed: &[(String, parse::FileResult)]) -> VueScopeMaps {
    let vue_component_ids: HashSet<SymbolId> = parsed
        .iter()
        .filter_map(|(_p, r)| r.vue_component.as_ref().map(|c| c.symbol_id))
        .collect();

    // Nothing to match against: every map below would come out empty, and skipping the
    // edge indexing keeps non-Vue projects from paying for it.
    if vue_component_ids.is_empty() {
        return (HashMap::new(), HashMap::new(), HashMap::new());
    }

    // First edge of each kind per source symbol, indexed once instead of rescanning every
    // reference for every import.
    let mut first_reexport: HashMap<SymbolId, SymbolId> = HashMap::new();
    let mut first_dynamic: HashMap<SymbolId, SymbolId> = HashMap::new();
    for r in &program.references {
        match r.kind {
            RefKind::ReExport => {
                first_reexport.entry(r.from).or_insert(r.to);
            }
            RefKind::Dynamic => {
                first_dynamic.entry(r.from).or_insert(r.to);
            }
            _ => {}
        }
    }

    // (file, bound local name) -> the Vue component symbol that name
    // resolves to, for every file's own imports and
    // `defineAsyncComponent` bindings -- covers `<script setup>`'s implicit
    // template-scope registration (any in-scope binding is usable) and
    // Options API's `components: { Foo }` shorthand (the common form,
    // where the key equals the import name).
    let mut local_scope: HashMap<(PathBuf, String), SymbolId> = HashMap::new();
    for (path, result) in parsed {
        for imp in &result.imports {
            if let Some(&target) = first_reexport.get(&imp.symbol_id) {
                if vue_component_ids.contains(&target) {
                    local_scope.insert(
                        (PathBuf::from(path.as_str()), imp.local_name.clone()),
                        target,
                    );
                }
            }
        }
        for reg in &result.vue_async_components {
            let Some(const_id) = result
                .symbols
                .iter()
                .find(|s| s.name == reg.local_name && s.kind == SymbolKind::Const)
                .map(|s| s.id)
            else {
                continue;
            };
            if let Some(&target) = first_dynamic.get(&const_id) {
                if vue_component_ids.contains(&target) {
                    local_scope.insert(
                        (PathBuf::from(path.as_str()), reg.local_name.clone()),
                        target,
                    );
                }
            }
        }
    }

    // `app.component('Name', Foo)` registrations, resolved through the
    // same per-file local scope (the call site's own file is where `Foo`
    // needs to resolve from) into a project-wide tag-name map.
    let mut global_components: HashMap<String, SymbolId> = HashMap::new();
    for (path, result) in parsed {
        for reg in &result.vue_global_registrations {
            if let Some(&target) =
                local_scope.get(&(PathBuf::from(path.as_str()), reg.local_identifier.clone()))
            {
                global_components
                    .entry(vue::kebab_to_pascal(&reg.tag_name))
                    .or_insert(target);
            }
        }
    }

    let mut canonical_names: HashMap<String, SymbolId> = HashMap::new();
    for (_path, result) in parsed {
        if let Some(c) = &result.vue_component {
            canonical_names.entry(c.name.clone()).or_insert(c.symbol_id);
        }
    }

    (local_scope, global_components, canonical_names)
}

/// Every Vue template prop/emit mismatch in an already-analyzed project. Rebuilds the same
/// scope maps the reachability-edge pass in `analyze_impl` uses (see `build_vue_scope_maps`),
/// plus a fresh read of each `.vue` file's `<template>` block -- not persisted in
/// `analyze_impl`'s own return value, since nothing else needs it once the edges are built.
pub(crate) fn vue_mismatches(
    program: &Program,
    parsed: &[(String, parse::FileResult)],
    root: &Path,
) -> Result<Vec<vue::VueBindingMismatch>, AnalyzeError> {
    if parsed.iter().all(|(_p, r)| r.vue_component.is_none()) {
        return Ok(Vec::new());
    }
    let (local_scope, global_components, canonical_names) = build_vue_scope_maps(program, parsed);

    let using_symbol_by_path: HashMap<PathBuf, SymbolId> = parsed
        .iter()
        .map(|(path, result)| {
            let using = result
                .vue_component
                .as_ref()
                .map(|c| c.symbol_id)
                .unwrap_or(result.module_id);
            (PathBuf::from(path.as_str()), using)
        })
        .collect();

    let mut sites: Vec<vue::TemplateUsageSite> = Vec::new();
    for path in vue::discover_vue_files(root)? {
        let src = read_source(&path)?;
        let sfc = vue::parse_sfc(&src);
        let (Some(template), Some(&using_symbol)) = (sfc.template, using_symbol_by_path.get(&path))
        else {
            continue;
        };
        sites.push(vue::TemplateUsageSite {
            file: path,
            template: template.content,
            using_symbol,
        });
    }

    let components: Vec<vue::VueComponent> = parsed
        .iter()
        .filter_map(|(_p, r)| r.vue_component.clone())
        .collect();

    Ok(vue::find_binding_mismatches(
        &components,
        &sites,
        &local_scope,
        &global_components,
        &canonical_names,
    ))
}

/// Collects references while dropping exact duplicates (the same member accessed a hundred
/// times in one function is one edge).
struct EdgeSink<'a> {
    references: &'a mut Vec<Reference>,
    seen: HashSet<(SymbolId, SymbolId, RefKind)>,
}

impl EdgeSink<'_> {
    fn push(&mut self, reference: Reference) {
        if self
            .seen
            .insert((reference.from, reference.to, reference.kind))
        {
            self.references.push(reference);
        }
    }
}

impl JavaScriptAnalyzer {
    /// Swap the static call-graph edges for type-aware `typescript-language-server` ones.
    fn apply_resolution(
        &self,
        root: &Path,
        program: &mut Program,
        resolution: ResolutionMode,
        tsserver_executable: Option<&str>,
        timeout_ms: u64,
    ) -> Result<(), AnalyzeError> {
        if resolution == ResolutionMode::Static {
            return Ok(());
        }
        let executable = tsserver_executable
            .map(str::to_string)
            .unwrap_or_else(|| "typescript-language-server".to_string());
        let edges = tsserver::resolve_program_edges(
            root,
            program,
            Language::JavaScript,
            &executable,
            timeout_ms,
        )
        .map_err(|e| AnalyzeError::Io(std::io::Error::other(e.to_string())))?;
        // Replace the static call-graph edges between queryable symbols with
        // the type-aware ones (same swap as spectograph-py's pyright backend
        // and spectograph-ts's tsserver backend).
        let queryable: HashSet<SymbolId> = program
            .symbols
            .iter()
            .filter(|s| {
                matches!(
                    s.kind,
                    SymbolKind::Function | SymbolKind::Class | SymbolKind::Const
                )
            })
            .map(|s| s.id)
            .collect();
        program
            .references
            .retain(|r| !(queryable.contains(&r.from) && queryable.contains(&r.to)));
        for (from, to) in edges {
            program.references.push(Reference {
                from,
                to,
                kind: RefKind::Static,
                environment: Environment::Universal,
            });
        }
        Ok(())
    }

    /// Analyze with a resolution backend. `Static` is equivalent to
    /// [`Analyzer::analyze`].
    pub fn analyze_with_resolution(
        &self,
        root: &Path,
        resolution: ResolutionMode,
        tsserver_executable: Option<&str>,
        timeout_ms: u64,
    ) -> Result<Program, AnalyzeError> {
        let mut program = Analyzer::analyze(self, root)?;
        self.apply_resolution(
            root,
            &mut program,
            resolution,
            tsserver_executable,
            timeout_ms,
        )?;
        Ok(program)
    }

    /// The full `check` pipeline for JavaScript: the [`Program`], framework diagnostics
    /// (React prop mismatches, Vue prop/emit mismatches) from the same parse pass, and the
    /// optional type-aware resolution swap.
    pub fn analyze_full(
        &self,
        root: &Path,
        resolution: ResolutionMode,
        tsserver_executable: Option<&str>,
        timeout_ms: u64,
    ) -> Result<(Program, Vec<Diagnostic>), AnalyzeError> {
        let (mut program, parsed) = self.analyze_impl(root)?;
        let diagnostics = diagnostics::collect(&program, &parsed, root)?;
        self.apply_resolution(
            root,
            &mut program,
            resolution,
            tsserver_executable,
            timeout_ms,
        )?;
        Ok((program, diagnostics))
    }

    /// Like [`Analyzer::analyze`], but also returns React component/prop/hook
    /// data collected from JSX while parsing. Doesn't re-parse anything -- both
    /// results come from the same pass over the source.
    pub fn analyze_react(&self, root: &Path) -> Result<(Program, ReactAnalysis), AnalyzeError> {
        let (program, parsed) = self.analyze_impl(root)?;
        let mut react = ReactAnalysis::default();
        for (_path, result) in parsed {
            react.components.extend(result.components);
            react.jsx_usages.extend(result.jsx_usages);
            react.hook_calls.extend(result.hook_calls);
        }
        Ok((program, react))
    }

    /// Like [`Analyzer::analyze`], but also returns Vue component/prop/emit
    /// data collected from `.vue` files' script blocks while parsing. Doesn't
    /// re-parse anything -- both results come from the same pass over the
    /// source (see [`vue::VueComponent`] for what's detected).
    pub fn analyze_vue(&self, root: &Path) -> Result<(Program, VueAnalysis), AnalyzeError> {
        let (program, parsed) = self.analyze_impl(root)?;
        let mut vue = VueAnalysis::default();
        for (_path, result) in parsed {
            vue.components.extend(result.vue_component);
        }
        Ok((program, vue))
    }

    /// Like [`Analyzer::analyze`], but also returns every Vue template
    /// prop/emit mismatch found -- a `<user-card :extra="1">` (or `@bogus=`)
    /// usage passing a binding the target component doesn't declare via
    /// `props`/`defineProps`/`emits`/`defineEmits`. See `vue_mismatches`.
    pub fn analyze_vue_mismatches(
        &self,
        root: &Path,
    ) -> Result<(Program, Vec<vue::VueBindingMismatch>), AnalyzeError> {
        let (program, parsed) = self.analyze_impl(root)?;
        let mismatches = vue_mismatches(&program, &parsed, root)?;
        Ok((program, mismatches))
    }

    fn analyze_impl(
        &self,
        root: &Path,
    ) -> Result<(Program, Vec<(String, parse::FileResult)>), AnalyzeError> {
        let files = discover::discover_files(root, Language::JavaScript)?;
        let resolver = ProjectResolver::load(root);

        // `.vue` Single-File Components aren't owned by `Language::JavaScript`'s
        // extension list (see `vue::discover_vue_files`'s doc comment) -- only
        // the ones whose script block is plain JavaScript belong to this
        // analyzer; a `<script lang="ts">`/`<script setup lang="ts">` block is
        // the TypeScript analyzer's to parse. `materialize_script` gives back
        // source text with each block's lines at their true position in the
        // `.vue` file (blank elsewhere), so it can be parsed exactly like a
        // regular file below with no further changes.
        let mut file_sources: Vec<(PathBuf, String)> = Vec::new();
        for path in &files {
            let src = read_source(path)?;
            file_sources.push((path.clone(), src));
        }
        // Captured alongside `file_sources` for the Vue template-usage
        // reachability pass below, scoped to exactly the `.vue` files this
        // analyzer actually parses (a TS-lang file's template is scanned by
        // `TypeScriptAnalyzer` instead -- see that crate's mirror of this loop).
        let mut vue_templates: Vec<(PathBuf, String)> = Vec::new();
        // Paths whose `.vue` file has a `<script setup>` block -- threaded
        // into `parse_file` below since a bare `<script setup>` (no
        // `defineProps`/`defineEmits` macro call) is otherwise
        // indistinguishable from a plain non-Vue file once materialized.
        let mut vue_script_setup_files: HashSet<PathBuf> = HashSet::new();
        for path in vue::discover_vue_files(root)? {
            let src = read_source(&path)?;
            let sfc = vue::parse_sfc(&src);
            if let Some((materialized, vue::ScriptLang::JavaScript)) = vue::materialize_script(&sfc)
            {
                if let Some(template) = &sfc.template {
                    vue_templates.push((path.clone(), template.content.clone()));
                }
                if sfc.script_setup.is_some() {
                    vue_script_setup_files.insert(path.clone());
                }
                file_sources.push((path, materialized));
            }
        }
        if file_sources.is_empty() {
            return Err(AnalyzeError::NoSourceFiles(root.display().to_string()));
        }

        let mut program = Program {
            language: Language::JavaScript,
            root: root.to_path_buf(),
            symbols: Vec::new(),
            references: Vec::new(),
            entrypoints: Vec::new(),
            reachable: Vec::new(),
        };

        let mut next_id: usize = 0;
        // Global name -> symbol id (first definition wins), used to resolve names no scope
        // knows (the uncertain, by-name fallback).
        let mut global_names: HashMap<String, SymbolId> = HashMap::new();
        // Bundler `browser` field remapping, applied during module resolution below.
        let browser_map = read_browser_map(root);

        // Pass 1: parse each file, collect its symbols and unresolved refs.
        let mut parsed: Vec<(String, parse::FileResult)> = Vec::new();
        for (path, src) in &file_sources {
            let module_id = next_id;
            next_id += 1;

            let result = parse_file(
                path,
                src,
                module_id,
                &mut next_id,
                vue_script_setup_files.contains(path),
            );

            for sym in &result.symbols {
                if sym.kind != SymbolKind::Module {
                    // First definition wins for vertical slice
                    global_names.entry(sym.name.clone()).or_insert(sym.id);
                }
            }
            program.symbols.extend(result.symbols.clone());
            parsed.push((path.to_string_lossy().into_owned(), result));
        }

        // Build module path index: normalized file path -> module symbol id. Normalized so
        // `./src/a.js` (what a walk of `.` yields) and `src/a.js` (what resolution produces)
        // are the same key.
        let module_paths: HashMap<String, SymbolId> = program
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Module)
            .map(|s| (normalize_path(&s.file).to_string_lossy().into_owned(), s.id))
            .collect();

        // Build name_in_module index: (module_id, name) -> symbol_id
        let mut name_in_module: HashMap<(SymbolId, String), SymbolId> = HashMap::new();
        for (_path, result) in &parsed {
            for sym in &result.symbols {
                if sym.kind != SymbolKind::Module {
                    name_in_module
                        .entry((result.module_id, sym.name.clone()))
                        .or_insert(sym.id);
                }
            }
        }

        // What each module exposes, and which symbols a property access like `.run` can name.
        let exported_by_module: ExportedByModule = parsed
            .iter()
            .filter(|(_, r)| !r.exported.is_empty())
            .map(|(_, r)| (r.module_id, r.exported.clone()))
            .collect();
        let members_by_name = exports::member_index(&program, &exported_by_module);

        let mut edges = EdgeSink {
            references: &mut program.references,
            seen: HashSet::new(),
        };
        // (user symbol, module) pairs where the module's whole namespace is used.
        let mut whole_module_uses: Vec<(SymbolId, SymbolId)> = Vec::new();

        // Pass 2: resolve references within each file.
        for (path, result) in &parsed {
            let base = Path::new(path.as_str()).parent().unwrap_or(Path::new(""));
            for r in &result.refs {
                match &r.target {
                    parse::RefTarget::Symbol(id) => edges.push(Reference {
                        from: r.from,
                        to: *id,
                        kind: r.kind,
                        environment: r.environment,
                    }),
                    parse::RefTarget::Name(name) => {
                        if let Some(&to) = global_names.get(name) {
                            edges.push(Reference {
                                from: r.from,
                                to,
                                kind: r.kind,
                                environment: r.environment,
                            });
                        }
                    }
                    parse::RefTarget::Member(name) => {
                        for &to in members_by_name.get(name).into_iter().flatten() {
                            edges.push(Reference {
                                from: r.from,
                                to,
                                kind: r.kind,
                                environment: r.environment,
                            });
                        }
                    }
                    parse::RefTarget::Module(spec) | parse::RefTarget::ModuleNamespace(spec) => {
                        let (source_path, _env) =
                            resolve_module_path(base, spec, &browser_map, &resolver);
                        let key = normalize_path(&source_path).to_string_lossy().into_owned();
                        if let Some(&module) = module_paths.get(&key) {
                            edges.push(Reference {
                                from: r.from,
                                to: module,
                                kind: r.kind,
                                environment: r.environment,
                            });
                            if matches!(r.target, parse::RefTarget::ModuleNamespace(_)) {
                                whole_module_uses.push((r.from, module));
                            }
                        }
                    }
                }
            }
        }

        // Pass 3: resolve import references to their definitions.
        // For ES6: import {x} from "y" -> link Import symbol to definition in module y
        // For CommonJS: const x = require("y") -> link Import symbol to definition in module y
        for (_path, result) in &parsed {
            for imp in &result.imports {
                // Resolve the source module with Node.js-style resolution
                let (source_path, resolved_env) = result
                    .symbols
                    .iter()
                    .find(|s| s.kind == SymbolKind::Module)
                    .map(|s| s.file.parent().unwrap_or(&s.file))
                    .map(|p| resolve_module_path(p, &imp.source, &browser_map, &resolver))
                    .unwrap_or_else(|| (root.join(&imp.source), Environment::Universal));
                // An explicit guard (Tier 1) is a stronger signal than the browser-field
                // remap (which only tells us "this specifier differs in a browser build").
                let ref_env = if imp.environment != Environment::Universal {
                    imp.environment
                } else {
                    resolved_env
                };

                let source_path_str = normalize_path(&source_path).to_string_lossy().into_owned();

                // Find the target module in module_paths
                let Some(&target_module_id) = module_paths.get(&source_path_str) else {
                    // Not part of the analyzed tree (third-party package, unresolved path).
                    continue;
                };

                // The definition in the *target module* the import names: a default import
                // binds whatever the module's default export is, which by convention shares
                // the importing binding's name; a named import looks up the exported name.
                // (Never a same-named symbol elsewhere in the project -- that is an accident
                // of naming, not what the import refers to. And a *guessed* default must be
                // a declaration: the module may import something of the same name itself.)
                let mut linked = false;
                if imp.imported_name == "*" {
                    // Namespace import / `export * from`: every export of the module.
                    whole_module_uses.push((imp.symbol_id, target_module_id));
                    linked = true;
                } else {
                    let guessed_default = imp.imported_name == "default";
                    let lookup = if guessed_default {
                        &imp.local_name
                    } else {
                        &imp.imported_name
                    };
                    if let Some(&def_id) = name_in_module
                        .get(&(target_module_id, lookup.clone()))
                        .filter(|&&id| {
                            !guessed_default || program.symbols[id].kind != SymbolKind::Import
                        })
                    {
                        edges.push(Reference {
                            from: imp.symbol_id,
                            to: def_id,
                            kind: RefKind::ReExport,
                            environment: ref_env,
                        });
                        linked = true;
                    }
                }
                // A default import (or `require`) that names no definition is the whole
                // module object -- `const agent = require('./agent'); agent.get()`.
                if !linked && imp.is_default {
                    whole_module_uses.push((imp.symbol_id, target_module_id));
                }
                // Importing a module runs it, so it is reachable whenever the import is
                // (pushed after the definition edge so that stays the import's first edge).
                edges.push(Reference {
                    from: imp.symbol_id,
                    to: target_module_id,
                    kind: RefKind::ReExport,
                    environment: ref_env,
                });
            }
        }

        // Pass 3.6: resolve `const X = defineAsyncComponent(() => import(
        // './X.vue'))` bindings the same way a static import resolves, but
        // as a `Dynamic` edge -- this genuinely is a runtime code-split
        // boundary, not a statically-guaranteed-present import, the same
        // distinction `require`/dynamic-`import()` calls get elsewhere.
        // `build_vue_scope_maps` below picks this edge up alongside the
        // ordinary ReExport ones, registering `X` as template-usable
        // exactly like a regular import.
        for (path, result) in &parsed {
            let base = Path::new(path.as_str()).parent().unwrap_or(Path::new(""));
            for reg in &result.vue_async_components {
                let (source_path, _env) =
                    resolve_module_path(base, &reg.source, &browser_map, &resolver);
                let source_path_str = normalize_path(&source_path).to_string_lossy().into_owned();
                let Some(&target_module_id) = module_paths.get(&source_path_str) else {
                    continue;
                };
                let Some(target_component_id) = parsed
                    .iter()
                    .find(|(_p, r)| r.module_id == target_module_id)
                    .and_then(|(_p, r)| r.vue_component.as_ref())
                    .map(|c| c.symbol_id)
                else {
                    continue;
                };
                let Some(local_const_id) = result
                    .symbols
                    .iter()
                    .find(|s| s.name == reg.local_name && s.kind == SymbolKind::Const)
                    .map(|s| s.id)
                else {
                    continue;
                };
                edges.push(Reference {
                    from: local_const_id,
                    to: target_component_id,
                    kind: RefKind::Dynamic,
                    environment: Environment::Universal,
                });
            }
        }
        drop(edges);

        // Pass 3.7: Vue template usage (component tags, `:is` dynamic
        // bindings) -> reachability edges. A component's rendered template
        // can reference other components by tag, which none of the passes
        // above can see (not an import, not a call) -- same category of gap
        // Angular's template scanner fixes for `.ts`. See
        // `vue::collect_template_edges`.
        {
            let (local_scope, global_components, canonical_names) =
                build_vue_scope_maps(&program, &parsed);

            let using_symbol_by_path: HashMap<PathBuf, SymbolId> = parsed
                .iter()
                .map(|(path, result)| {
                    let using = result
                        .vue_component
                        .as_ref()
                        .map(|c| c.symbol_id)
                        .unwrap_or(result.module_id);
                    (PathBuf::from(path.as_str()), using)
                })
                .collect();

            let sites: Vec<vue::TemplateUsageSite> = vue_templates
                .into_iter()
                .filter_map(|(file, template)| {
                    using_symbol_by_path
                        .get(&file)
                        .map(|&using_symbol| vue::TemplateUsageSite {
                            file,
                            template,
                            using_symbol,
                        })
                })
                .collect();

            for (from, to) in vue::collect_template_edges(
                &sites,
                &local_scope,
                &global_components,
                &canonical_names,
            ) {
                program.references.push(Reference {
                    from,
                    to,
                    kind: RefKind::Static,
                    environment: Environment::Universal,
                });
            }

            // Script bindings (handlers, state, imported components) named in a template.
            template_bindings::add_template_binding_edges(&mut program, &sites);
        }

        // Pass 4: detect and mark entrypoints
        let detection = detect_entries(&program, root);
        program.entrypoints = detection.entrypoints;

        // A `createApp(...).mount(...)` file is always the app's real
        // entrypoint, whatever its name or `package.json`'s `main` field
        // say -- added on top of (not instead of) the detection above,
        // since a project can have other, unrelated real entrypoints too
        // (a library's own `main`, a CLI script, ...).
        for (_path, result) in &parsed {
            if result.vue_app_mount && !program.entrypoints.contains(&result.module_id) {
                program.entrypoints.push(result.module_id);
            }
        }

        // Pass 5: exported declarations of entry modules (a package's API, a route file, a
        // config or story read by a tool), and of modules used as a whole (`export *`,
        // `import * as`, `require`, `import()`), are used from outside.
        exports::add_export_edges(
            &mut program,
            &exported_by_module,
            &detection.api_roots,
            &whole_module_uses,
        );

        // Pass 6: constructors, framework lifecycle methods, overrides of external base
        // classes, and `React` under the classic JSX transform.
        implicit::add_implicit_edges(&mut program, &file_sources);

        Ok((program, parsed))
    }
}

impl Analyzer for JavaScriptAnalyzer {
    fn language(&self) -> Language {
        Language::JavaScript
    }

    fn analyze(&self, root: &Path) -> Result<Program, AnalyzeError> {
        self.analyze_impl(root).map(|(program, _parsed)| program)
    }

    fn analyze_with_diagnostics(
        &self,
        root: &Path,
    ) -> Result<(Program, Vec<Diagnostic>), AnalyzeError> {
        self.analyze_full(root, ResolutionMode::Static, None, 0)
    }
}
