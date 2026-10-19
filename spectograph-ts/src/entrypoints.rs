//! Entrypoint detection for TypeScript projects.
//!
//! The detection logic is not TypeScript-specific, so it lives in
//! [`spectograph_js::entrypoints`] and is shared: `package.json` `main`/`module`/`bin`/
//! `exports`/`scripts` (with `dist/` -> `src/` remapping, so a compiled `dist/index.js` finds
//! its `src/index.ts`), HTML script tags, `angular.json`, monorepo workspace members, and
//! framework/test/config file conventions. As there, a project with no recognizable entry at
//! all falls back to treating every non-external module as an entrypoint, so it doesn't get
//! reported as 100% bloat.

use std::path::Path;

use spectograph_core::ir::{Program, SymbolId};

/// Detect entrypoint module symbol ids for an already-built [`Program`].
pub fn detect_entrypoint_symbols(program: &Program, root: &Path) -> Vec<SymbolId> {
    spectograph_js::entrypoints::detect_entry_modules(program, root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spectograph_analyzer::Analyzer;
    use tempfile::tempdir;

    fn analyze(files: &[(&str, &str)]) -> Program {
        let dir = tempdir().unwrap();
        for (name, content) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
        }
        crate::TypeScriptAnalyzer.analyze(dir.path()).unwrap()
    }

    fn entry_files(program: &Program) -> Vec<String> {
        program
            .entrypoints
            .iter()
            .filter_map(|id| program.symbol(*id))
            .map(|s| s.file.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn package_json_main_field_is_the_entrypoint() {
        let program = analyze(&[
            ("package.json", r#"{"main": "src/index.ts"}"#),
            ("src/index.ts", "export function run(): void {}"),
            ("src/other.ts", "export function unused(): void {}"),
        ]);
        let entry_files = entry_files(&program);
        assert_eq!(entry_files.len(), 1);
        assert!(entry_files[0].ends_with("src/index.ts"));
    }

    #[test]
    fn conventional_index_ts_is_the_entrypoint_without_package_json() {
        let program = analyze(&[
            ("index.ts", "export function run(): void {}"),
            ("other.ts", "export function unused(): void {}"),
        ]);
        let entry_files = entry_files(&program);
        assert_eq!(entry_files.len(), 1);
        assert!(entry_files[0].ends_with("index.ts"));
    }

    #[test]
    fn falls_back_to_every_module_when_no_convention_matches() {
        let program = analyze(&[
            ("a.ts", "export function a(): void {}"),
            ("b.ts", "export function b(): void {}"),
        ]);
        assert_eq!(program.entrypoints.len(), 2);
    }

    #[test]
    fn node_modules_files_are_excluded_from_the_fallback() {
        let program = analyze(&[
            ("a.ts", "export function a(): void {}"),
            ("node_modules/pkg/index.ts", "export function b(): void {}"),
        ]);
        let entry_files = entry_files(&program);
        assert_eq!(entry_files.len(), 1);
        assert!(entry_files[0].ends_with("a.ts"));
    }

    #[test]
    fn compiled_main_maps_back_to_the_ts_source() {
        // `main` names the build artifact; the analyzed tree only has the source.
        let program = analyze(&[
            ("package.json", r#"{"main": "./dist/index.js"}"#),
            ("src/index.ts", "export function run(): void {}"),
            ("src/internal.ts", "export function helper(): void {}"),
        ]);
        let entry_files = entry_files(&program);
        assert_eq!(entry_files.len(), 1);
        assert!(entry_files[0].ends_with("src/index.ts"));
    }

    #[test]
    fn tests_and_configs_are_roots_but_do_not_replace_the_real_entry() {
        let program = analyze(&[
            ("src/index.ts", "export function run(): void {}"),
            ("src/run.test.ts", "export function t(): void {}"),
            ("vite.config.ts", "export default {};"),
            ("src/dead.ts", "export function dead(): void {}"),
        ]);
        let entry_files = entry_files(&program);
        assert!(entry_files.iter().any(|f| f.ends_with("src/index.ts")));
        assert!(entry_files.iter().any(|f| f.ends_with("run.test.ts")));
        assert!(entry_files.iter().any(|f| f.ends_with("vite.config.ts")));
        assert!(!entry_files.iter().any(|f| f.ends_with("dead.ts")));
    }

    #[test]
    fn only_tests_present_does_not_make_the_rest_of_the_project_dead() {
        // No primary entry convention anywhere: every module stays a root.
        let program = analyze(&[
            ("lib/a.ts", "export function a(): void {}"),
            ("lib/a.test.ts", "export function t(): void {}"),
        ]);
        assert_eq!(program.entrypoints.len(), 2);
    }

    #[test]
    fn next_app_router_routes_are_entrypoints() {
        let program = analyze(&[
            ("package.json", r#"{"dependencies": {"next": "14.0.0"}}"#),
            (
                "app/page.tsx",
                "export default function Page() { return null; }",
            ),
            (
                "app/dashboard/layout.tsx",
                "export default function L() { return null; }",
            ),
            (
                "components/unused.tsx",
                "export function Unused() { return null; }",
            ),
        ]);
        let entry_files = entry_files(&program);
        assert!(entry_files.iter().any(|f| f.ends_with("app/page.tsx")));
        assert!(entry_files
            .iter()
            .any(|f| f.ends_with("dashboard/layout.tsx")));
        assert!(!entry_files.iter().any(|f| f.ends_with("unused.tsx")));
    }

    #[test]
    fn vite_index_html_module_script_is_the_entrypoint() {
        let program = analyze(&[
            (
                "index.html",
                r#"<html><body><script type="module" src="/src/bootstrap.tsx"></script></body></html>"#,
            ),
            ("src/bootstrap.tsx", "export const x = 1;"),
            ("src/dead.tsx", "export const y = 2;"),
        ]);
        let entry_files = entry_files(&program);
        assert_eq!(entry_files.len(), 1);
        assert!(entry_files[0].ends_with("bootstrap.tsx"));
    }

    #[test]
    fn npm_scripts_name_entry_files() {
        let program = analyze(&[
            (
                "package.json",
                r#"{"scripts": {"start": "tsx watch src/server.ts"}}"#,
            ),
            ("src/server.ts", "export const s = 1;"),
            ("src/dead.ts", "export const d = 2;"),
        ]);
        let entry_files = entry_files(&program);
        assert!(entry_files.iter().any(|f| f.ends_with("src/server.ts")));
        assert!(!entry_files.iter().any(|f| f.ends_with("dead.ts")));
    }

    #[test]
    fn monorepo_packages_each_get_their_own_roots() {
        let program = analyze(&[
            ("package.json", r#"{"name": "root", "private": true}"#),
            (
                "packages/a/package.json",
                r#"{"name": "@x/a", "main": "dist/index.js"}"#,
            ),
            ("packages/a/src/index.ts", "export const a = 1;"),
            ("packages/a/src/dead.ts", "export const d = 1;"),
            (
                "packages/b/package.json",
                r#"{"name": "@x/b", "main": "lib/main.js"}"#,
            ),
            ("packages/b/src/main.ts", "export const b = 1;"),
        ]);
        let entry_files = entry_files(&program);
        assert!(entry_files
            .iter()
            .any(|f| f.ends_with("packages/a/src/index.ts")));
        assert!(entry_files
            .iter()
            .any(|f| f.ends_with("packages/b/src/main.ts")));
        assert!(!entry_files.iter().any(|f| f.ends_with("a/src/dead.ts")));
    }
}
