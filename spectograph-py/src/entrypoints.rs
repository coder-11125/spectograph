//! Entrypoint detection from packaging metadata.
//!
//! Beyond `__main__` guards and `__init__.py` roots, a Python project's real entrypoints
//! are declared in packaging config. This module reads `pyproject.toml` `[project.scripts]`
//! and `[project.gui-scripts]` tables and yields `(module_path, function_name)` pairs.

use std::path::Path;

use spectograph_analyzer::AnalyzeError;

/// A single console-script entrypoint: `name = "module.path:function"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptEntrypoint {
    pub module_path: String,
    pub function: String,
}

/// Extract `module:function` from a console-script value string.
fn parse_script_value(value: &str) -> Option<(String, String)> {
    // Values look like `pkg.mod:func` or `pkg.mod:func.with.dots`.
    let (module, func) = value.trim().split_once(':')?;
    if module.is_empty() || func.is_empty() {
        return None;
    }
    Some((module.to_string(), func.to_string()))
}

/// Parse `pyproject.toml` `[project.scripts]` and `[project.gui-scripts]` entrypoints.
///
/// Returns an error only for I/O issues; a missing or malformed pyproject is treated as
/// "no console-script entrypoints" rather than a hard failure.
pub fn scripts_from_pyproject(root: &Path) -> Result<Vec<ScriptEntrypoint>, AnalyzeError> {
    let path = root.join("pyproject.toml");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&path)?;
    let value: toml::Value = match text.parse() {
        Ok(v) => v,
        Err(_) => return Ok(Vec::new()),
    };

    let mut out = Vec::new();
    let project = value.get("project");
    for table_key in ["scripts", "gui-scripts"] {
        let Some(scripts) = project.and_then(|p| p.get(table_key)) else {
            continue;
        };
        let Some(table) = scripts.as_table() else {
            continue;
        };
        for (name, val) in table {
            let _ = name;
            if let Some(func) = val.as_str().and_then(parse_script_value) {
                out.push(ScriptEntrypoint {
                    module_path: func.0,
                    function: func.1,
                });
            }
        }
    }
    Ok(out)
}
