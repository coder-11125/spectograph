//! End-to-end tests: run the real `spectograph` binary against small projects written
//! inline (the repo's testing convention), asserting on exit codes and the JSON contract.

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::tempdir;

fn write(dir: &Path, name: &str, content: &[u8]) {
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn spectograph(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_spectograph"))
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap()
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout was not JSON ({e}):\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn finding_names(report: &Value, tier: &str) -> Vec<String> {
    report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["tier"] == tier)
        .map(|f| f["name"].as_str().unwrap().to_string())
        .collect()
}

/// A tiny TypeScript project: `index.ts` uses `used`, `dead` is never referenced.
fn ts_project(dir: &Path) {
    write(
        dir,
        "package.json",
        br#"{"name": "demo", "main": "dist/index.js"}"#,
    );
    write(
        dir,
        "src/index.ts",
        b"import { used } from './util.js';\nexport function run(): void { used(); }\n",
    );
    write(
        dir,
        "src/util.ts",
        b"export function used(): void {}\nexport function dead(): void {}\n",
    );
    write(
        dir,
        "src/orphan.ts",
        b"export function neverImported(): void {}\n",
    );
}

#[test]
fn json_report_follows_the_documented_contract() {
    let dir = tempdir().unwrap();
    ts_project(dir.path());

    let out = spectograph(dir.path(), &["check", ".", "--format", "json"]);
    let report = json(&out);

    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["language"], "typescript");
    assert!(report["total_symbols"].as_u64().unwrap() > 0);
    assert!(report["diagnostics"].is_array());
    // Paths are root-relative with forward slashes, never absolute.
    for f in report["findings"].as_array().unwrap() {
        let file = f["file"].as_str().unwrap();
        assert!(!file.starts_with('/') && !file.contains('\\'), "{file}");
    }
    // `./util.js` resolves to util.ts, so `used` is live; `neverImported` is not.
    let dead = finding_names(&report, "guaranteed");
    assert!(dead.contains(&"neverImported".to_string()), "{dead:?}");
    assert!(!dead.contains(&"used".to_string()), "{dead:?}");
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn relative_dot_root_still_resolves_cross_file_imports() {
    // Walking `.` yields `./src/a.ts` while resolution produces `src/a.ts`; they must be
    // the same module or every import silently fails to link.
    let dir = tempdir().unwrap();
    ts_project(dir.path());
    let report = json(&spectograph(
        dir.path(),
        &["check", ".", "--format", "json"],
    ));
    assert!(!finding_names(&report, "guaranteed").contains(&"used".to_string()));
}

#[test]
fn lean_project_exits_zero() {
    let dir = tempdir().unwrap();
    write(dir.path(), "index.ts", b"export function run(): void {}\n");
    let out = spectograph(dir.path(), &["check", "."]);
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("No bloat detected"));
}

#[test]
fn language_is_detected_by_file_count_not_first_match() {
    // A TypeScript app with one stray `.js` config file used to be analyzed as JavaScript
    // (and report 24 symbols for 131 files).
    let dir = tempdir().unwrap();
    for i in 0..5 {
        write(
            dir.path(),
            &format!("src/m{i}.ts"),
            format!("export const v{i} = {i};\n").as_bytes(),
        );
    }
    write(dir.path(), "eslint.config.js", b"module.exports = {};\n");
    let out = spectograph(dir.path(), &["check", ".", "--format", "json"]);
    assert_eq!(json(&out)["language"], "typescript");
    assert!(String::from_utf8_lossy(&out.stderr).contains("also found"));
}

