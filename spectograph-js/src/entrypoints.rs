//! Entrypoint detection for JavaScript and TypeScript projects.
//!
//! A JS/TS project has no single `main()`. Its real roots of usage are declared in packaging
//! config (`package.json` `main`/`module`/`bin`/`exports`/`scripts`), in an HTML shell
//! (`<script type="module" src="/src/main.tsx">`), in `angular.json`, or implied by framework
//! file conventions (Next.js `app/**/page.tsx`, Nuxt `pages/`, SvelteKit `src/routes/`, ...),
//! test runners, and tool configs. This module finds all of them, per `package.json` so
//! monorepo workspace members each get their own roots.
//!
//! Entrypoints split into two strengths:
//!
//! - **primary**: package-declared files and framework/convention roots. If none are found
//!   at all the project has no recognizable entry convention, so every non-external module
//!   becomes a root rather than reporting the whole codebase as bloat.
//! - **secondary**: tests, stories, tool configs, ambient `.d.ts`, scripts, examples. Always
//!   roots (their imports are real usage), but never enough by themselves to conclude the
//!   rest of the project is dead.
//!
//! Every entrypoint is also a *public surface*: what it exports is consumed by something
//! outside the analyzed code -- a package's users, the framework's router, a test runner
//! (`export default` of a config, a story's `export const Primary`, a fixture's
//! `FIXTURE_ENTRYPOINT`) -- so its exported declarations are roots too. See
//! [`EntryDetection::api_roots`].

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;
use spectograph_core::ir::{Program, SymbolId, SymbolKind};
use thiserror::Error;

use crate::aliases::find_package_jsons;
use crate::jsonc;
use crate::pathutil::{join_norm, normalize_path, parent_dir, rel_slash};

