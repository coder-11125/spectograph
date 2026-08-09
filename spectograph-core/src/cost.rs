//! Cost model: attribute cost to symbols, and compute the bloat score.

use std::collections::HashMap;

use crate::ir::{Program, SymbolId};

/// Measured or estimated runtime cost attributed to a symbol.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Cost {
    /// CPU cost (estimated or measured), in abstract cost units.
    pub cpu: f64,
    /// Import-chain weight: how much this symbol drags in when its module loads.
    pub import_weight: f64,
    /// Source size in lines (static fallback signal).
    pub loc: u32,
}

impl Cost {
    /// A single weighted cost used for scoring and ranking.
    pub fn total(&self) -> f64 {
        self.cpu + self.import_weight + self.loc as f64 * 0.05
    }
}

/// How cost is derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostSource {
    /// Measured from runtime profiling.
    Profiled,
    /// Estimated statically (no workload was available).
    Estimated,
}

/// The per-symbol cost/usage model for a [`Program`].
pub struct CostModel {
    /// Cost attributed to each symbol.
    pub costs: HashMap<SymbolId, Cost>,
    /// Usage intensity (count of non-uncertain incoming references) per symbol.
    pub usage: HashMap<SymbolId, u32>,
    /// Whether cost came from profiling or estimation.
    pub source: CostSource,
}

impl CostModel {
    pub fn new(source: CostSource) -> Self {
        Self {
            costs: HashMap::new(),
            usage: HashMap::new(),
            source,
        }
    }

    /// Build a cost model from a program, computing usage intensity from references.
    pub fn build(program: &Program, source: CostSource) -> Self {
        let mut model = Self::new(source);
        for sym in &program.symbols {
            model.costs.insert(sym.id, Cost::default());
            model.usage.insert(sym.id, 0);
        }
        for r in &program.references {
            if let Some(u) = model.usage.get_mut(&r.to) {
                // Ignore uncertain refs when counting trustworthy usage.
                if !r.kind.is_uncertain() {
                    *u += 1;
                }
            }
        }
        model
    }

    /// Usage intensity for a symbol (0 if absent).
    pub fn usage_of(&self, id: SymbolId) -> u32 {
        self.usage.get(&id).copied().unwrap_or(0)
    }

    /// Cost for a symbol (default if absent).
    pub fn cost_of(&self, id: SymbolId) -> Cost {
        self.costs.get(&id).copied().unwrap_or_default()
    }

    /// The bloat score: cost / usage. High cost with low usage is bloat.
    /// Returns 0 for symbols with no measured cost (never counted as bloat on cost alone).
    pub fn bloat_score(&self, id: SymbolId) -> f64 {
        let cost = self.cost_of(id).total();
        let usage = self.usage_of(id).max(1); // avoid div-by-zero; floor usage at 1
        cost / usage as f64
    }
}
