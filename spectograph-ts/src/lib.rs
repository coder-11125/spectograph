//! Spectograph TypeScript analyzer.
//!
//! Parses TypeScript source with tree-sitter into the shared [`Program`] IR.

pub mod angular;
pub mod diagnostics;
pub mod entrypoints;
pub mod parse;

/// React support (component/prop/hook data types, naming-convention helpers,
/// and [`react::find_prop_mismatches`]) is shared with `spectograph-js` rather
/// than duplicated -- re-exported here so callers don't need that crate as a
/// separate explicit dependency just to use it.
pub use spectograph_js::react;
pub use spectograph_js::ReactAnalysis;

/// Vue SFC (`.vue`) splitting/materialization is shared with `spectograph-js`
/// for the same reason React support is -- see [`react`]'s doc comment.
pub use spectograph_js::vue;

/// The `typescript-language-server`-backed type-aware resolution backend lives
/// in `spectograph-tsserver` since `spectograph-js` shares it (and can't
/// depend on this crate, which already depends on `spectograph-js`) --
/// re-exported here for ergonomic access.
pub use spectograph_tsserver as tsserver;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use parse::parse_file;
use serde_json::Value;
use spectograph_analyzer::{discover, AnalyzeError, Analyzer};
use spectograph_core::diagnostic::Diagnostic;
use spectograph_core::ir::{
    Environment, Language, Program, RefKind, Reference, SymbolId, SymbolKind,
};
use spectograph_js::aliases::ProjectResolver;
use spectograph_js::entrypoints::detect_entries;
use spectograph_js::exports::{self, ExportedByModule};
use spectograph_js::pathutil::normalize_path;
use spectograph_js::tsconfig::TsConfig;
use spectograph_js::{implicit, template_bindings};

/// Analyzer for the TypeScript language (`.ts`/`.tsx`/`.mts`/`.cts`).
pub struct TypeScriptAnalyzer;

/// Which reference-resolution backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionMode {
    /// Hand-rolled static resolver (scope, module resolution, DI edges).
    Static,
    /// `typescript-language-server` type-aware resolver (via LSP find-references).
    Tsserver,
}

