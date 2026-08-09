//! Spectograph language analyzer trait and harness.
//!
//! A language crate implements [`Analyzer`] to turn a directory of source files into a
//! language-agnostic [`Program`] that `spectograph-core` can analyze.

pub mod discover;

use std::path::Path;

use spectograph_core::cost::{Cost, CostModel, CostSource};
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

    /// Build a cost model for the program. Default: static estimate.
    ///
    /// Runtime profiling implementations should override this and return
    /// [`CostSource::Profiled`]. Static estimation is the fallback when no workload
    /// (entry/test command) is available.
    fn cost_model(&self, program: &Program) -> CostModel {
        let mut model = CostModel::build(program, CostSource::Estimated);
        for sym in &program.symbols {
            let cost = model.cost_of(sym.id);
            let imports = program.incoming(sym.id).count() as f64;
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
