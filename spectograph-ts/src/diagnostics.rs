//! Framework diagnostics for the TypeScript analyzer: React and Vue (shared with
//! `spectograph-js`) plus Angular template-binding mismatches.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use spectograph_analyzer::AnalyzeError;
use spectograph_core::analysis::display_path;
use spectograph_core::diagnostic::{Diagnostic, ANGULAR_BINDING_MISMATCH};
use spectograph_core::ir::{Program, SymbolId, SymbolKind};
use spectograph_js::diagnostics::{finish, react_diagnostics, vue_diagnostics};

use crate::angular;
use crate::parse::FileResult;

/// All framework diagnostics for an already-parsed TypeScript project.
pub(crate) fn collect(
    program: &Program,
    parsed: &[(PathBuf, FileResult)],
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
    // Unused scoped-style classes in the `.vue` files this analyzer owns (`lang="ts"`).
    out.extend(spectograph_js::vue_css::unused_css_diagnostics(
        root,
        |lang| lang == Some(spectograph_js::vue::ScriptLang::TypeScript),
    )?);

    let mut global_names: HashMap<String, SymbolId> = HashMap::new();
    for sym in &program.symbols {
        if sym.kind != SymbolKind::Module {
            global_names.entry(sym.name.clone()).or_insert(sym.id);
        }
    }
    let selectors = crate::build_angular_selectors(parsed, &global_names);
    out.extend(
        angular::find_binding_mismatches(&selectors)
            .into_iter()
            .map(|m| Diagnostic {
                code: ANGULAR_BINDING_MISMATCH,
                message: format!(
                    "<{}> is bound `{}`, which the component does not declare as an @Input()/@Output()",
                    m.target_component, m.binding
                ),
                file: display_path(root, &m.file),
                line: m.line,
            }),
    );

    Ok(finish(out))
}