/// Errors from entrypoint detection.
#[derive(Debug, Error)]
pub enum EntrypointError {
    #[error("I/O error reading package.json: {0}")]
    Io(#[from] std::io::Error),
    #[error("Invalid JSON in package.json: {0}")]
    Json(#[from] serde_json::Error),
}

/// A detected entrypoint: module file path that should be marked as reachable.
#[derive(Debug, Clone)]
pub struct Entrypoint {
    pub path: String,
}

/// Detect entrypoint module IDs from the program (JS analyzer entry point; the file-module
/// map is no longer needed -- module files are matched by normalized relative path).
pub fn detect_entrypoint_symbols(
    program: &Program,
    _file_module: &HashMap<String, SymbolId>,
    root: &Path,
) -> Result<Vec<SymbolId>, EntrypointError> {
    Ok(detect_entry_modules(program, root))
}

// ---------------------------------------------------------------------------------------
// Build-output -> source remapping
// ---------------------------------------------------------------------------------------

/// Directory names that conventionally hold *build output*, as opposed to source.
const BUILD_DIRS: &[&str] = &[
    "dist", "build", "lib", "out", "esm", "cjs", "es", "umd", "types", "typings", "module",
    "browser", "node", "bundle",
];

/// Candidate *source* paths for a package-declared entry that points into build output.
/// `"dist/esm/index.js"` -> `src/index.ts`, `src/index.tsx`, `src/index.js`, `index.ts`, ...
///
/// `main`/`exports`/`bin` almost always name the compiled artifact (`dist/index.js`), which is
/// not in the analyzed tree; without this remap the real source entry is never found and the
/// whole project looks unreferenced. Callers try the declared path first and these only as a
/// fallback.
pub fn source_candidates(path: &str) -> Vec<String> {
    let p = path.trim_start_matches("./");
    let mut segments: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    while segments.len() > 1 && BUILD_DIRS.contains(&segments[0]) {
        segments.remove(0);
    }
    let rest = segments.join("/");
    if rest.is_empty() {
        return Vec::new();
    }

    let mut bases = vec![format!("src/{rest}"), rest];
    bases.retain(|b| b != p);
    bases.dedup();

    let mut out = Vec::new();
    for base in bases {
        let swaps: &[(&str, &[&str])] = &[
            (".d.mts", &[".mts"]),
            (".d.cts", &[".cts"]),
            (".d.ts", &[".ts", ".tsx"]),
            (".mjs", &[".mts", ".mjs"]),
            (".cjs", &[".cts", ".cjs"]),
            (".jsx", &[".tsx", ".jsx"]),
            (".js", &[".ts", ".tsx", ".jsx", ".js"]),
        ];
        match swaps.iter().find(|(suffix, _)| base.ends_with(suffix)) {
            Some((suffix, replacements)) => {
                let stem = &base[..base.len() - suffix.len()];
                out.extend(replacements.iter().map(|r| format!("{stem}{r}")));
            }
            None => out.push(base),
        }
    }
    out
}

// ---------------------------------------------------------------------------------------
// Convention classification
// ---------------------------------------------------------------------------------------

/// Which strength of root a path-convention match is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryClass {
    Primary,
    Secondary,
}

/// Frameworks (inferred from a package's declared dependencies) whose file-system routing
/// makes whole directories into entrypoints.
#[derive(Debug, Clone, Copy, Default)]
pub struct Frameworks {
    pub next: bool,
    pub nuxt: bool,
    pub sveltekit: bool,
    pub remix: bool,
    pub astro: bool,
    pub gatsby: bool,
    pub angular: bool,
    /// A Vue file-based-routing/layout plugin (`vite-plugin-pages`, `unplugin-vue-router`,
    /// `vite-plugin-vue-layouts`, ...): it generates routes from `src/pages/**` and wraps them
    /// in `src/layouts/**`, so those files are loaded by the plugin, never imported.
    pub vue_file_routing: bool,
}

impl Frameworks {
    pub fn from_package_json(json: &Value) -> Self {
        let mut deps: HashSet<&str> = HashSet::new();
        for key in [
            "dependencies",
            "devDependencies",
            "peerDependencies",
            "optionalDependencies",
        ] {
            if let Some(map) = json.get(key).and_then(|v| v.as_object()) {
                deps.extend(map.keys().map(|k| k.as_str()));
            }
        }
        let any = |names: &[&str]| names.iter().any(|n| deps.contains(n));
        Self {
            next: any(&["next"]),
            nuxt: any(&["nuxt", "nuxt3", "nuxt-edge", "@nuxt/kit", "@nuxt/schema"]),
            sveltekit: any(&["@sveltejs/kit"]),
            remix: any(&[
                "@remix-run/react",
                "@remix-run/node",
                "@remix-run/dev",
                "@remix-run/serve",
                "@react-router/dev",
            ]),
            astro: any(&["astro"]),
            gatsby: any(&["gatsby"]),
            angular: any(&["@angular/core"]),
            vue_file_routing: any(&[
                "vite-plugin-pages",
                "unplugin-vue-router",
                "vite-plugin-vue-layouts",
                "vite-plugin-vue-layouts-next",
            ]),
        }
    }
}

const ENTRY_STEMS: &[&str] = &["index", "main", "server", "app", "cli"];

const NEXT_APP_FILES: &[&str] = &[
    "page",
    "layout",
    "route",
    "loading",
    "error",
    "global-error",
    "not-found",
    "template",
    "default",
    "opengraph-image",
    "twitter-image",
    "icon",
    "apple-icon",
    "sitemap",
    "robots",
    "manifest",
];

/// Middle name parts (`foo.<part>.ts`) that mark a test/story/benchmark file.
const TEST_MARKERS: &[&str] = &[
    "test",
    "spec",
    "stories",
    "story",
    "e2e",
    "bench",
    "benchmark",
    "cy",
    "mock",
    "fixture",
    // Vitest/tsd type-level tests: `foo.test-d.ts`.
    "test-d",
    "spec-d",
];

/// Directories whose contents are tests, tooling, examples, or static assets: roots of usage
/// that are loaded by something other than an import from the app.
const SECONDARY_DIRS: &[&str] = &[
    "__tests__",
    "__mocks__",
    "__fixtures__",
    "__snapshots__",
    "test",
    "tests",
    "e2e",
    "cypress",
    "spec",
    "specs",
    ".storybook",
    "storybook",
    "stories",
    "benchmark",
    "benchmarks",
    "bench",
    "fixtures",
    "mocks",
    "examples",
    "example",
    "demo",
    "demos",
    "playground",
    "scripts",
    "script",
    "tools",
    "tooling",
    "migrations",
    "seeds",
    "seeders",
    "docs",
    "website",
    "public",
    "static",
];

/// Tool names whose `<tool>.*.js` files are build/test configuration even without `.config`.
const CONFIG_STEMS: &[&str] = &[
    "webpack",
    "rollup",
    "gulpfile",
    "Gruntfile",
    "karma",
    "babel",
    "eslint",
    "prettier",
    "jest",
    "vitest",
    "postcss",
    "tailwind",
    "metro",
    "craco",
];

const MISC_SECONDARY_STEMS: &[&str] = &[
    "setupTests",
    "setupProxy",
    "serviceWorker",
    "service-worker",
    "registerServiceWorker",
    "sw",
];

/// Classify a path (relative to its package directory, `/`-separated) as an entrypoint by
/// convention, or `None`.
pub fn classify_path(rel: &str, fw: &Frameworks) -> Option<EntryClass> {
    let segs: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
    let (file, dirs) = segs.split_last()?;
    let parts: Vec<&str> = file.split('.').collect();
    let stem = parts[0];
    let middle: &[&str] = if parts.len() > 2 {
        &parts[1..parts.len() - 1]
    } else {
        &[]
    };

    // Tests, stories, and benchmarks first: `index.test.ts` is a test, not the index.
    if middle.iter().any(|m| TEST_MARKERS.contains(m)) {
        return Some(EntryClass::Secondary);
    }

    if is_primary(dirs, stem, &parts, fw) {
        return Some(EntryClass::Primary);
    }

    if dirs
        .iter()
        .any(|d| SECONDARY_DIRS.contains(d) || d.ends_with("-test") || d.ends_with("-tests"))
        || middle
            .iter()
            .any(|m| matches!(*m, "config" | "conf" | "setup" | "rc" | "worker"))
        || CONFIG_STEMS.contains(&stem)
        || MISC_SECONDARY_STEMS.contains(&stem)
        || file.ends_with(".d.ts")
        || file.ends_with(".d.mts")
        || file.ends_with(".d.cts")
    {
        return Some(EntryClass::Secondary);
    }
    None
}

fn is_primary(dirs: &[&str], stem: &str, parts: &[&str], fw: &Frameworks) -> bool {
    let in_root = dirs.is_empty();
    let in_src = dirs == ["src"];
    let top = dirs.first().copied();
    let second = dirs.get(1).copied();

    // `index.js`, `src/main.ts`, ... exactly `<stem>.<ext>`; `src/main.server.ts` (Angular
    // SSR) and friends keep the stem but add a part.
    if (in_root || in_src) && ENTRY_STEMS.contains(&stem) && parts.len() == 2 {
        return true;
    }
    if in_src && stem == "main" {
        return true;
    }
    // Executables and serverless handlers.
    if dirs == ["bin"] || top == Some("api") || dirs.starts_with(&["netlify", "functions"]) {
        return true;
    }

    if fw.angular && in_src && matches!(stem, "polyfills" | "server") {
        return true;
    }

    let pages_dir = top == Some("pages") || (top == Some("src") && second == Some("pages"));
    let app_dir = top == Some("app") || (top == Some("src") && second == Some("app"));

    if fw.next {
        if pages_dir {
            return true;
        }
        if app_dir && NEXT_APP_FILES.contains(&stem) {
            return true;
        }
        if (in_root || in_src)
            && matches!(
                stem,
                "middleware" | "instrumentation" | "instrumentation-client" | "proxy"
            )
        {
            return true;
        }
    }

    if fw.nuxt {
        // Nuxt 4 moved sources under `app/`; Nuxt 3 uses the project root (or `src/`).
        let base = if matches!(top, Some("app") | Some("src")) && dirs.len() > 1 {
            second
        } else {
            top
        };
        if matches!(
            base,
            Some(
                "pages"
                    | "layouts"
                    | "middleware"
                    | "plugins"
                    | "server"
                    | "composables"
                    | "components"
                    | "utils"
                    | "stores"
                    | "modules"
            )
        ) {
            return true;
        }
        if (in_root || in_src || dirs == ["app"]) && matches!(stem, "app" | "error") {
            return true;
        }
    }

    if fw.vue_file_routing {
        // Default folders of the file-routing and layout plugins: `src/pages/**`,
        // `src/layouts/**` (or the same at the package root).
        let base = if top == Some("src") { second } else { top };
        if matches!(base, Some("pages") | Some("layouts"))
            && dirs.len() > usize::from(top == Some("src"))
        {
            return true;
        }
    }

    if fw.sveltekit {
        if top == Some("src") && matches!(second, Some("routes") | Some("params")) {
            return true;
        }
        if in_src && (stem.starts_with("hooks") || matches!(stem, "service-worker" | "app")) {
            return true;
        }
    }

    if fw.remix && top == Some("app") {
        if second == Some("routes") {
            return true;
        }
        if dirs == ["app"] && matches!(stem, "root" | "routes" | "entry") {
            return true;
        }
    }

    if fw.astro {
        if top == Some("src") && matches!(second, Some("pages") | Some("actions") | Some("content"))
        {
            return true;
        }
        if in_src && matches!(stem, "middleware" | "content") {
            return true;
        }
    }

    if fw.gatsby {
        if top == Some("src") && matches!(second, Some("pages") | Some("templates") | Some("api")) {
            return true;
        }
        if in_root && stem.starts_with("gatsby-") {
            return true;
        }
    }

    false
}

// ---------------------------------------------------------------------------------------
// Declared entrypoints
// ---------------------------------------------------------------------------------------

fn script_path_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?:^|[\s=:"'])((?:\.{1,2}/)?[A-Za-z0-9_@.\-/]+\.(?:[cm]?[jt]sx?))(?:$|[\s"'&;|)])"#,
        )
        .unwrap()
    })
}