#[test]
fn fail_on_tier_gates_the_exit_code_in_the_documented_direction() {
    // Only a "probable" finding: a function referenced solely via a dynamic import.
    let dir = tempdir().unwrap();
    write(
        dir.path(),
        "index.js",
        b"function main() {}\nmodule.exports = main;\n",
    );
    // A guaranteed-dead helper in a file nothing imports.
    write(dir.path(), "lib/dead.js", b"function dead() {}\n");
    write(dir.path(), "package.json", br#"{"main": "index.js"}"#);

    let strict = spectograph(dir.path(), &["check", ".", "--fail-on", "guaranteed"]);
    assert_eq!(
        strict.status.code(),
        Some(1),
        "a guaranteed finding must fail --fail-on guaranteed"
    );

    // A project with no findings at all passes every threshold.
    let clean = tempdir().unwrap();
    write(
        clean.path(),
        "index.js",
        b"function main() {}\nmodule.exports = main;\n",
    );
    for tier in ["guaranteed", "probable", "high_cost"] {
        let out = spectograph(clean.path(), &["check", ".", "--fail-on", tier]);
        assert_eq!(out.status.code(), Some(0), "--fail-on {tier}");
    }
}

#[test]
fn invalid_flag_values_and_paths_are_usage_errors() {
    let dir = tempdir().unwrap();
    write(dir.path(), "index.ts", b"export const a = 1;\n");

    let bad_tier = spectograph(dir.path(), &["check", ".", "--fail-on", "nonsense"]);
    assert_eq!(bad_tier.status.code(), Some(2));

    let missing = spectograph(dir.path(), &["check", "does/not/exist"]);
    assert_eq!(missing.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("not a directory"));

    let empty = tempdir().unwrap();
    let none = spectograph(empty.path(), &["check", "."]);
    assert_eq!(none.status.code(), Some(2));
}

#[test]
fn fix_is_not_implemented_and_does_not_pretend_to_succeed() {
    let dir = tempdir().unwrap();
    let out = spectograph(dir.path(), &["fix", "."]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not implemented"));
}

#[test]
fn a_non_utf8_source_file_does_not_abort_the_analysis() {
    let dir = tempdir().unwrap();
    write(
        dir.path(),
        "index.js",
        b"function main() {}\nmodule.exports = main;\n",
    );
    write(
        dir.path(),
        "vendor.js",
        b"// caf\xe9 latin-1 \xff\xfe\nfunction legacy() {}\n",
    );
    let out = spectograph(dir.path(), &["check", ".", "--format", "json"]);
    assert!(
        out.status.code() != Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(json(&out)["language"], "javascript");
}

#[test]
fn react_prop_mismatches_surface_as_diagnostics_and_can_gate_ci() {
    let dir = tempdir().unwrap();
    write(
        dir.path(),
        "src/index.tsx",
        b"import { Card } from './Card';\n\
          export function App() { return <Card title=\"x\" bogus=\"y\" />; }\n",
    );
    write(
        dir.path(),
        "src/Card.tsx",
        b"export function Card({ title }: { title: string }) { return <div>{title}</div>; }\n",
    );

    let out = spectograph(dir.path(), &["check", ".", "--format", "json"]);
    let report = json(&out);
    let diags = report["diagnostics"].as_array().unwrap();
    assert!(
        diags.iter().any(|d| d["code"] == "react-prop-mismatch"
            && d["message"].as_str().unwrap().contains("bogus")
            && d["file"] == "src/index.tsx"),
        "{diags:?}"
    );
    // Informational by default...
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // ...a CI gate on request.
    let gated = spectograph(
        dir.path(),
        &["check", ".", "--format", "json", "--fail-on-diagnostics"],
    );
    assert_eq!(gated.status.code(), Some(1));
}

#[test]
fn tsconfig_with_comments_still_provides_path_aliases() {
    // Real tsconfig files have comments and trailing commas; treating one as unparsable
    // used to drop every alias and report all aliased imports' targets as dead.
    let dir = tempdir().unwrap();
    write(
        dir.path(),
        "tsconfig.json",
        b"{\n  // paths\n  \"compilerOptions\": { \"baseUrl\": \".\", \"paths\": { \"@/*\": [\"src/*\",], }, },\n}\n",
    );
    write(
        dir.path(),
        "src/index.ts",
        b"import { helper } from '@/lib/helper';\nexport function run(): void { helper(); }\n",
    );
    write(
        dir.path(),
        "src/lib/helper.ts",
        b"export function helper(): void {}\n",
    );
    let report = json(&spectograph(
        dir.path(),
        &["check", ".", "--format", "json"],
    ));
    assert!(
        !finding_names(&report, "guaranteed").contains(&"helper".to_string()),
        "{report}"
    );
}