/// Read a source file as text, replacing invalid UTF-8 rather than failing: a single
/// latin-1 or binary-ish file in a repo must not abort the analysis of everything else.
fn read_source(path: &Path) -> std::io::Result<String> {
    std::fs::read(path).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// Compile one `tsconfig.json` glob. As in `tsc`, `*` and `?` stay within a single path
/// segment (`scripts/*` is the files directly in `scripts/`, not its subdirectories); only
/// `**` crosses directories.
fn tsconfig_glob(pattern: &str) -> Option<globset::Glob> {
    globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .ok()
}

/// Build a matcher for a list of `tsconfig.json` `include`/`exclude` glob patterns.
///
/// `tsc` treats a pattern whose last path segment has no extension and no wildcard
/// (`"src"`, `"packages/*/src"`, `"node_modules"`) as a *directory* and includes everything
/// beneath it, so those get a `/**` companion. (A literal existing directory with a dot in
/// its name is covered too.) Without this, `"packages/*/src"` -- the standard monorepo
/// include -- would match only the directories themselves and silently drop every file.
fn build_globset(patterns: &[String], root: &Path) -> Option<globset::GlobSet> {
    if patterns.is_empty() {
        return None;
    }
    let mut builder = globset::GlobSetBuilder::new();
    for pattern in patterns {
        if let Some(glob) = tsconfig_glob(pattern) {
            builder.add(glob);
        }
        let trimmed = pattern.trim_end_matches('/');
        let last_segment = trimmed.rsplit('/').next().unwrap_or(trimmed);
        let is_directory_pattern = !last_segment.contains(['.', '*', '?', '[']);
        let is_existing_directory =
            !trimmed.contains(['*', '?', '[']) && root.join(trimmed).is_dir();
        if is_directory_pattern || is_existing_directory {
            if let Some(glob) = tsconfig_glob(&format!("{trimmed}/**")) {
                builder.add(glob);
            }
        }
    }
    builder.build().ok()
}

/// Filter discovered files by the project's `tsconfig.json` `include`/`exclude`
/// patterns, matched against each file's path relative to `root`. When
/// `tsconfig.json` has neither key (including when there's no `tsconfig.json`
/// at all), `files` passes through unfiltered -- this deliberately does *not*
/// apply `tsc`'s own implicit default exclude of `node_modules`, since we
/// intentionally parse `node_modules` sources to resolve and mark third-party
/// symbols (see `is_external` handling in `parse.rs`); only an *explicit*
/// exclude in a real `tsconfig.json` overrides that.
fn filter_by_tsconfig(files: Vec<PathBuf>, root: &Path, tsconfig: &TsConfig) -> Vec<PathBuf> {
    if tsconfig.include.is_none() && tsconfig.exclude.is_none() {
        return files;
    }
    let include_set = tsconfig
        .include
        .as_deref()
        .and_then(|p| build_globset(p, root));
    let exclude_set = tsconfig
        .exclude
        .as_deref()
        .and_then(|p| build_globset(p, root));
    files
        .into_iter()
        .filter(|f| {
            let rel = f.strip_prefix(root).unwrap_or(f);
            if exclude_set.as_ref().is_some_and(|s| s.is_match(rel)) {
                return false;
            }
            include_set.as_ref().is_none_or(|s| s.is_match(rel))
        })
        .collect()
}

/// TypeScript's ESM convention: an import written `./foo.js` names the source file
/// `./foo.ts` (the specifier is what the *compiled* output will be called).
const JS_TO_TS_EXTENSIONS: &[(&str, &[&str])] = &[
    (".js", &[".ts", ".tsx", ".d.ts"]),
    (".jsx", &[".tsx"]),
    (".mjs", &[".mts", ".d.mts"]),
    (".cjs", &[".cts", ".d.cts"]),
];

/// Try `path` as-is, then as the TS source behind a `.js`-style specifier, then with a
/// TypeScript source extension, then as a directory's index file. `.d.ts` is tried last
/// since it's a type-only declaration file -- a real `.ts`/`.tsx` implementation is
/// preferred when both exist.
fn resolve_file_candidate(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    let text = path.to_string_lossy();
    for (js_ext, ts_exts) in JS_TO_TS_EXTENSIONS {
        if let Some(stem) = text.strip_suffix(js_ext) {
            for ts_ext in *ts_exts {
                let candidate = PathBuf::from(format!("{stem}{ts_ext}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    // Append, don't replace: `Path::with_extension` treats the text after the
    // *last* dot in the file name as "the extension" and replaces it, which
    // silently breaks any specifier whose real file name contains a dot
    // before its actual extension (`./app.module` -> `app.module.ts`,
    // `./foo.service` -> `foo.service.ts`, ... -- Angular's own file-naming
    // convention for every file). `with_extension("ts")` on `app.module`
    // produces `app.ts`, not `app.module.ts`, so the guessed candidate never
    // exists and resolution fails outright.
    for ext in ["ts", "tsx", "mts", "cts", "vue", "d.ts"] {
        let candidate = PathBuf::from(format!("{text}.{ext}"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    if path.is_dir() {
        for name in [
            "index.ts",
            "index.tsx",
            "index.mts",
            "index.cts",
            "index.vue",
            "index.d.ts",
        ] {
            let candidate = path.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Resolve a `node_modules` package directory to its entry-point file, preferring
/// the TS-specific `types`/`typings` fields (most `@types/*` and TS-authored
/// packages declare one of these), then `main`, then a bare index file.
fn resolve_package_entry(pkg_dir: &Path) -> Option<PathBuf> {
    if let Ok(content) = std::fs::read_to_string(pkg_dir.join("package.json")) {
        if let Ok(json) = serde_json::from_str::<Value>(&content) {
            let entry = json
                .get("types")
                .or_else(|| json.get("typings"))
                .or_else(|| json.get("main"))
                .and_then(|v| v.as_str());
            if let Some(entry) = entry {
                if let Some(resolved) =
                    resolve_file_candidate(&normalize_path(&pkg_dir.join(entry)))
                {
                    return Some(resolved);
                }
            }
        }
    }
    resolve_file_candidate(&pkg_dir.join("index"))
}

/// Node.js/TypeScript-style module resolution: relative paths (with `.ts`/`.tsx`/
/// `.d.ts` extension guessing and `./x.js` -> `x.ts` mapping), then project aliases
/// (`tsconfig` `paths`/`baseUrl`, `package.json` `imports`, monorepo workspace packages,
/// `@/`-style conventions -- bare specifiers only, TS never remaps relative ones), then
/// `node_modules`.
fn resolve_module_path(
    base: &Path,
    specifier: &str,
    resolver: &ProjectResolver,
) -> Option<PathBuf> {
    if specifier.starts_with('/') {
        return resolve_file_candidate(&PathBuf::from(specifier));
    }
    if specifier.starts_with('.') {
        return resolve_file_candidate(&normalize_path(&base.join(specifier)));
    }

    for candidate in resolver.candidates(specifier) {
        if let Some(resolved) = resolve_file_candidate(&candidate) {
            return Some(resolved);
        }
    }

    let mut current = base;
    loop {
        let candidate = current.join("node_modules").join(specifier);
        if candidate.is_dir() {
            if let Some(entry) = resolve_package_entry(&candidate) {
                return Some(entry);
            }
        } else if let Some(resolved) = resolve_file_candidate(&candidate) {
            return Some(resolved);
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => break,
        }
    }
    None
}

/// Build the project-wide Angular selector list, with `@Input`/`@Output`
/// binding names merged in from every file's decorators. Shared by the
/// template-usage reachability-edge pass (`analyze_impl`) and
/// `analyze_angular`'s binding-mismatch check, so the two don't drift.
fn build_angular_selectors(
    parsed: &[(PathBuf, parse::FileResult)],
    global_names: &HashMap<String, SymbolId>,
) -> Vec<angular::AngularSelector> {
    let mut declared_bindings: HashMap<SymbolId, Vec<String>> = HashMap::new();
    for (_path, result) in parsed {
        for dec in &result.decorators {
            if !matches!(dec.name.as_str(), "Input" | "Output") {
                continue;
            }
            let parse::DecoratorTarget::Symbol(prop_id) = &dec.target else {
                continue;
            };
            let Some(prop_sym) = result.symbols.iter().find(|s| s.id == *prop_id) else {
                continue;
            };
            // Property symbols are always named "ClassName.propName".
            let Some((class_name, prop_name)) = prop_sym.name.rsplit_once('.') else {
                continue;
            };
            let Some(&class_id) = global_names.get(class_name) else {
                continue;
            };
            // `@Input('customName')` binds under the alias, not the
            // property's own name.
            let binding_name = dec
                .arguments
                .first()
                .map(|a| parse::strip_quotes(a))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| prop_name.to_string());
            declared_bindings
                .entry(class_id)
                .or_default()
                .push(binding_name);
        }
    }

    parsed
        .iter()
        .flat_map(|(_path, result)| result.angular_selectors.iter().cloned())
        .map(|mut s| {
            if let Some(bindings) = declared_bindings.get(&s.class_id) {
                s.declared_bindings = bindings.clone();
            }
            s
        })
        .collect()
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
/// `analyze_impl` and [`TypeScriptAnalyzer::analyze_vue_mismatches`], so
/// the two can't silently drift apart -- same role as
/// `build_angular_selectors` above. Mirrors `spectograph_js`'s copy of this
/// function exactly, differing only in that `parsed`'s file keys are
/// already `PathBuf` here.
fn build_vue_scope_maps(
    program: &Program,
    parsed: &[(PathBuf, parse::FileResult)],
) -> VueScopeMaps {
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

    let mut local_scope: HashMap<(PathBuf, String), SymbolId> = HashMap::new();
    for (path, result) in parsed {
        for imp in &result.imports {
            if let Some(&target) = first_reexport.get(&imp.symbol_id) {
                if vue_component_ids.contains(&target) {
                    local_scope.insert((path.clone(), imp.local_name.clone()), target);
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
                    local_scope.insert((path.clone(), reg.local_name.clone()), target);
                }
            }
        }
    }

    let mut global_components: HashMap<String, SymbolId> = HashMap::new();
    for (path, result) in parsed {
        for reg in &result.vue_global_registrations {
            if let Some(&target) = local_scope.get(&(path.clone(), reg.local_identifier.clone())) {
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

/// Every Vue template prop/emit mismatch in an already-analyzed project. See
/// `spectograph_js`'s copy of this function.
pub(crate) fn vue_mismatches(
    program: &Program,
    parsed: &[(PathBuf, parse::FileResult)],
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
            (path.clone(), using)
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
    fn push(&mut self, from: SymbolId, to: SymbolId, kind: RefKind) {
        if self.seen.insert((from, to, kind)) {
            self.references.push(Reference {
                from,
                to,
                kind,
                environment: Environment::Universal,
            });
        }
    }
}

impl TypeScriptAnalyzer {
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
            Language::TypeScript,
            &executable,
            timeout_ms,
        )
        .map_err(|e| AnalyzeError::Io(std::io::Error::other(e.to_string())))?;
        // Replace the static call-graph edges between queryable symbols with
        // the type-aware ones (same swap as spectograph-py's pyright backend).
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

    /// The full `check` pipeline for TypeScript: the [`Program`], framework diagnostics
    /// (React prop, Angular binding, and Vue prop/emit mismatches) from the same parse pass,
    /// and the optional type-aware resolution swap.
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

    /// Like [`Analyzer::analyze`], but also returns every decorator found while
    /// parsing (`@Component(...)`, `@Injectable()`, `@Input()`, parameter
    /// decorators, ...), captured as structured but uninterpreted data -- see
    /// [`parse::DecoratorInfo`]. Doesn't re-parse anything; both results come
    /// from the same pass over the source.
    pub fn analyze_decorators(
        &self,
        root: &Path,
    ) -> Result<(Program, Vec<parse::DecoratorInfo>), AnalyzeError> {
        let (program, parsed) = self.analyze_impl(root)?;
        let decorators = parsed
            .into_iter()
            .flat_map(|(_path, result)| result.decorators)
            .collect();
        Ok((program, decorators))
    }

    /// Like [`Analyzer::analyze`], but also returns React component/prop/hook
    /// data collected from JSX (in both `.tsx` and, for completeness, `.ts`)
    /// while parsing -- see [`spectograph_js::ReactAnalysis`]. Reuses that
    /// crate's types directly rather than duplicating them: JSX parses to the
    /// same node shapes under the TSX grammar dialect, and component/hook
    /// naming conventions aren't JS-vs-TS specific. Doesn't re-parse anything.
    pub fn analyze_react(
        &self,
        root: &Path,
    ) -> Result<(Program, spectograph_js::ReactAnalysis), AnalyzeError> {
        let (program, parsed) = self.analyze_impl(root)?;
        let mut react = spectograph_js::ReactAnalysis::default();
        for (_path, result) in parsed {
            react.components.extend(result.components);
            react.jsx_usages.extend(result.jsx_usages);
            react.hook_calls.extend(result.hook_calls);
        }
        Ok((program, react))
    }

    /// Like [`Analyzer::analyze`], but also returns every Angular template
    /// binding mismatch found -- a `<app-child [foo]="x">` (or `(bar)=`,
    /// `*directive=`) usage passing a binding the target component/directive
    /// doesn't declare via `@Input()`/`@Output()`. Doesn't re-parse anything;
    /// reuses the same selector/binding data the template-usage reachability
    /// edges (already part of the main `analyze`/`Analyzer::analyze` result)
    /// are built from.
    pub fn analyze_angular(
        &self,
        root: &Path,
    ) -> Result<(Program, Vec<angular::BindingMismatch>), AnalyzeError> {
        let (program, parsed) = self.analyze_impl(root)?;
        let mut global_names: HashMap<String, SymbolId> = HashMap::new();
        for sym in &program.symbols {
            if sym.kind != SymbolKind::Module {
                global_names.entry(sym.name.clone()).or_insert(sym.id);
            }
        }
        let selectors = build_angular_selectors(&parsed, &global_names);
        let mismatches = angular::find_binding_mismatches(&selectors);
        Ok((program, mismatches))
    }

    /// Like [`Analyzer::analyze`], but also returns Vue component/prop/emit
    /// data collected from `.vue` files' `lang="ts"` script blocks while
    /// parsing. Doesn't re-parse anything -- reuses `spectograph_js`'s
    /// [`spectograph_js::VueAnalysis`] type directly rather than duplicating
    /// it, the same as [`Self::analyze_react`] does for React data.
    pub fn analyze_vue(
        &self,
        root: &Path,
    ) -> Result<(Program, spectograph_js::VueAnalysis), AnalyzeError> {
        let (program, parsed) = self.analyze_impl(root)?;
        let mut vue = spectograph_js::VueAnalysis::default();
        for (_path, result) in parsed {
            vue.components.extend(result.vue_component);
        }
        Ok((program, vue))
    }

    /// Like [`Analyzer::analyze`], but also returns every Vue template
    /// prop/emit mismatch found. Mirrors
    /// [`spectograph_js::JavaScriptAnalyzer::analyze_vue_mismatches`]
    /// exactly -- see its doc comment.
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
    ) -> Result<(Program, Vec<(PathBuf, parse::FileResult)>), AnalyzeError> {
        let resolver = ProjectResolver::load(root);
        let files = filter_by_tsconfig(
            discover::discover_files(root, Language::TypeScript)?,
            root,
            resolver.tsconfig(),
        );

        // `.vue` Single-File Components aren't owned by `Language::TypeScript`'s
        // extension list -- only the ones with a `lang="ts"` script block
        // belong to this analyzer; a plain (untyped) `<script>`/`<script setup>`
        // is `spectograph_js::JavaScriptAnalyzer`'s to parse. Not run through
        // `filter_by_tsconfig`: a tsconfig covering a Vue project would need an
        // explicit `"**/*.vue"` include entry to match here, which is a
        // refinement for later rather than this foundational wiring.
        // `materialize_script` gives back source text with each script block's
        // lines at their true position in the `.vue` file (blank elsewhere),
        // parseable exactly like a regular file below.
        let mut file_sources: Vec<(PathBuf, String)> = Vec::new();
        for path in &files {
            let src = read_source(path)?;
            file_sources.push((path.clone(), src));
        }
        // Captured alongside `file_sources` for the Vue template-usage
        // reachability pass below, scoped to exactly the `.vue` files this
        // analyzer actually parses (an untyped file's template is scanned by
        // `JavaScriptAnalyzer` instead -- see that crate's mirror of this loop).
        let mut vue_templates: Vec<(PathBuf, String)> = Vec::new();
        // Paths whose `.vue` file has a `<script setup>` block -- threaded
        // into `parse_file` below since a bare `<script setup>` (no
        // `defineProps`/`defineEmits` macro call) is otherwise
        // indistinguishable from a plain non-Vue file once materialized.
        let mut vue_script_setup_files: HashSet<PathBuf> = HashSet::new();
        for path in vue::discover_vue_files(root)? {
            let src = read_source(&path)?;
            let sfc = vue::parse_sfc(&src);
            if let Some((materialized, vue::ScriptLang::TypeScript)) = vue::materialize_script(&sfc)
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
            language: Language::TypeScript,
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

        let mut parsed: Vec<(PathBuf, parse::FileResult)> = Vec::new();
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
                    global_names.entry(sym.name.clone()).or_insert(sym.id);
                }
            }
            program.symbols.extend(result.symbols.clone());
            parsed.push((path.clone(), result));
        }

        // Normalized so `./src/a.ts` (what a walk of `.` yields) and `src/a.ts` (what
        // resolution produces) are the same key.
        let module_paths: HashMap<String, SymbolId> = program
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Module)
            .map(|s| (normalize_path(&s.file).to_string_lossy().into_owned(), s.id))
            .collect();

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

        // Pass: resolve in-file references (calls, plain identifiers, member accesses,
        // module loads).
        for (path, result) in &parsed {
            let base = path.parent().unwrap_or(path);
            for r in &result.refs {
                match &r.target {
                    parse::RefTarget::Symbol(id) => edges.push(r.from, *id, r.kind),
                    parse::RefTarget::Name(name) => {
                        if let Some(&to) = global_names.get(name) {
                            edges.push(r.from, to, r.kind);
                        }
                    }
                    parse::RefTarget::Member(name) => {
                        for &to in members_by_name.get(name).into_iter().flatten() {
                            edges.push(r.from, to, r.kind);
                        }
                    }
                    parse::RefTarget::Module(spec) | parse::RefTarget::ModuleNamespace(spec) => {
                        let Some(source_path) = resolve_module_path(base, spec, &resolver) else {
                            continue;
                        };
                        let key = normalize_path(&source_path).to_string_lossy().into_owned();
                        if let Some(&module) = module_paths.get(&key) {
                            edges.push(r.from, module, r.kind);
                            if matches!(r.target, parse::RefTarget::ModuleNamespace(_)) {
                                whole_module_uses.push((r.from, module));
                            }
                        }
                    }
                }
            }
        }

        // Pass: resolve imports (including the "from" half of re-exports) to
        // their definitions in the source module.
        for (path, result) in &parsed {
            let base = path.parent().unwrap_or(path);
            for imp in &result.imports {
                let Some(source_path) = resolve_module_path(base, &imp.source, &resolver) else {
                    continue;
                };
                let source_path_str = normalize_path(&source_path).to_string_lossy().into_owned();
                let Some(&target_module_id) = module_paths.get(&source_path_str) else {
                    continue;
                };

                // The definition in the *target module* the import names (never a
                // same-named symbol elsewhere in the project -- that is an accident of
                // naming, not what the import refers to). A default import's definition
                // is a `default` export, or a declaration sharing the binding's name
                // (`export default class Greeter {}` imported as `Greeter`) -- a
                // *declaration*, never an import binding of the target module that merely
                // happens to share the name.
                let mut linked = false;
                if imp.imported_name == "*" {
                    // Namespace import / `export * from` / `export * as ns from`: every
                    // export of the module is in play.
                    whole_module_uses.push((imp.symbol_id, target_module_id));
                    linked = true;
                } else if let Some(&def_id) =
                    name_in_module.get(&(target_module_id, imp.imported_name.clone()))
                {
                    edges.push(imp.symbol_id, def_id, RefKind::ReExport);
                    linked = true;
                } else if imp.is_default {
                    if let Some(&def_id) = name_in_module
                        .get(&(target_module_id, imp.local_name.clone()))
                        .filter(|&&id| program.symbols[id].kind != SymbolKind::Import)
                    {
                        edges.push(imp.symbol_id, def_id, RefKind::ReExport);
                        linked = true;
                    }
                }
                // A default import that names no definition is the module's anonymous
                // default export (`export default { ... }`, `export default connect(...)`),
                // which is the whole module as far as this analysis can tell.
                if !linked && imp.is_default {
                    whole_module_uses.push((imp.symbol_id, target_module_id));
                }
                // Importing a module runs it, so it is reachable whenever the import is
                // (pushed after the definition edge so that stays the import's first edge).
                edges.push(imp.symbol_id, target_module_id, RefKind::ReExport);
            }
        }

        // Pass: resolve `const X = defineAsyncComponent(() => import(
        // './X.vue'))` bindings the same way a static import resolves, but
        // as a `Dynamic` edge -- this genuinely is a runtime code-split
        // boundary, not a statically-guaranteed-present import. Mirrors
        // `spectograph_js`'s copy of this pass exactly, differing only in
        // that `parsed`'s file keys are already `PathBuf` here and
        // `resolve_module_path` takes a project resolver instead of a
        // browser-field map.
        for (path, result) in &parsed {
            let base = path.parent().unwrap_or(path);
            for reg in &result.vue_async_components {
                let Some(source_path) = resolve_module_path(base, &reg.source, &resolver) else {
                    continue;
                };
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
                edges.push(local_const_id, target_component_id, RefKind::Dynamic);
            }
        }
        drop(edges);

        // Pass: Angular template usage (component/directive tags and
        // attributes, pipe names) -> reachability edges. Project-wide, unlike
        // the other per-file decorator passes: a component's template
        // commonly references a component/directive/pipe declared in a
        // different file, so the selector/pipe maps have to be built from
        // every file's decorators before any template can be matched against
        // them.
        let all_selectors = build_angular_selectors(&parsed, &global_names);
        let all_pipes: Vec<angular::AngularPipe> = parsed
            .iter()
            .flat_map(|(_path, result)| result.angular_pipes.iter().cloned())
            .collect();
        for (from, to) in angular::collect_template_edges(&all_selectors, &all_pipes) {
            program.references.push(Reference {
                from,
                to,
                kind: RefKind::Static,
                environment: Environment::Universal,
            });
        }

        // Pass: Vue template usage (component tags, `:is` dynamic bindings)
        // -> reachability edges. See `vue::collect_template_edges`.
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
                    (path.clone(), using)
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

        let detection = detect_entries(&program, root);
        program.entrypoints = detection.entrypoints;

        // A `createApp(...).mount(...)` file is always the app's real
        // entrypoint, whatever its name or `package.json`'s `main` field
        // say -- added on top of (not instead of) the detection above, so a
        // Vue bootstrap file that isn't named `main.ts`/`index.ts` and isn't
        // `package.json`'s `main` is never missed.
        for (_path, result) in &parsed {
            if result.vue_app_mount && !program.entrypoints.contains(&result.module_id) {
                program.entrypoints.push(result.module_id);
            }
        }

        // Exported declarations of public-API entry modules, and of modules used as a whole
        // (`export *`, `import * as`, `import()`), are used from outside the analyzed code.
        exports::add_export_edges(
            &mut program,
            &exported_by_module,
            &detection.api_roots,
            &whole_module_uses,
        );

        // Constructors, framework lifecycle methods, overrides of external base classes,
        // and `React` under the classic JSX transform.
        implicit::add_implicit_edges(&mut program, &file_sources);

        Ok((program, parsed))
    }
}

impl Analyzer for TypeScriptAnalyzer {
    fn language(&self) -> Language {
        Language::TypeScript
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