fn html_script_src_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?is)<script\b[^>]*?\bsrc\s*=\s*["']([^"']+)["']"#).unwrap())
}

fn push_leaf_paths(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) if s.starts_with('.') => out.push(s.clone()),
        Value::Array(items) => items.iter().for_each(|v| push_leaf_paths(v, out)),
        Value::Object(map) => map.values().for_each(|v| push_leaf_paths(v, out)),
        _ => {}
    }
}

/// Every file path a `package.json` declares as an entry: `main`/`module`/`browser`/`source`,
/// `bin`, `exports` leaves, and any script path mentioned in `scripts` (`node scripts/x.js`,
/// `tsx src/server.ts`).
fn package_entry_specs(json: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for key in ["main", "module", "browser", "source"] {
        if let Some(s) = json.get(key).and_then(|v| v.as_str()) {
            out.push(s.to_string());
        }
    }
    match json.get("bin") {
        Some(Value::String(s)) => out.push(s.clone()),
        Some(Value::Object(map)) => {
            out.extend(map.values().filter_map(|v| v.as_str()).map(String::from))
        }
        _ => {}
    }
    if let Some(exports) = json.get("exports") {
        push_leaf_paths(exports, &mut out);
    }
    if let Some(scripts) = json.get("scripts").and_then(|v| v.as_object()) {
        for command in scripts.values().filter_map(|v| v.as_str()) {
            out.extend(
                script_path_re()
                    .captures_iter(command)
                    .map(|c| c[1].to_string()),
            );
        }
    }
    out
}

