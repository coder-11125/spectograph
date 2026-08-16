//! Integration tests for the Python analyzer's resolution quality.

use std::path::Path;

use spectograph_analyzer::Analyzer;
use spectograph_core::analysis;
use spectograph_core::cost::CostModel;
use spectograph_core::ir::SymbolKind;
use spectograph_py::PythonAnalyzer;

/// Run the analyzer on a temporary dir containing the given (filename -> source) files.
fn analyze(files: &[(&str, &str)]) -> spectograph_core::ir::Program {
    let dir = tempfile::tempdir().unwrap();
    for (name, src) in files {
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, src).unwrap();
    }
    PythonAnalyzer.analyze(dir.path()).expect("analyze")
}

fn report(program: &mut spectograph_core::ir::Program) -> analysis::Report {
    let model = CostModel::build(program, spectograph_core::cost::CostSource::Estimated);
    analysis::analyze(program, &model)
}

fn find<'a>(report: &'a analysis::Report, name: &str) -> Option<&'a analysis::BloatFinding> {
    report.findings.iter().find(|f| f.name == name)
}

/// A function called only via a local variable shadowing a module function must NOT
/// create a false "used" reference.
#[test]
fn local_shadowing_does_not_falsely_use_module_symbol() {
    let mut program = analyze(&[(
        "main.py",
        r#"
def helper():
    return 1

def main():
    helper = lambda: 2   # shadows module `helper`
    return helper()

if __name__ == "__main__":
    main()
"#,
    )]);
    let report = report(&mut program);
    // The module `helper` is referenced nowhere reachable, so it's bloat (unused).
    assert!(
        find(&report, "helper").is_some(),
        "shadowed helper should be unused"
    );
}

/// `from utils import helper as h; h()` must make `helper` reachable through the alias.
#[test]
fn aliased_import_keeps_target_reachable() {
    let mut program = analyze(&[
        (
            "main.py",
            "from utils import helper as h\ndef main():\n    return h()\nif __name__ == '__main__':\n    main()\n",
        ),
        ("utils.py", "def helper():\n    return 2\n"),
    ]);
    let report = report(&mut program);
    assert!(
        find(&report, "helper").is_none(),
        "helper should be used via alias"
    );
    // The alias import binding should exist.
    let import = program
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Import && s.name == "helper");
    assert!(import.is_some(), "import symbol for helper should exist");
}

/// A function referenced only through `getattr(x, "name")` must be classified as
/// probable (uncertain) bloat, not guaranteed, and the edge must be Reflection.
#[test]
fn getattr_reflection_marks_probable_bloat() {
    let mut program = analyze(&[(
        "main.py",
        "def maybe_called():\n    return 1\ndef main():\n    return getattr(None, \"maybe_called\")\nif __name__ == '__main__':\n    main()\n",
    )]);
    let report = report(&mut program);
    let reflection = program
        .references
        .iter()
        .any(|r| r.kind == spectograph_core::ir::RefKind::Reflection);
    assert!(reflection, "getattr should produce a reflection reference");
    let finding = find(&report, "maybe_called").expect("maybe_called finding");
    assert_eq!(
        finding.tier,
        spectograph_core::analysis::BloatTier::Probable,
        "reflection-only usage should be probable bloat"
    );
}

/// `__import__("name")` must produce a dynamic (uncertain) reference, not a static one.
#[test]
fn import_dynamic_marked_uncertain() {
    let program = analyze(&[(
        "main.py",
        "def helper():\n    return 1\ndef load():\n    return __import__(\"helper\")\nif __name__ == '__main__':\n    load()\n",
    )]);
    let dynamic = program
        .references
        .iter()
        .any(|r| r.kind == spectograph_core::ir::RefKind::Dynamic);
    assert!(dynamic, "__import__ should produce a dynamic reference");
}

/// A star import must not fabricate static "used" edges (it is uncertain).
#[test]
fn star_import_is_uncertain_not_false_static() {
    let program = analyze(&[
        (
            "main.py",
            "from utils import *\ndef main():\n    return anything()\nif __name__ == '__main__':\n    main()\n",
        ),
        ("utils.py", "def anything():\n    return 3\n"),
    ]);
    // `anything` should not be marked reachable via the star import alone.
    let anything = program
        .symbols
        .iter()
        .find(|s| s.name == "anything")
        .expect("anything symbol");
    assert!(
        !anything.reachable,
        "star-imported symbol must not be statically reachable"
    );
}

/// A method called only via `self.method()` inside another method stays reachable.
#[test]
fn self_method_call_keeps_method_reachable() {
    let mut program = analyze(&[(
        "main.py",
        concat!(
            "class A:\n",
            "    def run(self):\n",
            "        return self.step()\n",
            "    def step(self):\n",
            "        return 1\n",
            "    def dead(self):\n",
            "        return 2\n",
            "def main():\n",
            "    return A().run()\n",
            "if __name__ == '__main__':\n",
            "    main()\n",
        ),
    )]);
    let report = report(&mut program);
    assert!(find(&report, "run").is_none(), "run should be reachable");
    assert!(
        find(&report, "step").is_none(),
        "step via self.step() should be reachable"
    );
    assert!(
        find(&report, "dead").is_some(),
        "dead method should be bloat"
    );
}

/// A method called only via `obj.method()` where obj is a local instance is reachable.
#[test]
fn obj_method_call_keeps_method_reachable() {
    let mut program = analyze(&[(
        "main.py",
        concat!(
            "class B:\n",
            "    def handle(self):\n",
            "        return 1\n",
            "def main():\n",
            "    b = B()\n",
            "    return b.handle()\n",
            "if __name__ == '__main__':\n",
            "    main()\n",
        ),
    )]);
    let report = report(&mut program);
    assert!(
        find(&report, "handle").is_none(),
        "handle via b.handle() should be reachable"
    );
}

/// Cross-file usage through a plain module import must keep the target reachable.
#[test]
fn cross_file_import_keeps_target_reachable() {
    let mut program = analyze(&[
        (
            "main.py",
            "from utils import helper\ndef main():\n    return helper()\nif __name__ == '__main__':\n    main()\n",
        ),
        ("utils.py", "def helper():\n    return 2\n"),
    ]);
    let report = report(&mut program);
    assert!(
        find(&report, "helper").is_none(),
        "helper should be reachable"
    );
    let _ = Path::new("");
}
