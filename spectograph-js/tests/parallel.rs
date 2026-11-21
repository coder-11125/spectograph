//! Parsing runs in parallel above a file-count threshold; ids must still come out dense and
//! deterministic, and every id-bearing structure must be relocated consistently.

use spectograph_analyzer::Analyzer;
use spectograph_core::analysis::{self, BloatTier};
use spectograph_core::cost::{CostModel, CostSource};
use spectograph_core::ir::Program;
use spectograph_js::JavaScriptAnalyzer;
use tempfile::tempdir;

const FILES: usize = 60;

/// main.js -> mod0 -> mod1 -> ...; each module exports `used{i}` and has an unused `dead{i}`.
fn write_project(dir: &std::path::Path) {
    std::fs::write(
        dir.join("main.js"),
        "import { used0 } from './mod0.js';\nused0();\n",
    )
    .unwrap();
    std::fs::write(dir.join("package.json"), r#"{"main":"main.js"}"#).unwrap();
    for i in 0..FILES {
        let (import, call) = if i + 1 < FILES {
            (
                format!("import {{ used{n} }} from './mod{n}.js';\n", n = i + 1),
                format!("used{}();", i + 1),
            )
        } else {
            (String::new(), String::new())
        };
        std::fs::write(
            dir.join(format!("mod{i}.js")),
            format!("{import}export function used{i}() {{ {call} }}\nfunction dead{i}() {{}}\n"),
        )
        .unwrap();
    }
}

type Shape = (Vec<(usize, String)>, Vec<(usize, usize)>);

fn shape(p: &Program) -> Shape {
    (
        p.symbols.iter().map(|s| (s.id, s.name.clone())).collect(),
        p.references.iter().map(|r| (r.from, r.to)).collect(),
    )
}

#[test]
fn ids_are_dense_and_deterministic_under_parallel_parsing() {
    let dir = tempdir().unwrap();
    write_project(dir.path());

    let first = JavaScriptAnalyzer.analyze(dir.path()).unwrap();
    let second = JavaScriptAnalyzer.analyze(dir.path()).unwrap();

    for (i, s) in first.symbols.iter().enumerate() {
        assert_eq!(s.id, i, "symbol ids must be dense indices");
    }
    for r in &first.references {
        assert!(r.from < first.symbols.len() && r.to < first.symbols.len());
    }
    assert_eq!(shape(&first), shape(&second));
}

#[test]
fn reachability_is_correct_across_relocated_ids() {
    let dir = tempdir().unwrap();
    write_project(dir.path());

    let mut program = JavaScriptAnalyzer.analyze(dir.path()).unwrap();
    let model = CostModel::build(&program, CostSource::Estimated);
    let dead: Vec<String> = analysis::analyze(&mut program, &model)
        .findings
        .into_iter()
        .filter(|f| f.tier == BloatTier::Guaranteed)
        .map(|f| f.name)
        .collect();
    for i in 0..FILES {
        assert!(dead.contains(&format!("dead{i}")), "dead{i} not reported");
        assert!(
            !dead.contains(&format!("used{i}")),
            "used{i} wrongly reported"
        );
    }
}