/// `angular.json` names its bootstrap files under `main`/`browser`/`server`/`polyfills` keys,
/// nested inside `projects.*.architect.*.options`.
fn collect_angular_entries(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, v) in map {
                if matches!(key.as_str(), "main" | "browser" | "server" | "polyfills") {
                    match v {
                        Value::String(s) => out.push(s.clone()),
                        Value::Array(items) => out.extend(
                            items
                                .iter()
                                .filter_map(|i| i.as_str())
                                .filter(|s| s.contains('.'))
                                .map(String::from),
                        ),
                        _ => {}
                    }
                } else {
                    collect_angular_entries(v, out);
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|v| collect_angular_entries(v, out)),
        _ => {}
    }
}

/// Module files in the analyzed tree, indexed by normalized root-relative path.
#[derive(Default)]
struct ModuleIndex {
    by_rel: HashMap<String, SymbolId>,
    order: Vec<(String, SymbolId)>,
}

const RESOLVE_EXTS: &[&str] = &[
    ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".vue",
];

impl ModuleIndex {
    /// Modules matching `candidate` exactly, with an extension appended, or as a directory
    /// index.
    fn expand(&self, candidate: &str) -> Vec<SymbolId> {
        if let Some(&id) = self.by_rel.get(candidate) {
            return vec![id];
        }
        for ext in RESOLVE_EXTS {
            if let Some(&id) = self.by_rel.get(&format!("{candidate}{ext}")) {
                return vec![id];
            }
        }
        for ext in RESOLVE_EXTS {
            if let Some(&id) = self.by_rel.get(&format!("{candidate}/index{ext}")) {
                return vec![id];
            }
        }
        Vec::new()
    }

