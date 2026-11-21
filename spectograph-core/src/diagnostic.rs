//! Framework-level diagnostics: findings that aren't "this symbol is bloat" but are still
//! worth surfacing from a `check` run, such as a React component receiving a prop it never
//! declares, or an Angular template binding an `@Input()` that doesn't exist.
//!
//! These are language-agnostic on purpose: an analyzer produces them, `core` only carries
//! and renders them. They never affect reachability or the bloat tiers.

use serde::Serialize;

/// One framework-level finding. `code` is a stable, kebab-case identifier that CI
/// consumers can match on (part of the JSON schema contract, see `docs/json-schema.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    pub code: &'static str,
    pub message: String,
    /// Path relative to the analyzed root, `/`-separated.
    pub file: String,
    pub line: usize,
}

pub const REACT_PROP_MISMATCH: &str = "react-prop-mismatch";
pub const ANGULAR_BINDING_MISMATCH: &str = "angular-binding-mismatch";
pub const VUE_PROP_MISMATCH: &str = "vue-prop-mismatch";
pub const VUE_EVENT_MISMATCH: &str = "vue-event-mismatch";
pub const VUE_UNUSED_CSS_CLASS: &str = "vue-unused-css-class";
