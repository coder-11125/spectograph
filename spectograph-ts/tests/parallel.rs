//! Parallel parsing relocates file-local ids; the TS result carries more id-bearing data
//! than JS (decorators, Angular selectors/pipes), so check those survive relocation on a
//! project large enough to take the parallel path.

use spectograph_analyzer::Analyzer;
use spectograph_core::analysis::{self, BloatTier};
use spectograph_core::cost::{CostModel, CostSource};
use spectograph_ts::TypeScriptAnalyzer;
use tempfile::tempdir;

const FILLER: usize = 40;

fn write_project(dir: &std::path::Path) {
    std::fs::write(dir.join("package.json"), r#"{"main":"main.ts"}"#).unwrap();
    // Filler modules first alphabetically so the Angular files get non-zero offsets.
    for i in 0..FILLER {
        std::fs::write(
            dir.join(format!("a_fill{i}.ts")),
            format!("export function fill{i}() {{}}\n"),
        )
        .unwrap();
    }
    std::fs::write(
        dir.join("main.ts"),
        "import { ParentComponent } from './parent.component';\nnew ParentComponent();\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("parent.component.ts"),
        "import { Component } from '@angular/core';\n\
         @Component({ selector: 'app-parent', template: '<app-child [value]=\"1\"></app-child>' })\n\
         export class ParentComponent {}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("child.component.ts"),
        "import { Component, Input } from '@angular/core';\n\
         @Component({ selector: 'app-child', template: '' })\n\
         export class ChildComponent { @Input() value = 0; }\n",
    )
    .unwrap();
}

#[test]
fn ids_dense_and_angular_selector_edges_survive_relocation() {
    let dir = tempdir().unwrap();
    write_project(dir.path());

    let mut program = TypeScriptAnalyzer.analyze(dir.path()).unwrap();
    for (i, s) in program.symbols.iter().enumerate() {
        assert_eq!(s.id, i, "symbol ids must be dense indices");
    }
    let model = CostModel::build(&program, CostSource::Estimated);
    let dead: Vec<String> = analysis::analyze(&mut program, &model)
        .findings
        .into_iter()
        .filter(|f| f.tier == BloatTier::Guaranteed)
        .map(|f| f.name)
        .collect();
    // ChildComponent is reachable only through ParentComponent's template selector match,
    // which depends on the Angular `class_id`s having been relocated correctly.
    assert!(
        !dead.contains(&"ChildComponent".to_string()),
        "ChildComponent wrongly reported: {dead:?}"
    );
}

#[test]
fn analysis_is_deterministic_under_parallel_parsing() {
    let dir = tempdir().unwrap();
    write_project(dir.path());
    let shape = |p: &spectograph_core::ir::Program| {
        (
            p.symbols
                .iter()
                .map(|s| (s.id, s.name.clone()))
                .collect::<Vec<_>>(),
            p.references
                .iter()
                .map(|r| (r.from, r.to))
                .collect::<Vec<_>>(),
        )
    };
    let a = TypeScriptAnalyzer.analyze(dir.path()).unwrap();
    let b = TypeScriptAnalyzer.analyze(dir.path()).unwrap();
    assert_eq!(shape(&a), shape(&b));
}