    /// Resolve an entry spec declared by the package in `pkg_dir` (root-relative).
    fn resolve_spec(&self, spec: &str, pkg_dir: &str) -> Vec<SymbolId> {
        let spec = spec.trim();
        if spec.is_empty() || spec.contains("://") || spec.starts_with("//") {
            return Vec::new();
        }
        // HTML-style `/src/main.tsx` is relative to the package, not the filesystem root.
        let spec = spec.trim_start_matches('/');

        if let Some(star) = spec.find('*') {
            let prefix = join_norm("", &spec[..star]);
            let mut prefixes = vec![prefix.clone()];
            prefixes.extend(
                source_candidates(&format!("{prefix}x.js"))
                    .into_iter()
                    .filter_map(|c| c.strip_suffix("x.ts").map(|d| d.to_string())),
            );
            let mut hits = Vec::new();
            for p in prefixes {
                if p.is_empty() {
                    continue;
                }
                let full = join_norm(pkg_dir, &p);
                hits.extend(
                    self.order
                        .iter()
                        .filter(|(rel, _)| rel.starts_with(&full))
                        .map(|(_, id)| *id),
                );
            }
            return hits;
        }

        let declared = join_norm("", spec);
        let mut candidates = vec![declared.clone()];
        candidates.extend(source_candidates(&declared));
        for candidate in candidates {
            let hits = self.expand(&join_norm(pkg_dir, &candidate));
            if !hits.is_empty() {
                return hits;
            }
        }
        Vec::new()
    }
}

#[derive(Default)]
struct PackageInfo {
    frameworks: Frameworks,
    specs: Vec<String>,
}

