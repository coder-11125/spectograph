//! Reporting: render analysis results as human-readable CLI output or versioned JSON.
//!
//! The JSON shape is a public contract (see `docs/json-schema.md`). Compatibility policy:
//! adding a field never bumps [`SCHEMA_VERSION`]; renaming, removing, or changing the type or
//! meaning of a field does. `spectograph-core/tests/report.rs` pins the exact shape.

use serde::Serialize;

use crate::analysis::{BloatFinding, BloatTier, Report};
use crate::diagnostic::Diagnostic;
use crate::ir::SymbolKind;

/// A machine-stable JSON report. Versioned so CI and `fix` can rely on the schema.
#[derive(Debug, Clone, Serialize)]
pub struct JsonReport {
    pub schema_version: u32,
    /// Analyzed language: `python`, `javascript`, or `typescript`.
    pub language: &'static str,
    pub total_symbols: usize,
    pub reachable_symbols: usize,
    pub bloat_symbols: usize,
    pub findings: Vec<JsonFinding>,
    /// Framework-level diagnostics (prop/binding mismatches). Informational; they don't
    /// count toward `bloat_symbols`.
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonFinding {
    pub tier: &'static str,
    pub name: String,
    /// What kind of symbol this is: `function`, `class`, `import`, `const`, `type`.
    pub kind: &'static str,
    pub file: String,
    pub line: usize,
    pub usage: u32,
    pub bloat_score: f64,
    pub ref_kinds: Vec<&'static str>,
}

pub const SCHEMA_VERSION: u32 = 1;

fn tier_str(t: BloatTier) -> &'static str {
    match t {
        BloatTier::Guaranteed => "guaranteed",
        BloatTier::Probable => "probable",
        BloatTier::HighCost => "high_cost",
    }
}

fn kind_str(k: SymbolKind) -> &'static str {
    match k {
        SymbolKind::Function => "function",
        SymbolKind::Class => "class",
        SymbolKind::Import => "import",
        SymbolKind::Export => "export",
        SymbolKind::Const => "const",
        SymbolKind::Type => "type",
        SymbolKind::Module => "module",
    }
}

impl JsonReport {
    pub fn from_report(report: &Report) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            language: report.language.name(),
            total_symbols: report.total_symbols,
            reachable_symbols: report.reachable_symbols,
            bloat_symbols: report.findings.len(),
            findings: report
                .findings
                .iter()
                .map(|f| JsonFinding {
                    tier: tier_str(f.tier),
                    name: f.name.clone(),
                    kind: kind_str(f.kind),
                    file: f.file.clone(),
                    line: f.line,
                    usage: f.usage,
                    bloat_score: f.bloat_score,
                    ref_kinds: f
                        .ref_kinds
                        .iter()
                        .map(|k| match k {
                            crate::ir::RefKind::Static => "static",
                            crate::ir::RefKind::ReExport => "reexport",
                            crate::ir::RefKind::Dynamic => "dynamic",
                            crate::ir::RefKind::Reflection => "reflection",
                            crate::ir::RefKind::StringKeyed => "string_keyed",
                        })
                        .collect(),
                })
                .collect(),
            diagnostics: report.diagnostics.clone(),
        }
    }
}

/// Render a human-readable, tiered report.
pub fn render_human(report: &Report) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Symbols: {} total, {} reachable, {} bloat ({})\n",
        report.total_symbols,
        report.reachable_symbols,
        report.findings.len(),
        report.language.name(),
    ));

    if report.findings.is_empty() {
        out.push_str("\nNo bloat detected. Lean.\n");
    } else {
        out.push('\n');
        for (tier, mark) in [
            (BloatTier::Guaranteed, "🔴"),
            (BloatTier::Probable, "🟠"),
            (BloatTier::HighCost, "🟡"),
        ] {
            let items: Vec<&BloatFinding> =
                report.findings.iter().filter(|f| f.tier == tier).collect();
            if items.is_empty() {
                continue;
            }
            let label = match tier {
                BloatTier::Guaranteed => "guaranteed bloat",
                BloatTier::Probable => "probably bloat",
                BloatTier::HighCost => "high cost / low usage",
            };
            out.push_str(&format!("{} {label} ({}):\n", mark, items.len()));
            for f in items {
                out.push_str(&format!(
                    "   {:<28} {}:{}   {}  usage={}  score={:.2}\n",
                    f.name,
                    f.file,
                    f.line,
                    kind_str(f.kind),
                    f.usage,
                    f.bloat_score
                ));
            }
            out.push('\n');
        }
    }

    if !report.diagnostics.is_empty() {
        out.push_str(&format!(
            "⚠️  framework diagnostics ({}):\n",
            report.diagnostics.len()
        ));
        for d in &report.diagnostics {
            out.push_str(&format!(
                "   [{}] {}:{}   {}\n",
                d.code, d.file, d.line, d.message
            ));
        }
        out.push('\n');
    }
    out
}
