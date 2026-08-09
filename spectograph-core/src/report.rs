//! Reporting: render analysis results as human-readable CLI output or versioned JSON.

use serde::Serialize;

use crate::analysis::{BloatFinding, BloatTier, Report};

/// A machine-stable JSON report. Versioned so CI and `fix` can rely on the schema.
#[derive(Debug, Clone, Serialize)]
pub struct JsonReport {
    pub schema_version: u32,
    pub total_symbols: usize,
    pub reachable_symbols: usize,
    pub bloat_symbols: usize,
    pub findings: Vec<JsonFinding>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonFinding {
    pub tier: &'static str,
    pub name: String,
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

impl JsonReport {
    pub fn from_report(report: &Report) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            total_symbols: report.total_symbols,
            reachable_symbols: report.reachable_symbols,
            bloat_symbols: report.findings.len(),
            findings: report
                .findings
                .iter()
                .map(|f| JsonFinding {
                    tier: tier_str(f.tier),
                    name: f.name.clone(),
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
        }
    }
}

/// Render a human-readable, tiered report.
pub fn render_human(report: &Report, show_used: bool) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Symbols: {} total, {} reachable, {} bloat\n",
        report.total_symbols, report.reachable_symbols, report.findings.len()
    ));

    if report.findings.is_empty() {
        out.push_str("\nNo bloat detected. Lean.\n");
        return out;
    }

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
                "   {:<28} {}:{}   usage={}  score={:.2}\n",
                f.name, f.file, f.line, f.usage, f.bloat_score
            ));
        }
        out.push('\n');
    }

    if !show_used {
        let _ = show_used;
    }
    out
}
