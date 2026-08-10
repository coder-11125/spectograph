//! Integration tests for `__init__.py` re-exports and `__all__` package handling.

use spectograph_analyzer::Analyzer;
use spectograph_core::analysis;
use spectograph_core::cost::CostModel;
use spectograph_core::ir::SymbolKind;
use spectograph_py::PythonAnalyzer;

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

fn is_export(program: &spectograph_core::ir::Program, name: &str) -> bool {
    program
        .symbols
        .iter()
        .any(|s| s.kind == SymbolKind::Export && s.name == name)
}

/// A member re-exported through `__init__.py` and imported by a consumer is reachable.
#[test]
fn init_py_reexport_keeps_member_reachable() {
    let mut program = analyze(&[
        ("pkg/__init__.py", "from .maths import add\n"),
        ("pkg/maths.py", "def add(a,b):\n    return a+b\ndef unused():\n    return 1\n"),
        ("main.py", "from pkg import add\ndef main():\n    return add(1,2)\nif __name__ == '__main__':\n    main()\n"),
    ]);
    let report = report(&mut program);
    assert!(find(&report, "add").is_none(), "add should be reachable via re-export");
    assert!(
        find(&report, "unused").is_some(),
        "unused non-exported member should be bloat"
    );
}

/// A name listed in `__all__` is public API and must not be flagged as bloat, while a
/// non-exported member of the same module is.
#[test]
fn all_protects_public_members_only() {
    let mut program = analyze(&[(
        "lib.py",
        "__all__ = ['exported']\ndef exported():\n    return 2\ndef private():\n    return 3\n",
    )]);
    let report = report(&mut program);
    assert!(is_export(&program, "exported"), "exported should have an Export symbol");
    assert!(
        find(&report, "exported").is_none(),
        "__all__ member should not be bloat"
    );
    assert!(
        find(&report, "private").is_some(),
        "non-exported member should be bloat"
    );
}

/// The definition behind an `__all__` entry is reachable via the export edge.
#[test]
fn all_links_export_to_definition() {
    let mut program = analyze(&[(
        "lib.py",
        "__all__ = ['helper']\ndef helper():\n    return 1\n",
    )]);
    // Reachability is computed inside analysis; run it first.
    report(&mut program);
    let helper = program
        .symbols
        .iter()
        .find(|s| s.name == "helper" && s.kind == SymbolKind::Function)
        .expect("helper def");
    assert!(helper.reachable, "helper behind __all__ should be reachable");
}

/// A member re-exported in `__init__.py` but defined in a submodule is reachable and
/// not flagged; a genuinely unused submodule member is.
#[test]
fn package_level_dead_code_found() {
    let mut program = analyze(&[
        ("pkg/__init__.py", "from .strings import to_upper\n"),
        ("pkg/strings.py", "def to_upper(s):\n    return s.upper()\ndef dead_string_fn():\n    return 'dead'\n"),
        ("main.py", "from pkg import to_upper\ndef main():\n    return to_upper('hi')\nif __name__ == '__main__':\n    main()\n"),
    ]);
    let report = report(&mut program);
    assert!(find(&report, "to_upper").is_none(), "to_upper should be reachable");
    assert!(
        find(&report, "dead_string_fn").is_some(),
        "dead_string_fn should be flagged as package-level dead code"
    );
}
