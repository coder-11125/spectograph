//! Spectograph language analyzer trait and harness.
//!
//! A language crate implements [`Analyzer`] to turn a directory of source files into a
//! language-agnostic [`Program`] that `spectograph-core` can analyze.

pub mod discover;
pub mod parallel;

use std::path::Path;

use spectograph_core::cost::{Cost, CostModel, CostSource};
use spectograph_core::diagnostic::Diagnostic;
use spectograph_core::ir::{Language, Program};

/// Errors produced while analyzing source.
#[derive(Debug, thiserror::Error)]
pub enum AnalyzeError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unsupported language {0:?} for path {1}")]
    UnsupportedLanguage(Language, String),
    #[error("no source files found under {0}")]
    NoSourceFiles(String),
}

/// A language analyzer: source tree in, [`Program`] out.
pub trait Analyzer {
    /// The language this analyzer handles.
    fn language(&self) -> Language;

    /// Parse and resolve `root`, producing a [`Program`].
    ///
    /// Implementations are responsible for respecting `.gitignore`, discovering the
    /// language's source files, extracting symbols, and resolving references. The
    /// [`Program`] must set `entrypoints` appropriately for reachability analysis.
    fn analyze(&self, root: &Path) -> Result<Program, AnalyzeError>;

    /// Like [`Analyzer::analyze`], but also returns framework-level diagnostics (prop and
    /// binding mismatches, ...) collected in the same pass. The default has none; analyzers
    /// with framework support override it.
    fn analyze_with_diagnostics(
        &self,
        root: &Path,
    ) -> Result<(Program, Vec<Diagnostic>), AnalyzeError> {
        Ok((self.analyze(root)?, Vec::new()))
    }

    /// Build a cost model for the program. Default: static estimate.
    ///
    /// Runtime profiling implementations should override this and return
    /// [`CostSource::Profiled`]. Static estimation is the fallback when no workload
    /// (entry/test command) is available.
    fn cost_model(&self, program: &Program) -> CostModel {
        let mut model = CostModel::build(program, CostSource::Estimated);

        // Count every incoming reference (certain or not) per symbol in one pass.
        let mut incoming = vec![0u32; program.symbols.len()];
        for r in &program.references {
            if let Some(count) = incoming.get_mut(r.to) {
                *count += 1;
            }
        }

        for sym in &program.symbols {
            let cost = model.cost_of(sym.id);
            let imports = incoming.get(sym.id).copied().unwrap_or(0) as f64;
            model.costs.insert(
                sym.id,
                Cost {
                    cpu: cost.cpu + 0.05,
                    import_weight: imports * 0.1,
                    loc: 1,
                },
            );
        }
        model
    }
}