fn read_text(path: &Path) -> Option<String> {
    std::fs::read(path)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// What [`detect_entries`] found.
#[derive(Debug, Clone, Default)]
pub struct EntryDetection {
    /// Every module that is a root of usage, strongest evidence first.
    pub entrypoints: Vec<SymbolId>,
    /// The entrypoints whose *exports are public API* -- consumed by something outside the
    /// analyzed code (a package's users, the router, a test runner or bundler reading a
    /// config). Equal to `entrypoints` unless detection fell back to "every module is an
    /// entrypoint": then there is no evidence any module is a public surface, so unused
    /// exports still count and this is empty.
    pub api_roots: Vec<SymbolId>,
    /// True when no recognizable entry convention was found and every non-external module
    /// was made an entrypoint instead.
    pub fallback: bool,
}

/// Detect entrypoint module symbols for an already-built [`Program`].
///
/// Works on any JS/TS [`Program`]: modules are matched to entry declarations by normalized
/// root-relative path, so it does not matter which analyzer produced them.
pub fn detect_entry_modules(program: &Program, root: &Path) -> Vec<SymbolId> {
    detect_entries(program, root).entrypoints
}

/// Like [`detect_entry_modules`], also reporting which entrypoints are public API surfaces.
pub fn detect_entries(program: &Program, root: &Path) -> EntryDetection {
    let root_norm = normalize_path(root);

    let mut index = ModuleIndex::default();
    for sym in program
        .symbols
        .iter()
        .filter(|s| s.kind == SymbolKind::Module && !s.is_external)
    {
        if let Some(rel) = rel_slash(&root_norm, &sym.file) {
            index.by_rel.insert(rel.clone(), sym.id);
            index.order.push((rel, sym.id));
        }
    }
    if index.order.is_empty() {
        return EntryDetection::default();
    }

    // One entry per package.json (root-relative directory), always including the root.
    let mut packages: HashMap<String, PackageInfo> = HashMap::new();
    packages.insert(String::new(), PackageInfo::default());
    for pj in find_package_jsons(root) {
        let (Some(dir), Some(json)) = (
            pj.parent().and_then(|d| rel_slash(&root_norm, d)),
            read_text(&pj).and_then(|t| jsonc::parse(&t)),
        ) else {
            continue;
        };
        packages.insert(
            dir,
            PackageInfo {
                frameworks: Frameworks::from_package_json(&json),
                specs: package_entry_specs(&json),
            },
        );
    }

    let mut explicit: Vec<SymbolId> = Vec::new();
    for (dir, pkg) in &packages {
        for spec in &pkg.specs {
            explicit.extend(index.resolve_spec(spec, dir));
        }
        // HTML shells name their module entry: Vite's `index.html`, Parcel's `src/index.html`.
        for html in ["index.html", "src/index.html", "public/index.html"] {
            let Some(text) = read_text(&root.join(dir).join(html)) else {
                continue;
            };
            for caps in html_script_src_re().captures_iter(&text) {
                let spec = &caps[1];
                let base = if html.contains('/') {
                    parent_dir(html)
                } else {
                    ""
                };
                let relative_to = if spec.starts_with('/') {
                    dir.clone()
                } else {
                    join_norm(dir, base)
                };
                explicit.extend(index.resolve_spec(spec, &relative_to));
            }
        }
    }
    if let Some(json) = read_text(&root.join("angular.json")).and_then(|t| jsonc::parse(&t)) {
        let mut specs = Vec::new();
        collect_angular_entries(&json, &mut specs);
        for spec in specs {
            explicit.extend(index.resolve_spec(&spec, ""));
        }
    }

    let mut primary: Vec<SymbolId> = Vec::new();
    let mut secondary: Vec<SymbolId> = Vec::new();
    for (rel, id) in &index.order {
        // Nearest enclosing package.json directory.
        let mut dir = parent_dir(rel);
        let pkg_dir = loop {
            if packages.contains_key(dir) {
                break dir;
            }
            if dir.is_empty() {
                break "";
            }
            dir = parent_dir(dir);
        };
        let in_pkg = rel
            .strip_prefix(pkg_dir)
            .map(|r| r.trim_start_matches('/'))
            .unwrap_or(rel);
        let fw = packages
            .get(pkg_dir)
            .map(|p| p.frameworks)
            .unwrap_or_default();
        match classify_path(in_pkg, &fw) {
            Some(EntryClass::Primary) => primary.push(*id),
            Some(EntryClass::Secondary) => secondary.push(*id),
            None => {}
        }
    }

    fn unique(ids: impl IntoIterator<Item = SymbolId>) -> Vec<SymbolId> {
        let mut seen = HashSet::new();
        ids.into_iter().filter(|id| seen.insert(*id)).collect()
    }

    let strong = unique(explicit.into_iter().chain(primary));
    if strong.is_empty() {
        // No recognizable entry convention: don't report the whole project as bloat, and
        // don't pretend any module is a public surface either.
        return EntryDetection {
            entrypoints: index.order.iter().map(|(_, id)| *id).collect(),
            api_roots: Vec::new(),
            fallback: true,
        };
    }
    let entrypoints = unique(strong.into_iter().chain(secondary));
    EntryDetection {
        api_roots: entrypoints.clone(),
        entrypoints,
        fallback: false,
    }
}

/// Detect entrypoints from package.json `main`, `bin`, `browser`, and `exports` fields.
pub fn detect_from_package_json(root: &Path) -> Result<Vec<Entrypoint>, EntrypointError> {
    let path = root.join("package.json");
    if !path.exists() {
        return Ok(Vec::new());
    }

    let text = std::fs::read_to_string(&path)?;
    let value: serde_json::Value = serde_json::from_str(&text)?;

    let mut out = Vec::new();

    // package.json "main" field
    if let Some(main) = value.get("main") {
        if let Some(main_str) = main.as_str() {
            out.push(Entrypoint {
                path: root.join(main_str).to_string_lossy().into_owned(),
            });
        }
    }

    // package.json "bin" field (for CLI tools)
    if let Some(bin) = value.get("bin") {
        match bin {
            serde_json::Value::String(s) => {
                // Single binary
                out.push(Entrypoint {
                    path: root.join(s).to_string_lossy().into_owned(),
                });
            }
            serde_json::Value::Object(map) => {
                // Multiple binaries
                for (_, value) in map {
                    if let Some(s) = value.as_str() {
                        out.push(Entrypoint {
                            path: root.join(s).to_string_lossy().into_owned(),
                        });
                    }
                }
            }
            _ => {}
        }
    }

    // package.json "browser" field, string form: bundlers use this as the browser-build
    // entry in place of "main". The object form (per-module remapping) is handled during
    // import resolution instead (see `resolve_module_path` in lib.rs).
    if let Some(browser) = value.get("browser") {
        if let Some(browser_str) = browser.as_str() {
            out.push(Entrypoint {
                path: root.join(browser_str).to_string_lossy().into_owned(),
            });
        }
    }

    // package.json "exports" field (ES Modules entrypoints)
    if let Some(exports) = value.get("exports") {
        match exports {
            serde_json::Value::String(s) => {
                out.push(Entrypoint {
                    path: root.join(s).to_string_lossy().into_owned(),
                });
            }
            serde_json::Value::Object(map) => {
                // Handle exports map - extract all leaf paths
                extract_export_paths(map, root, &mut out);
            }
            _ => {}
        }
    }

    Ok(out)
}

/// Recursively extract paths from exports map.
fn extract_export_paths(
    map: &serde_json::Map<String, serde_json::Value>,
    root: &Path,
    out: &mut Vec<Entrypoint>,
) {
    for (_, value) in map {
        match value {
            serde_json::Value::String(s) => {
                out.push(Entrypoint {
                    path: root.join(s).to_string_lossy().into_owned(),
                });
            }
            serde_json::Value::Object(nested) => {
                extract_export_paths(nested, root, out);
            }
            serde_json::Value::Array(arr) => {
                for item in arr {
                    if let Some(s) = item.as_str() {
                        out.push(Entrypoint {
                            path: root.join(s).to_string_lossy().into_owned(),
                        });
                    }
                }
            }
            _ => {}
        }
    }
}

/// Check if a file is a TypeScript file (for potential future use).
pub fn is_typescript_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|ext| ext == "ts" || ext == "tsx" || ext == "mts" || ext == "cts")
        .unwrap_or(false)
}

