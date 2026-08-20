//! React-specific intelligence: component detection, prop tracking, and hook
//! usage, layered on top of the JS analyzer's regular symbol/reference graph.
//!
//! These are additive: a `ReactComponent` is still a normal `Function`/`Const`
//! [`spectograph_core::ir::Symbol`] in the shared IR (so existing reachability and
//! bloat analysis already applies to it unchanged); this module just captures the
//! React-specific facts -- declared props, JSX usage sites, hook calls -- that the
//! shared, language-agnostic IR has no vocabulary for.

use std::path::PathBuf;

use spectograph_core::ir::SymbolId;

/// A function/arrow-function symbol identified as a React component: a
/// PascalCase-named declaration whose body returns JSX.
#[derive(Debug, Clone)]
pub struct ReactComponent {
    pub symbol_id: SymbolId,
    pub name: String,
    pub file: PathBuf,
    pub line: usize,
    /// Prop names destructured from the component's first parameter
    /// (`function Foo({ a, b })`). Empty if the component takes a bare `props`
    /// parameter (or none) -- we can't statically enumerate props read off an
    /// un-destructured object without deeper flow analysis.
    pub declared_props: Vec<String>,
}

/// One JSX usage site: `<ComponentName propA={x} propB="y" />`.
#[derive(Debug, Clone)]
pub struct JsxUsage {
    pub component_name: String,
    pub file: PathBuf,
    pub line: usize,
    pub passed_props: Vec<String>,
}

/// A call to something matching React's hook naming convention (`useXxx`), whether
/// a built-in hook or a project-defined custom hook.
#[derive(Debug, Clone)]
pub struct HookCall {
    /// The component or custom hook this call was made from.
    pub enclosing: SymbolId,
    pub hook_name: String,
    pub file: PathBuf,
    pub line: usize,
}

/// A prop passed at a JSX usage site that the target component doesn't declare.
#[derive(Debug, Clone)]
pub struct PropMismatch {
    pub component_name: String,
    pub file: PathBuf,
    pub line: usize,
    pub prop: String,
}

/// Props JSX treats specially and never shows up in a component's own parameter
/// list: `key`/`ref` are consumed by React itself, `children` comes from nesting.
const IMPLICIT_PROPS: &[&str] = &["key", "ref", "children"];

/// True if `name` looks like a JSX component tag (PascalCase) as opposed to an
/// intrinsic DOM tag (`div`, `span`, ...) -- JSX/React distinguish the two purely
/// by the first letter's case.
pub fn is_component_name(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

/// True if `name` follows React's hook naming convention: `use` followed by an
/// uppercase letter (`useState`, `useMyCustomHook`). Plain `use` (no suffix)
/// doesn't count -- there's no name left to be a hook of.
pub fn is_hook_name(name: &str) -> bool {
    name.strip_prefix("use")
        .and_then(|rest| rest.chars().next())
        .is_some_and(|c| c.is_ascii_uppercase())
}

/// Flag JSX usage sites that pass a prop the target component doesn't declare.
/// Only checks components whose props were statically enumerable (a destructured
/// parameter); components taking a bare `props` object are skipped since we can't
/// tell what they read from it, and would otherwise produce nothing but noise.
pub fn find_prop_mismatches(
    components: &[ReactComponent],
    usages: &[JsxUsage],
) -> Vec<PropMismatch> {
    let mut by_name: std::collections::HashMap<&str, &ReactComponent> =
        std::collections::HashMap::new();
    for c in components {
        // First definition wins, matching the "vertical slice" name-resolution
        // convention used elsewhere in this analyzer.
        by_name.entry(c.name.as_str()).or_insert(c);
    }

    let mut out = Vec::new();
    for usage in usages {
        let Some(component) = by_name.get(usage.component_name.as_str()) else {
            continue;
        };
        if component.declared_props.is_empty() {
            continue;
        }
        for prop in &usage.passed_props {
            if IMPLICIT_PROPS.contains(&prop.as_str()) {
                continue;
            }
            if !component.declared_props.iter().any(|p| p == prop) {
                out.push(PropMismatch {
                    component_name: usage.component_name.clone(),
                    file: usage.file.clone(),
                    line: usage.line,
                    prop: prop.clone(),
                });
            }
        }
    }
    out
}
