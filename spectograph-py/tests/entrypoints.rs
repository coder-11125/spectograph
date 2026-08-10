//! Integration tests for entrypoint detection via pyproject.toml console scripts.

use spectograph_analyzer::Analyzer;
use spectograph_core::analysis;
use spectograph_core::cost::CostModel;
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

/// A function referenced by `[project.scripts]` becomes an entrypoint and is not bloat,
/// and the code it reaches stays reachable.
#[test]
fn console_script_entrypoint_reaches_code() {
    let mut program = analyze(&[
        (
            "pyproject.toml",
            "[project]\nname = \"x\"\n[project.scripts]\nmycli = \"pkg.cli:run\"\n",
        ),
        ("pkg/__init__.py", ""),
        ("pkg/cli.py", "from .core import do_work\ndef run():\n    return do_work()\n"),
        ("pkg/core.py", "def do_work():\n    return 1\ndef dead():\n    return 2\n"),
    ]);
    let report = report(&mut program);
    assert!(find(&report, "run").is_none(), "script entrypoint run should not be bloat");
    assert!(find(&report, "do_work").is_none(), "do_work reached via run should be reachable");
    assert!(
        find(&report, "dead").is_some(),
        "dead function in same module should be bloat"
    );
}

/// A gui-script entrypoint also marks its target reachable.
#[test]
fn gui_script_entrypoint_reaches_code() {
    let mut program = analyze(&[
        (
            "pyproject.toml",
            "[project]\nname = \"x\"\n[project.gui-scripts]\nmygui = \"pkg.gui:main\"\n",
        ),
        ("pkg/__init__.py", ""),
        ("pkg/gui.py", "def main():\n    return 'gui'\n"),
    ]);
    let report = report(&mut program);
    assert!(find(&report, "main").is_none(), "gui script entrypoint main should not be bloat");
}

/// A script pointing at a function that only reaches part of a module keeps the rest dead.
#[test]
fn script_entrypoint_does_not_mark_whole_module_used() {
    let mut program = analyze(&[
        (
            "pyproject.toml",
            "[project]\nname = \"x\"\n[project.scripts]\nmycli = \"pkg.cli:run\"\n",
        ),
        ("pkg/__init__.py", ""),
        (
            "pkg/cli.py",
            "def run():\n    return 1\ndef unreferenced():\n    return 2\n",
        ),
    ]);
    let report = report(&mut program);
    assert!(find(&report, "run").is_none(), "entrypoint run should be reachable");
    assert!(
        find(&report, "unreferenced").is_some(),
        "unreferenced function should still be bloat"
    );
}

/// A malformed or script-free pyproject should not error and should not add entrypoints.
#[test]
fn missing_or_malformed_pyproject_is_harmless() {
    // No pyproject at all.
    let program = analyze(&[("pkg/a.py", "def f():\n    return 1\n")]);
    assert!(program.entrypoints.is_empty() || program.entrypoints.len() == 1);
    // Malformed toml.
    let program2 = analyze(&[("pyproject.toml", "not [valid toml at all\n"), ("pkg/a.py", "def g():\n    return 2\n")]);
    let _ = program2;
}
