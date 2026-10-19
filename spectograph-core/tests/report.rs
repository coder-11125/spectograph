//! The JSON report is a public contract (`docs/json-schema.md`): pin its exact shape so a
//! change to it is always a deliberate, reviewed one.

use std::path::PathBuf;

use serde_json::json;
use spectograph_core::analysis::{self, display_path};
use spectograph_core::cost::{CostModel, CostSource};
use spectograph_core::diagnostic::{Diagnostic, REACT_PROP_MISMATCH};
use spectograph_core::ir::{
    Environment, Language, Program, RefKind, Reference, SourceSpan, Symbol, SymbolKind,
};
use spectograph_core::report::{render_human, JsonReport, SCHEMA_VERSION};

fn sym(id: usize, name: &str, kind: SymbolKind, file: &str, line: usize) -> Symbol {
    Symbol {
        id,
        name: name.to_string(),
        kind,
        file: PathBuf::from(file),
        span: SourceSpan::new(line, 1, line, 1),
        reachable: false,
        environment: Environment::Universal,
        is_external: false,
    }
}

fn sample_report() -> analysis::Report {
    // module(0) -> used(1); orphan(2) is dead; only_dynamic(3) is referenced dynamically.
    let mut program = Program {
        language: Language::TypeScript,
        root: PathBuf::from("/proj"),
        symbols: vec![
            sym(0, "index", SymbolKind::Module, "/proj/src/index.ts", 1),
            sym(1, "used", SymbolKind::Function, "/proj/src/index.ts", 3),
            sym(2, "orphan", SymbolKind::Function, "/proj/src/dead.ts", 7),
            sym(
                3,
                "only_dynamic",
                SymbolKind::Import,
                "/proj/src/lazy.ts",
                2,
            ),
        ],
        references: vec![
            Reference {
                from: 0,
                to: 1,
                kind: RefKind::Static,
                environment: Environment::Universal,
            },
            Reference {
                from: 0,
                to: 3,
                kind: RefKind::Dynamic,
                environment: Environment::Universal,
            },
        ],
        entrypoints: vec![0],
        reachable: Vec::new(),
    };
    let model = CostModel::build(&program, CostSource::Estimated);
    let mut report = analysis::analyze(&mut program, &model);
    report.diagnostics.push(Diagnostic {
        code: REACT_PROP_MISMATCH,
        message: "<Card> receives prop `foo` that the component does not declare".to_string(),
        file: "src/ui.tsx".to_string(),
        line: 12,
    });
    report
}

#[test]
fn json_report_shape_is_pinned() {
    let json = serde_json::to_value(JsonReport::from_report(&sample_report())).unwrap();
    assert_eq!(
        json,
        json!({
            "schema_version": 1,
            "language": "typescript",
            "total_symbols": 4,
            "reachable_symbols": 2,
            "bloat_symbols": 2,
            "findings": [
                {
                    "tier": "guaranteed",
                    "name": "orphan",
                    "kind": "function",
                    "file": "src/dead.ts",
                    "line": 7,
                    "usage": 0,
                    "bloat_score": json["findings"][0]["bloat_score"],
                    "ref_kinds": []
                },
                {
                    "tier": "probable",
                    "name": "only_dynamic",
                    "kind": "import",
                    "file": "src/lazy.ts",
                    "line": 2,
                    "usage": 0,
                    "bloat_score": json["findings"][1]["bloat_score"],
                    "ref_kinds": ["dynamic"]
                }
            ],
            "diagnostics": [
                {
                    "code": "react-prop-mismatch",
                    "message": "<Card> receives prop `foo` that the component does not declare",
                    "file": "src/ui.tsx",
                    "line": 12
                }
            ]
        })
    );
    assert_eq!(SCHEMA_VERSION, 1);
}

#[test]
fn findings_are_deterministically_ordered() {
    let a = JsonReport::from_report(&sample_report());
    let b = JsonReport::from_report(&sample_report());
    let names = |r: &JsonReport| {
        r.findings
            .iter()
            .map(|f| f.name.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&a), names(&b));
}

#[test]
fn human_report_lists_tiers_kinds_and_diagnostics() {
    let text = render_human(&sample_report());
    assert!(text.contains("guaranteed bloat (1)"));
    assert!(text.contains("probably bloat (1)"));
    assert!(text.contains("src/dead.ts:7"));
    assert!(text.contains("function"));
    assert!(text.contains("[react-prop-mismatch] src/ui.tsx:12"));
    assert!(text.contains("(typescript)"));
}

#[test]
fn display_path_is_root_relative_with_forward_slashes() {
    let root = PathBuf::from("/proj");
    assert_eq!(
        display_path(&root, &PathBuf::from("/proj/src/a.ts")),
        "src/a.ts"
    );
    // Outside the root: left as given rather than mangled.
    assert_eq!(
        display_path(&root, &PathBuf::from("/other/a.ts")),
        "/other/a.ts"
    );
}
