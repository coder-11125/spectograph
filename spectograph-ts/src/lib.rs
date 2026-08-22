//! Spectograph TypeScript analyzer.
//!
//! Parses TypeScript source with tree-sitter into the shared [`Program`] IR.

pub mod angular;
pub mod entrypoints;
pub mod parse;

/// React support (component/prop/hook data types, naming-convention helpers,
/// and [`react::find_prop_mismatches`]) is shared with `spectograph-js` rather
/// than duplicated -- re-exported here so callers don't need that crate as a
/// separate explicit dependency just to use it.
pub use spectograph_js::react;
pub use spectograph_js::ReactAnalysis;

/// The `typescript-language-server`-backed type-aware resolution backend lives
/// in `spectograph-tsserver` since `spectograph-js` shares it (and can't
/// depend on this crate, which already depends on `spectograph-js`) --
/// re-exported here for ergonomic access.
pub use spectograph_tsserver as tsserver;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use parse::parse_file;
use serde_json::Value;
use spectograph_analyzer::{discover, AnalyzeError, Analyzer};
use spectograph_core::ir::{
    Environment, Language, Program, RefKind, Reference, SymbolId, SymbolKind,
};

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

/// `compilerOptions.baseUrl`/`paths` read from the project's `tsconfig.json`.
/// Missing or unparsable `tsconfig.json` (including the common case of it
/// containing JSONC comments, which `serde_json` can't parse) just yields an
/// empty config -- resolution falls back to relative/node_modules lookup only.
#[derive(Default)]
struct TsConfig {
    /// Resolved to an absolute path already (root joined with the raw `baseUrl`).
    base_url: Option<PathBuf>,
    paths: HashMap<String, Vec<String>>,
    /// Top-level `include`/`exclude` glob patterns, kept as `None` when the key
    /// is absent from `tsconfig.json` (as opposed to present-but-empty) so file
    /// filtering can tell "no tsconfig opinion here" from "matches nothing" --
    /// see [`filter_by_tsconfig`].
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
}

fn read_tsconfig(root: &Path) -> TsConfig {
    let Some(json) = std::fs::read_to_string(root.join("tsconfig.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
    else {
        return TsConfig::default();
    };
    let compiler_options = json.get("compilerOptions");
    let base_url = compiler_options
        .and_then(|c| c.get("baseUrl"))
        .and_then(|v| v.as_str())
        .map(|b| normalize_path(&root.join(b)));
    let paths = compiler_options
        .and_then(|c| c.get("paths"))
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| {
                    let targets: Vec<String> = v
                        .as_array()?
                        .iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect();
                    Some((k.clone(), targets))
                })
                .collect()
        })
        .unwrap_or_default();
    let string_array = |key: &str| {
        json.get(key).and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
    };
    let include = string_array("include");
    let exclude = string_array("exclude");
    TsConfig {
        base_url,
        paths,
        include,
        exclude,
    }
}

