//! Framework diagnostics: turn React/Vue (and, from `spectograph-ts`, Angular) prop and
//! binding mismatches into language-agnostic [`Diagnostic`]s for `check`'s report.
//!
//! These are heuristics layered on name matching ("first definition wins" across the
//! project), so each one is filtered to the cases where the name match is trustworthy.

use std::collections::HashMap;
use std::path::Path;

use spectograph_analyzer::AnalyzeError;
use spectograph_core::analysis::display_path;
use spectograph_core::diagnostic::{
    Diagnostic, REACT_PROP_MISMATCH, VUE_EVENT_MISMATCH, VUE_PROP_MISMATCH,
};
use spectograph_core::ir::Program;

use crate::parse::FileResult;
use crate::react::{find_prop_mismatches, JsxUsage, ReactComponent};
use crate::vue::{VueBindingKind, VueBindingMismatch};

/// React prop mismatches as diagnostics.
///
/// A component name defined more than once in the project is ambiguous -- the name match
/// would pick one definition arbitrarily and report props "missing" from the wrong one -- so
/// those are skipped. `data-*`/`aria-*` attributes pass straight through to the DOM and are
/// never declared, so they're skipped too.
pub fn react_diagnostics(
    components: &[ReactComponent],
    usages: &[JsxUsage],
    root: &Path,
) -> Vec<Diagnostic> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for c in components {
        *counts.entry(c.name.as_str()).or_default() += 1;
    }
    let unique: Vec<ReactComponent> = components
        .iter()
        .filter(|c| counts.get(c.name.as_str()) == Some(&1))
        .cloned()
        .collect();

    find_prop_mismatches(&unique, usages)
        .into_iter()
        .filter(|m| !m.prop.starts_with("data-") && !m.prop.starts_with("aria-"))
        .map(|m| Diagnostic {
            code: REACT_PROP_MISMATCH,
            message: format!(
                "<{}> is passed prop `{}` that the component does not declare",
                m.component_name, m.prop
            ),
            file: display_path(root, &m.file),
            line: m.line,
        })
        .collect()
}

/// Vue template prop/emit mismatches as diagnostics.
pub fn vue_diagnostics(mismatches: &[VueBindingMismatch], root: &Path) -> Vec<Diagnostic> {
    mismatches
        .iter()
        .map(|m| {
            let (code, what) = match m.kind {
                VueBindingKind::Prop => (VUE_PROP_MISMATCH, "prop"),
                VueBindingKind::Event => (VUE_EVENT_MISMATCH, "event"),
            };
            Diagnostic {
                code,
                message: format!(
                    "<{}> is bound {what} `{}` that the component does not declare",
                    m.target_component, m.binding
                ),
                file: display_path(root, &m.file),
                line: m.line,
            }
        })
        .collect()
}

/// Sort deterministically (file, line, code, message) and drop exact duplicates.
pub fn finish(mut diagnostics: Vec<Diagnostic>) -> Vec<Diagnostic> {
    diagnostics.sort_by(|a, b| {
        (&a.file, a.line, a.code, &a.message).cmp(&(&b.file, b.line, b.code, &b.message))
    });
    diagnostics.dedup();
    diagnostics
}

/// All framework diagnostics for the JavaScript analyzer's already-parsed project.
pub(crate) fn collect(
    program: &Program,
    parsed: &[(String, FileResult)],
    root: &Path,
) -> Result<Vec<Diagnostic>, AnalyzeError> {
    let mut components = Vec::new();
    let mut usages = Vec::new();
    for (_path, result) in parsed {
        components.extend(result.components.iter().cloned());
        usages.extend(result.jsx_usages.iter().cloned());
    }
    let mut out = react_diagnostics(&components, &usages, root);
    let vue = crate::vue_mismatches(program, parsed, root)?;
    out.extend(vue_diagnostics(&vue, root));
    Ok(finish(out))
}
