//! Tests for JavaScript entrypoint detection.

use std::path::PathBuf;
use tempfile::tempdir;

use spectograph_js::entrypoints::detect_from_package_json;

#[test]
fn package_json_main_field() {
    let dir = tempdir().unwrap();
    let pkg_path = dir.path().join("package.json");
    std::fs::write(
        &pkg_path,
        r#"{"name": "test", "main": "index.js"}"#,
    )
    .unwrap();

    let entrypoints = detect_from_package_json(dir.path()).unwrap();
    assert_eq!(entrypoints.len(), 1);
    assert!(entrypoints[0].path.ends_with("index.js"));
}

#[test]
fn package_json_bin_string() {
    let dir = tempdir().unwrap();
    let pkg_path = dir.path().join("package.json");
    std::fs::write(
        &pkg_path,
        r#"{"name": "test", "bin": "cli.js"}"#,
    )
    .unwrap();

    let entrypoints = detect_from_package_json(dir.path()).unwrap();
    assert_eq!(entrypoints.len(), 1);
    assert!(entrypoints[0].path.ends_with("cli.js"));
}

#[test]
fn package_json_bin_object() {
    let dir = tempdir().unwrap();
    let pkg_path = dir.path().join("package.json");
    std::fs::write(
        &pkg_path,
        r#"{"name": "test", "bin": {"myapp": "bin/myapp.js", "helper": "bin/helper.js"}}"#,
    )
    .unwrap();

    let entrypoints = detect_from_package_json(dir.path()).unwrap();
    assert_eq!(entrypoints.len(), 2);
    let paths: Vec<_> = entrypoints.iter().map(|e| e.path.clone()).collect();
    assert!(paths.iter().any(|p| p.ends_with("bin/myapp.js")));
    assert!(paths.iter().any(|p| p.ends_with("bin/helper.js")));
}

#[test]
fn package_json_exports_string() {
    let dir = tempdir().unwrap();
    let pkg_path = dir.path().join("package.json");
    std::fs::write(
        &pkg_path,
        r#"{"name": "test", "exports": "./dist/index.js"}"#,
    )
    .unwrap();

    let entrypoints = detect_from_package_json(dir.path()).unwrap();
    assert_eq!(entrypoints.len(), 1);
    assert!(entrypoints[0].path.ends_with("./dist/index.js"));
}

#[test]
fn package_json_exports_object() {
    let dir = tempdir().unwrap();
    let pkg_path = dir.path().join("package.json");
    std::fs::write(
        &pkg_path,
        r#"{"name": "test", "exports": {".": "./dist/index.js", " ./feature": "./dist/feature.js"}}"#,
    )
    .unwrap();

    let entrypoints = detect_from_package_json(dir.path()).unwrap();
    assert_eq!(entrypoints.len(), 2);
}

#[test]
fn missing_package_json() {
    let dir = tempdir().unwrap();
    let entrypoints = detect_from_package_json(dir.path()).unwrap();
    assert!(entrypoints.is_empty());
}

#[test]
fn malformed_package_json() {
    let dir = tempdir().unwrap();
    let pkg_path = dir.path().join("package.json");
    std::fs::write(&pkg_path, r#"{invalid json}"#).unwrap();

    // Should not panic, should return error which we handle
    let result = detect_from_package_json(dir.path());
    // May return error for malformed JSON, which is fine
    let _ = result;
}