/// Build a matcher for a list of `tsconfig.json` `include`/`exclude` glob
/// patterns. A bare name with no glob metacharacters that names an existing
/// directory under `root` (e.g. `"node_modules"`) is expanded to match
/// everything under it too, the same way `tsc` treats such entries.
fn build_globset(patterns: &[String], root: &Path) -> Option<globset::GlobSet> {
    if patterns.is_empty() {
        return None;
    }
    let mut builder = globset::GlobSetBuilder::new();
    for pattern in patterns {
        if let Ok(glob) = globset::Glob::new(pattern) {
            builder.add(glob);
        }
        if !pattern.contains(['*', '?', '[']) && root.join(pattern).is_dir() {
            if let Ok(glob) = globset::Glob::new(&format!("{pattern}/**")) {
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

/// Match `specifier` against `paths` patterns (each may contain one `*`
/// wildcard), returning the substituted candidate target paths in the same
/// (unresolved-to-baseUrl) form they appear in `tsconfig.json`. An exact,
/// non-wildcard pattern always wins; among wildcard matches, the longest
/// literal prefix wins (TS's own tie-breaking rule).
fn resolve_paths_alias(
    specifier: &str,
    paths: &HashMap<String, Vec<String>>,
) -> Option<Vec<String>> {
    if let Some(targets) = paths.get(specifier) {
        return Some(targets.clone());
    }
    let mut best: Option<(usize, &str, &Vec<String>)> = None; // (prefix_len, middle, targets)
    for (pattern, targets) in paths {
        let Some(star) = pattern.find('*') else {
            continue;
        };
        let prefix = &pattern[..star];
        let suffix = &pattern[star + 1..];
        if specifier.len() < prefix.len() + suffix.len()
            || !specifier.starts_with(prefix)
            || !specifier.ends_with(suffix)
        {
            continue;
        }
        let middle = &specifier[prefix.len()..specifier.len() - suffix.len()];
        if best.map(|(len, _, _)| prefix.len() > len).unwrap_or(true) {
            best = Some((prefix.len(), middle, targets));
        }
    }
    let (_, middle, targets) = best?;
    Some(targets.iter().map(|t| t.replacen('*', middle, 1)).collect())
}

/// Collapse `.`/`..` components so a joined path's string form matches the plain
/// paths [`spectograph_analyzer::discover::discover_files`] produces.
pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir if result.file_name().is_some() => {
                result.pop();
            }
            other => result.push(other),
        }
    }
    result
}

/// Try `path` as-is, then with a TypeScript source extension, then as a
/// directory's index file. `.d.ts` is tried last since it's a type-only
/// declaration file -- a real `.ts`/`.tsx` implementation is preferred when
/// both exist.
pub(crate) fn resolve_file_candidate(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    // Append, don't replace: `Path::with_extension` treats the text after the
    // *last* dot in the file name as "the extension" and replaces it, which
    // silently breaks any specifier whose real file name contains a dot
    // before its actual extension (`./app.module` -> `app.module.ts`,
    // `./foo.service` -> `foo.service.ts`, ... -- Angular's own file-naming
    // convention for every file). `with_extension("ts")` on `app.module`
    // produces `app.ts`, not `app.module.ts`, so the guessed candidate never
    // exists and resolution fails outright.
    for ext in ["ts", "tsx", "d.ts"] {
        let candidate = PathBuf::from(format!("{}.{ext}", path.to_string_lossy()));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    if path.is_dir() {
        for name in ["index.ts", "index.tsx", "index.d.ts"] {
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
/// `.d.ts` extension guessing), `tsconfig.json` `paths`/`baseUrl` aliases (bare
/// specifiers only -- TS never remaps relative ones), then `node_modules`.
fn resolve_module_path(
    base: &Path,
    specifier: &str,
    root: &Path,
    tsconfig: &TsConfig,
) -> Option<PathBuf> {
    if specifier.starts_with('/') {
        return resolve_file_candidate(&PathBuf::from(specifier));
    }
    if specifier.starts_with('.') {
        return resolve_file_candidate(&normalize_path(&base.join(specifier)));
    }

    if let Some(candidates) = resolve_paths_alias(specifier, &tsconfig.paths) {
        let paths_base = tsconfig.base_url.as_deref().unwrap_or(root);
        for candidate in &candidates {
            if let Some(resolved) =
                resolve_file_candidate(&normalize_path(&paths_base.join(candidate)))
            {
                return Some(resolved);
            }
        }
    }

    if let Some(base_url) = &tsconfig.base_url {
        if let Some(resolved) = resolve_file_candidate(&normalize_path(&base_url.join(specifier))) {
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

impl TypeScriptAnalyzer {
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
        if resolution == ResolutionMode::Static {
            return Ok(program);
        }
        let executable = tsserver_executable
            .map(str::to_string)
            .unwrap_or_else(|| "typescript-language-server".to_string());
        let edges = tsserver::resolve_program_edges(
            root,
            &program,
            Language::TypeScript,
            &executable,
            timeout_ms,
        )
        .map_err(|e| AnalyzeError::Io(std::io::Error::other(e.to_string())))?;
        // Replace the static call-graph edges between queryable symbols with
        // the type-aware ones (same swap as spectograph-py's pyright backend).
        let queryable: std::collections::HashSet<SymbolId> = program
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
        Ok(program)
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

    fn analyze_impl(
        &self,
        root: &Path,
    ) -> Result<(Program, Vec<(PathBuf, parse::FileResult)>), AnalyzeError> {
        let tsconfig = read_tsconfig(root);
        let files = filter_by_tsconfig(
            discover::discover_files(root, Language::TypeScript)?,
            root,
            &tsconfig,
        );
        if files.is_empty() {
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
        // Global name -> symbol id (first definition wins, matching the JS/Python
        // analyzers' "vertical slice" resolution convention).
        let mut global_names: HashMap<String, SymbolId> = HashMap::new();
        let mut def_index: HashMap<String, SymbolId> = HashMap::new();

        let mut parsed: Vec<(PathBuf, parse::FileResult)> = Vec::new();
        for path in &files {
            let src = std::fs::read_to_string(path)?;
            let module_id = next_id;
            next_id += 1;

            let result = parse_file(path, &src, module_id, &mut next_id);

            for sym in &result.symbols {
                if sym.kind != SymbolKind::Module {
                    global_names.entry(sym.name.clone()).or_insert(sym.id);
                    if matches!(
                        sym.kind,
                        SymbolKind::Function | SymbolKind::Class | SymbolKind::Const
                    ) {
                        def_index.entry(sym.name.clone()).or_insert(sym.id);
                    }
                }
            }
            program.symbols.extend(result.symbols.clone());
            parsed.push((path.clone(), result));
        }

        let module_paths: HashMap<String, SymbolId> = program
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Module)
            .map(|s| (s.file.to_string_lossy().into_owned(), s.id))
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

        // Pass: resolve in-file references (calls, plain identifiers).
        for (_path, result) in &parsed {
            for r in &result.refs {
                let to = match &r.target {
                    parse::RefTarget::Symbol(id) => Some(*id),
                    parse::RefTarget::Name(name) => global_names.get(name).copied(),
                };
                if let Some(to) = to {
                    program.references.push(Reference {
                        from: r.from,
                        to,
                        kind: r.kind,
                        environment: Environment::Universal,
                    });
                }
            }
        }

        // Pass: resolve imports (including the "from" half of re-exports) to
        // their definitions in the source module.
        for (path, result) in &parsed {
            let base = path.parent().unwrap_or(path);
            for imp in &result.imports {
                let Some(source_path) = resolve_module_path(base, &imp.source, root, &tsconfig)
                else {
                    continue;
                };
                let source_path_str = source_path.to_string_lossy().into_owned();
                let Some(&target_module_id) = module_paths.get(&source_path_str) else {
                    continue;
                };

                if imp.imported_name == "*" {
                    // Namespace import / `export * from` / `export * as ns from`:
                    // link straight to the target module itself.
                    program.references.push(Reference {
                        from: imp.symbol_id,
                        to: target_module_id,
                        kind: RefKind::ReExport,
                        environment: Environment::Universal,
                    });
                    continue;
                }

                if let Some(&def_id) =
                    name_in_module.get(&(target_module_id, imp.imported_name.clone()))
                {
                    program.references.push(Reference {
                        from: imp.symbol_id,
                        to: def_id,
                        kind: RefKind::ReExport,
                        environment: Environment::Universal,
                    });
                } else if imp.is_default {
                    if let Some(&def_id) = def_index.get(&imp.local_name) {
                        program.references.push(Reference {
                            from: imp.symbol_id,
                            to: def_id,
                            kind: RefKind::ReExport,
                            environment: Environment::Universal,
                        });
                    }
                }
            }
        }

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

        program.entrypoints = entrypoints::detect_entrypoint_symbols(&program, root);

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
}