/// Check if a file is a JavaScript file.
pub fn is_javascript_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|ext| ext == "js" || ext == "mjs" || ext == "cjs" || ext == "jsx")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn next() -> Frameworks {
        Frameworks {
            next: true,
            ..Default::default()
        }
    }

    #[test]
    fn dist_entries_map_back_to_source() {
        let c = source_candidates("./dist/esm/index.js");
        assert_eq!(c[0], "src/index.ts");
        assert!(c.contains(&"src/index.tsx".to_string()));
        assert!(c.contains(&"index.ts".to_string()));
        assert!(source_candidates("dist/types/index.d.ts").contains(&"src/index.ts".to_string()));
        assert!(source_candidates("lib/cli.mjs").contains(&"src/cli.mts".to_string()));
    }

    #[test]
    fn top_level_files_have_no_remap() {
        // Already source; the declared path is tried first by the caller.
        assert!(!source_candidates("index.js").contains(&"index.js".to_string()));
    }

    #[test]
    fn plain_conventions() {
        let fw = Frameworks::default();
        assert_eq!(classify_path("index.js", &fw), Some(EntryClass::Primary));
        assert_eq!(classify_path("src/main.ts", &fw), Some(EntryClass::Primary));
        assert_eq!(classify_path("bin/cli.js", &fw), Some(EntryClass::Primary));
        assert_eq!(
            classify_path("api/hello.ts", &fw),
            Some(EntryClass::Primary)
        );
        assert_eq!(classify_path("src/utils/index.ts", &fw), None);
        assert_eq!(classify_path("src/lib/helper.ts", &fw), None);
    }

    #[test]
    fn tests_configs_and_stories_are_secondary() {
        let fw = Frameworks::default();
        for path in [
            "src/a.test.ts",
            "src/a.spec.tsx",
            "src/Button.stories.tsx",
            "test/helper.js",
            "__tests__/a.js",
            "vite.config.ts",
            "jest.setup.js",
            "webpack.prod.js",
            "scripts/build.js",
            "src/types/global.d.ts",
            "src/setupTests.ts",
            "public/sw.js",
        ] {
            assert_eq!(
                classify_path(path, &fw),
                Some(EntryClass::Secondary),
                "{path}"
            );
        }
    }

    #[test]
    fn index_test_is_a_test_not_the_index() {
        assert_eq!(
            classify_path("index.test.js", &Frameworks::default()),
            Some(EntryClass::Secondary)
        );
    }

    #[test]
    fn next_routes_are_primary_only_with_next_installed() {
        assert_eq!(
            classify_path("app/dashboard/page.tsx", &next()),
            Some(EntryClass::Primary)
        );
        assert_eq!(
            classify_path("src/pages/api/user.ts", &next()),
            Some(EntryClass::Primary)
        );
        assert_eq!(
            classify_path("middleware.ts", &next()),
            Some(EntryClass::Primary)
        );
        // A component inside app/ is not a route file.
        assert_eq!(classify_path("app/dashboard/chart.tsx", &next()), None);
        // Same paths without Next are just ordinary files.
        assert_eq!(
            classify_path("app/dashboard/page.tsx", &Frameworks::default()),
            None
        );
    }

    #[test]
    fn nuxt_sveltekit_remix_astro_conventions() {
        let nuxt = Frameworks {
            nuxt: true,
            ..Default::default()
        };
        assert_eq!(
            classify_path("pages/index.vue", &nuxt),
            Some(EntryClass::Primary)
        );
        assert_eq!(
            classify_path("composables/useAuth.ts", &nuxt),
            Some(EntryClass::Primary)
        );
        let svelte = Frameworks {
            sveltekit: true,
            ..Default::default()
        };
        assert_eq!(
            classify_path("src/routes/+page.ts", &svelte),
            Some(EntryClass::Primary)
        );
        assert_eq!(
            classify_path("src/hooks.server.ts", &svelte),
            Some(EntryClass::Primary)
        );
        let remix = Frameworks {
            remix: true,
            ..Default::default()
        };
        assert_eq!(
            classify_path("app/routes/_index.tsx", &remix),
            Some(EntryClass::Primary)
        );
        assert_eq!(
            classify_path("app/root.tsx", &remix),
            Some(EntryClass::Primary)
        );
        let astro = Frameworks {
            astro: true,
            ..Default::default()
        };
        assert_eq!(
            classify_path("src/pages/index.ts", &astro),
            Some(EntryClass::Primary)
        );
    }

    #[test]
    fn vue_file_routing_plugins_make_pages_and_layouts_entrypoints() {
        let json: Value = serde_json::from_str(
            r#"{"devDependencies": {"unplugin-vue-router": "0.8", "vue": "3"}}"#,
        )
        .unwrap();
        let fw = Frameworks::from_package_json(&json);
        assert!(fw.vue_file_routing);
        for path in [
            "src/pages/index.vue",
            "src/pages/users/[id].vue",
            "pages/about.vue",
            "src/layouts/default.vue",
        ] {
            assert_eq!(
                classify_path(path, &fw),
                Some(EntryClass::Primary),
                "{path}"
            );
        }
        // Without the plugin a `pages/` folder is just a folder.
        assert_eq!(
            classify_path("src/pages/index.vue", &Frameworks::default()),
            None
        );
        // Only files inside the folders, not a sibling that merely shares the name prefix.
        assert_eq!(classify_path("src/pagesx/a.vue", &fw), None);
    }

    #[test]
    fn frameworks_are_read_from_dependencies() {
        let json: Value = serde_json::from_str(
            r#"{"dependencies": {"next": "14"}, "devDependencies": {"@angular/core": "17"}}"#,
        )
        .unwrap();
        let fw = Frameworks::from_package_json(&json);
        assert!(fw.next && fw.angular && !fw.nuxt);
    }

    #[test]
    fn package_specs_include_scripts_and_exports() {
        let json: Value = serde_json::from_str(
            r#"{
                "main": "dist/index.js",
                "bin": {"x": "bin/x.js"},
                "exports": {".": {"import": "./dist/index.mjs", "types": "./dist/index.d.ts"}},
                "scripts": {"dev": "tsx watch src/server.ts", "seed": "node scripts/seed.js --force"}
            }"#,
        )
        .unwrap();
        let specs = package_entry_specs(&json);
        for expected in [
            "dist/index.js",
            "bin/x.js",
            "./dist/index.mjs",
            "src/server.ts",
            "scripts/seed.js",
        ] {
            assert!(
                specs.contains(&expected.to_string()),
                "missing {expected}: {specs:?}"
            );
        }
    }

    #[test]
    fn html_script_tags_are_found() {
        let caps: Vec<String> = html_script_src_re()
            .captures_iter(r#"<script type="module" src="/src/main.tsx"></script>"#)
            .map(|c| c[1].to_string())
            .collect();
        assert_eq!(caps, vec!["/src/main.tsx"]);
    }
}
