//! Bloat analysis: reachability from entrypoints plus cost/usage classification.

use std::collections::VecDeque;

use crate::cost::CostModel;
use crate::ir::{Program, RefKind, SymbolId, SymbolKind};

/// Severity tiers for reported bloat, ordered from most to least certain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BloatTier {
    /// Guaranteed bloat: zero usage, all references static, unreachable.
    Guaranteed,
    /// Probably bloat: only uncertain/dynamic usage, unreachable.
    Probable,
    /// High cost / low usage: used, but taxing relative to how often it runs.
    HighCost,
}

/// A single bloat finding.
#[derive(Debug, Clone)]
pub struct BloatFinding {
    pub id: SymbolId,
    pub tier: BloatTier,
    pub name: String,
    pub file: String,
    pub line: usize,
    pub usage: u32,
    pub bloat_score: f64,
    /// Reference kinds pointing at this symbol (for diagnostics).
    pub ref_kinds: Vec<RefKind>,
}

/// A report: the reachability result plus ranked findings.
#[derive(Debug, Clone)]
pub struct Report {
    pub total_symbols: usize,
    pub reachable_symbols: usize,
    pub findings: Vec<BloatFinding>,
}

/// Compute which symbols are reachable from the program's entrypoints.
pub fn compute_reachability(program: &mut Program) {
    // Reset.
    for sym in &mut program.symbols {
        sym.reachable = false;
    }
    program.reachable.clear();

    let mut queue: VecDeque<SymbolId> = program.entrypoints.iter().copied().collect();
    let mut seen = std::collections::HashSet::new();

    while let Some(id) = queue.pop_front() {
        if !seen.insert(id) {
            continue;
        }
        program.reachable.push(id);
        if let Some(sym) = program.symbols.get_mut(id) {
            sym.reachable = true;
        }
        for r in program.outgoing(id) {
            // Traverse static and re-export edges; dynamic edges cannot be trusted
            // for reachability and are handled by uncertainty/analysis instead.
            if !matches!(r.kind, RefKind::Dynamic | RefKind::Reflection | RefKind::StringKeyed) {
                queue.push_back(r.to);
            }
        }
    }
}

/// Analyze a program: compute reachability, then classify bloat.
pub fn analyze(program: &mut Program, model: &CostModel) -> Report {
    compute_reachability(program);

    let mut findings = Vec::new();

    for sym in &program.symbols {
        // Modules and entrypoints are structural; skip.
        if sym.kind == SymbolKind::Module || program.entrypoints.contains(&sym.id) {
            continue;
        }

        let ref_kinds: Vec<RefKind> = program.incoming(sym.id).map(|r| r.kind).collect();
        let any_uncertain = ref_kinds.iter().any(|k| k.is_uncertain());

        let tier = if sym.reachable {
            // Used but possibly taxing: classify by cost/usage.
            let score = model.bloat_score(sym.id);
            if score >= 1.0 {
                Some(BloatTier::HighCost)
            } else {
                None
            }
        } else if ref_kinds.is_empty() {
            // No references at all -> guaranteed dead (only if no uncertainty to worry about).
            Some(BloatTier::Guaranteed)
        } else if any_uncertain && ref_kinds.iter().all(|k| k.is_uncertain()) {
            // Only uncertain references -> probably dead.
            Some(BloatTier::Probable)
        } else if any_uncertain {
            // Mixed: has some static refs (reachable check handled it) -> not dead.
            None
        } else {
            // Unreachable, but has static references. Static refs from an unreachable
            // ancestor mean it's part of a dead subgraph -> guaranteed dead.
            Some(BloatTier::Guaranteed)
        };

        if let Some(tier) = tier {
            findings.push(BloatFinding {
                id: sym.id,
                tier,
                name: sym.name.clone(),
                file: sym.file.to_string_lossy().into_owned(),
                line: sym.span.start_line,
                usage: model.usage_of(sym.id),
                bloat_score: model.bloat_score(sym.id),
                ref_kinds,
            });
        }
    }

    // Sort by tier (most certain first), then by score desc within tier.
    findings.sort_by(|a, b| {
        a.tier
            .cmp(&b.tier)
            .then(b.bloat_score.partial_cmp(&a.bloat_score).unwrap_or(std::cmp::Ordering::Equal))
    });

    Report {
        total_symbols: program.symbols.len(),
        reachable_symbols: program.reachable.len(),
        findings,
    }
}
