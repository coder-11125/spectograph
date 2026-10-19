//! Bloat analysis: reachability from entrypoints plus cost/usage classification.
//!
//! Everything here is linear in `symbols + references`: edges are indexed once into
//! adjacency lists up front rather than re-scanning `Program::references` per symbol, which
//! is what keeps a 100k-symbol / 1M-reference project in the millisecond range.

use std::collections::{HashSet, VecDeque};
use std::path::Path;

use crate::cost::CostModel;
use crate::diagnostic::Diagnostic;
use crate::ir::{Language, Program, RefKind, SymbolId, SymbolKind};

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
    /// What the flagged symbol is: an unused `import`, a dead `function`, ...
    pub kind: SymbolKind,
    /// Path relative to the analyzed root (`/`-separated) when the symbol lives under it,
    /// otherwise the path as the analyzer reported it.
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
    pub language: Language,
    pub total_symbols: usize,
    pub reachable_symbols: usize,
    pub findings: Vec<BloatFinding>,
    /// Framework-level diagnostics attached by the caller (see [`Diagnostic`]). Empty after
    /// [`analyze`]; the analyzer's diagnostics are added by whoever ran the analyzer.
    pub diagnostics: Vec<Diagnostic>,
}

/// Render `file` relative to `root` with `/` separators, falling back to the path as given
/// when it isn't under `root`. Keeps reports portable across machines and operating systems.
pub fn display_path(root: &Path, file: &Path) -> String {
    let rel = file.strip_prefix(root).unwrap_or(file);
    rel.to_string_lossy().replace('\\', "/")
}

/// True for the edge kinds a reachability walk may follow. Dynamic edges cannot be trusted
/// for reachability and are handled by uncertainty/analysis instead.
fn is_traversable(kind: RefKind) -> bool {
    !matches!(
        kind,
        RefKind::Dynamic | RefKind::Reflection | RefKind::StringKeyed
    )
}

/// Compute which symbols are reachable from the program's entrypoints.
pub fn compute_reachability(program: &mut Program) {
    let n = program.symbols.len();

    // Reset.
    for sym in &mut program.symbols {
        sym.reachable = false;
    }
    program.reachable.clear();

    // Adjacency list over traversable edges, built once.
    let mut adjacency: Vec<Vec<SymbolId>> = vec![Vec::new(); n];
    for r in &program.references {
        if is_traversable(r.kind) && r.from < n {
            adjacency[r.from].push(r.to);
        }
    }

    // Seed from explicit entrypoints plus public API roots (exports). Exported names
    // are treated as used by external consumers, so they and their definitions stay
    // reachable and are never classified as bloat.
    let mut queue: VecDeque<SymbolId> = program.entrypoints.iter().copied().collect();
    for sym in &program.symbols {
        if sym.kind == SymbolKind::Export {
            queue.push_back(sym.id);
        }
    }
    let mut seen = vec![false; n];

    while let Some(id) = queue.pop_front() {
        if id >= n || seen[id] {
            continue;
        }
        seen[id] = true;
        program.reachable.push(id);
        program.symbols[id].reachable = true;
        queue.extend(adjacency[id].iter().copied());
    }
}

/// Analyze a program: compute reachability, then classify bloat.
pub fn analyze(program: &mut Program, model: &CostModel) -> Report {
    compute_reachability(program);

    let n = program.symbols.len();
    let mut incoming: Vec<Vec<RefKind>> = vec![Vec::new(); n];
    for r in &program.references {
        if r.to < n {
            incoming[r.to].push(r.kind);
        }
    }
    let entrypoints: HashSet<SymbolId> = program.entrypoints.iter().copied().collect();

    let mut findings = Vec::new();

    for sym in &program.symbols {
        // Modules and exports are structural/public declarations; skip. Entrypoints
        // are user-designated roots and are never bloat. External (third-party
        // library) symbols aren't actionable, so they're never reported either.
        if matches!(sym.kind, SymbolKind::Module | SymbolKind::Export)
            || entrypoints.contains(&sym.id)
            || sym.is_external
        {
            continue;
        }

        let ref_kinds: Vec<RefKind> = incoming.get(sym.id).cloned().unwrap_or_default();
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
            // Mixed: has some static refs (reachability handled it) -> not dead.
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
                kind: sym.kind,
                file: display_path(&program.root, &sym.file),
                line: sym.span.start_line,
                usage: model.usage_of(sym.id),
                bloat_score: model.bloat_score(sym.id),
                ref_kinds,
            });
        }
    }

    // Sort by tier (most certain first), then by score desc within tier, then by location so
    // output is deterministic run to run.
    findings.sort_by(|a, b| {
        a.tier
            .cmp(&b.tier)
            .then(
                b.bloat_score
                    .partial_cmp(&a.bloat_score)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then_with(|| a.file.cmp(&b.file))
            .then(a.line.cmp(&b.line))
            .then_with(|| a.name.cmp(&b.name))
    });

    Report {
        language: program.language,
        total_symbols: n,
        reachable_symbols: program.reachable.len(),
        findings,
        diagnostics: Vec::new(),
    }
}
