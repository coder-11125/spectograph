//! Automated tests that run the full analyzer against the committed fixtures in
//! `fixtures/`. Each fixture asserts which symbols must be reachable (not bloat) and
//! which must be flagged as dead. This keeps the demo fixtures load-bearing.

use std::path::PathBuf;

use spectograph_analyzer::Analyzer;
use spectograph_core::analysis;
use spectograph_core::cost::CostModel;
use spectograph_py::PythonAnalyzer;

/// Path to the fixtures dir relative to the crate root.
fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures")
}

fn analyze(name: &str) -> spectograph_core::ir::Program {
    let dir = fixtures_dir().join(name);
    PythonAnalyzer.analyze(&dir).unwrap_or_else(|e| panic!("analyze {name}: {e}"))
}

fn report(program: &mut spectograph_core::ir::Program) -> analysis::Report {
    let model = CostModel::build(program, spectograph_core::cost::CostSource::Estimated);
    analysis::analyze(program, &model)
}

fn is_flagged(report: &analysis::Report, name: &str) -> bool {
    report.findings.iter().any(|f| f.name == name)
}

/// python-classes: methods resolve via self.method() and obj.method(); constructor
/// __init__ is used; unused_method is dead.
#[test]
fn python_classes_fixture() {
    let mut program = analyze("python-classes");
    let report = report(&mut program);

    // Used methods + constructor.
    assert!(!is_flagged(&report, "greet"), "greet called via g.greet()");
    assert!(!is_flagged(&report, "_make_greeting"), "reached via self._make_greeting()");
    assert!(!is_flagged(&report, "__init__"), "constructor used when class instantiated");

    // Dead method.
    assert!(is_flagged(&report, "unused_method"), "unused_method is never called");
}

/// python-inheritance: Child subclasses Base; super().greet() reaches Base.greet;
/// Base is reachable via subclassing; dead methods in both are flagged.
#[test]
fn python_inheritance_fixture() {
    let mut program = analyze("python-inheritance");
    let report = report(&mut program);

    // Reachable via inheritance and super().
    assert!(!is_flagged(&report, "Base"), "Base is used via subclassing");
    assert!(!is_flagged(&report, "greet"), "both greet methods are used");

    // Dead methods.
    assert!(is_flagged(&report, "base_only"), "base_only is never called");
    assert!(is_flagged(&report, "unused_child_method"), "unused_child_method is never called");

    // super() must reach the base class's greet method: both greet methods (base and
    // child) are reachable.
    let reachable_greets = program
        .symbols
        .iter()
        .filter(|s| s.name == "greet" && s.reachable)
        .count();
    assert!(reachable_greets >= 2, "both base and child greet should be reachable");
}

/// python-relative: relative imports resolve to the correct module. core.helper is
/// reachable via `from .core import helper`; other.helper (same name, different module)
/// stays dead.
#[test]
fn python_relative_fixture() {
    let mut program = analyze("python-relative");
    let report = report(&mut program);

    // core.helper is imported and called -> reachable.
    let core_helper = program
        .symbols
        .iter()
        .find(|s| s.name == "helper" && s.file.to_string_lossy().ends_with("core.py"))
        .expect("core helper");
    assert!(core_helper.reachable, "core helper should be reachable via relative import");

    // Dead symbols.
    assert!(is_flagged(&report, "dead_core"), "dead_core is unused");
    assert!(is_flagged(&report, "other"), "other/other is unused");

    // There is exactly one `helper` finding: other.py's (core's is reachable).
    let helper_findings: Vec<_> = report.findings.iter().filter(|f| f.name == "helper").collect();
    assert_eq!(helper_findings.len(), 1, "only other.helper should be flagged");
    assert!(
        helper_findings[0].file.contains("other.py"),
        "flagged helper should be in other.py, not core.py"
    );
}

/// python-sample: helper is used; dead_function/UnusedClass/UsedClass are dead; the
/// dead imports stay dead.
#[test]
fn python_sample_fixture() {
    let mut program = analyze("python-sample");
    let report = report(&mut program);

    // Reachable/used.
    assert!(!is_flagged(&report, "helper"), "helper is called from main");
    assert!(!is_flagged(&report, "main"), "main is reachable via __main__");

    // Dead code.
    assert!(is_flagged(&report, "dead_function"), "dead_function is unused");
    assert!(is_flagged(&report, "UnusedClass"), "UnusedClass is unused");
    assert!(is_flagged(&report, "UsedClass"), "UsedClass is unused");
    assert!(
        is_flagged(&report, "unused_thing"),
        "unused_thing is only imported by a dead import"
    );
}

/// python-pkg: add/to_upper/double used via package; __all__ protects members;
/// dead_string_fn is dead.
#[test]
fn python_pkg_fixture() {
    let mut program = analyze("python-pkg");
    let report = report(&mut program);

    // Public/used via package re-exports.
    assert!(!is_flagged(&report, "add"), "add used via package");
    assert!(!is_flagged(&report, "to_upper"), "to_upper used via package");
    assert!(!is_flagged(&report, "double"), "double used via package");

    // __all__ protects public members even if not otherwise used.
    assert!(!is_flagged(&report, "unused_helper"), "in __all__, public API");
    assert!(!is_flagged(&report, "to_lower"), "in __all__, public API");

    // Dead.
    assert!(is_flagged(&report, "dead_string_fn"), "dead_string_fn is unused");
}

/// python-scripts: pyproject console scripts mark run/main as entrypoints; dead code
/// in the same modules is flagged.
#[test]
fn python_scripts_fixture() {
    let mut program = analyze("python-scripts");
    let report = report(&mut program);

    // Entrypoints from pyproject.toml.
    assert!(!is_flagged(&report, "run"), "run is a [project.scripts] entrypoint");
    assert!(!is_flagged(&report, "main"), "main is a [project.gui-scripts] entrypoint");

    // Reachable through entrypoints.
    assert!(!is_flagged(&report, "do_work"), "do_work reached via run");

    // Dead.
    assert!(is_flagged(&report, "unused_internal"), "unused_internal is unused");
    assert!(is_flagged(&report, "dead_helper"), "dead_helper is unused");
    assert!(
        is_flagged(&report, "unreferenced_module_fn"),
        "orphan module function is unused"
    );
}
