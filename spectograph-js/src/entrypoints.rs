//! Entrypoint detection for JavaScript projects.
//!
//! Beyond explicit entry files (index.js, main.js), a JavaScript project's real entrypoints
//! are declared in packaging config. This module reads `package.json` `main` and `bin`
//! fields, and handles CommonJS/ES Module entry detection.

use std::collections::HashMap;
use std::path::Path;

use spectograph_core::ir::SymbolId;
use thiserror::Error;

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

/// Detect entrypoint module IDs from the program and file module mapping.
pub fn detect_entrypoint_symbols(
    program: &spectograph_core::ir::Program,
    file_module: &HashMap<String, SymbolId>,
    root: &Path,
) -> Result<Vec<SymbolId>, EntrypointError> {
    let mut entrypoints = Vec::new();

    // 1. Detect from package.json
    let pkg_entrypoints = detect_from_package_json(root)?;
    for ep in &pkg_entrypoints {
        if let Some(&module_id) = file_module.get(&ep.path) {
            if !entrypoints.contains(&module_id) {
                entrypoints.push(module_id);
            }
        }
    }

    // 2. Default entrypoints: index.js, index.mjs, index.cjs
    let default_entrypoints = vec!["index.js", "index.mjs", "index.cjs"];
    for default in default_entrypoints {
        let full_path = root.join(default);
        if full_path.exists() {
            let path_str = full_path.to_string_lossy().into_owned();
            if let Some(&module_id) = file_module.get(&path_str) {
                if !entrypoints.contains(&module_id) {
                    entrypoints.push(module_id);
                }
            }
        }
    }

    // 3. All module symbols that haven't been explicitly marked yet
    // In JS, every file is potentially a module that could be an entrypoint
    // For now, mark all top-level module symbols as potential entrypoints
    // (This is conservative; we'll refine based on usage patterns)
    for sym in &program.symbols {
        if sym.kind == spectograph_core::ir::SymbolKind::Module
            && !entrypoints.contains(&sym.id) {
            // Only add if it's a real JS file (not node_modules, etc.)
            if sym.file.to_string_lossy().contains("node_modules") {
                continue;
            }
            entrypoints.push(sym.id);
        }
    }

    Ok(entrypoints)
}

/// Detect entrypoints from package.json `main` and `bin` fields.
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
