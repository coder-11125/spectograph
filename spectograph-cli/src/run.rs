//! Command execution for the CLI.

use std::cmp::Reverse;
use std::path::{Component, Path};
use std::process::ExitCode;

use spectograph_analyzer::Analyzer;
use spectograph_core::analysis::{self, BloatTier};
use spectograph_core::diagnostic::Diagnostic;
use spectograph_core::ir::{Language, Program};
use spectograph_core::report::{self, JsonReport};
use spectograph_js::JavaScriptAnalyzer;
use spectograph_py::PythonAnalyzer;
use spectograph_ts::TypeScriptAnalyzer;

use crate::cli::{CheckArgs, Cli, Command, FixArgs};

/// The languages with a working analyzer, in tie-break order (most specific first).
const IMPLEMENTED: [Language; 3] = [Language::TypeScript, Language::JavaScript, Language::Python];

/// Map a `--language` value to a [`Language`].
fn parse_language(name: &str) -> Result<Language, String> {
    match name {
        "python" | "py" => Ok(Language::Python),
        "javascript" | "js" => Ok(Language::JavaScript),
        "typescript" | "ts" => Ok(Language::TypeScript),
        other => Err(format!(
            "unsupported language '{other}'. Implemented: python, javascript, typescript"
        )),
    }
}

fn analyzer_for(lang: Language) -> Box<dyn Analyzer> {
    match lang {
        Language::Python => Box::new(PythonAnalyzer),
        Language::JavaScript => Box::new(JavaScriptAnalyzer),
        Language::TypeScript => Box::new(TypeScriptAnalyzer),
        other => unreachable!("{other:?} has no analyzer; callers only pass IMPLEMENTED languages"),
    }
}

/// True for paths inside dependency or virtual-environment directories, which shouldn't
/// influence which language a project "is".
fn is_vendored(path: &Path) -> bool {
    path.components().any(|c| {
        matches!(
            c,
            Component::Normal(n)
                if n == "node_modules" || n == "site-packages" || n == "venv" || n == "__pycache__"
        )
    })
}

/// Pick the language with the most source files under `root`. The first language found is
/// *not* good enough: a TypeScript app routinely contains a few `.js` config files, and a
/// JavaScript app a Python helper script, and analyzing the wrong one reports nonsense.
fn detect_language(root: &Path) -> Result<Language, String> {
    let mut counts: Vec<(Language, usize)> = Vec::new();
    for lang in IMPLEMENTED {
        let files = spectograph_analyzer::discover::discover_files(root, lang)
            .map_err(|e| format!("could not scan {}: {e}", root.display()))?;
        let n = files.iter().filter(|f| !is_vendored(f)).count();
        if n > 0 {
            counts.push((lang, n));
        }
    }
    // Stable sort keeps IMPLEMENTED's order among equal counts.
    counts.sort_by_key(|&(_, n)| Reverse(n));
    let Some(&(chosen, _)) = counts.first() else {
        return Err(format!(
            "could not detect a supported language (python, javascript, typescript) under {}",
            root.display()
        ));
    };
    if counts.len() > 1 {
        let others: Vec<String> = counts[1..]
            .iter()
            .map(|(l, n)| format!("{n} {} file(s)", l.name()))
            .collect();
        eprintln!(
            "note: analyzing {} (the most files); also found {}. Use --language to choose.",
            chosen.name(),
            others.join(", ")
        );
    }
    Ok(chosen)
}

fn tier_from_str(s: &str) -> Result<BloatTier, String> {
    match s {
        "guaranteed" => Ok(BloatTier::Guaranteed),
        "probable" => Ok(BloatTier::Probable),
        "high_cost" => Ok(BloatTier::HighCost),
        other => Err(format!(
            "unknown tier '{other}'. Valid: guaranteed, probable, high_cost"
        )),
    }
}

/// Run the right analyzer/resolution combination, returning the program plus any
/// framework diagnostics collected in the same pass.
fn analyze_program(lang: Language, args: &CheckArgs) -> Result<(Program, Vec<Diagnostic>), String> {
    let root = args.root.as_path();
    let failed = |e: spectograph_analyzer::AnalyzeError| format!("analysis failed: {e}");

    match (lang, args.resolution.as_str()) {
        (Language::Python, "pyright") => {
            // Type-aware resolution via basedpyright.
            let program = PythonAnalyzer
                .analyze_with_resolution(
                    root,
                    spectograph_py::ResolutionMode::Pyright,
                    args.pyright_executable.as_deref(),
                    args.pyright_timeout,
                )
                .map_err(failed)?;
            Ok((program, Vec::new()))
        }
        (Language::TypeScript, "tsserver") => TypeScriptAnalyzer
            .analyze_full(
                root,
                spectograph_ts::ResolutionMode::Tsserver,
                args.tsserver_executable.as_deref(),
                args.tsserver_timeout,
            )
            .map_err(failed),
        (Language::JavaScript, "tsserver") => JavaScriptAnalyzer
            .analyze_full(
                root,
                spectograph_js::ResolutionMode::Tsserver,
                args.tsserver_executable.as_deref(),
                args.tsserver_timeout,
            )
            .map_err(failed),
        (_, "pyright") => Err("--resolution pyright is only supported for python currently".into()),
        (_, "tsserver") => Err(
            "--resolution tsserver is only supported for typescript and javascript currently"
                .into(),
        ),
        _ => analyzer_for(lang)
            .analyze_with_diagnostics(root)
            .map_err(failed),
    }
}

fn run_check(args: &CheckArgs) -> Result<i32, String> {
    if !args.root.is_dir() {
        return Err(format!("{} is not a directory", args.root.display()));
    }

    let lang = match args.language.as_deref() {
        Some(name) => parse_language(name)?,
        None => detect_language(&args.root)?,
    };

    let (mut program, diagnostics) = analyze_program(lang, args)?;

    // Cost model: static estimate (no runtime workload yet).
    let model = analyzer_for(lang).cost_model(&program);

    let mut report = analysis::analyze(&mut program, &model);
    report.diagnostics = diagnostics;

    match args.format.as_str() {
        "json" => {
            let json = JsonReport::from_report(&report);
            println!(
                "{}",
                serde_json::to_string_pretty(&json).map_err(|e| e.to_string())?
            );
        }
        _ => print!("{}", report::render_human(&report)),
    }

    let min_tier = tier_from_str(&args.fail_on)?;
    let violated = report.findings.iter().any(|f| f.tier <= min_tier)
        || (args.fail_on_diagnostics && !report.diagnostics.is_empty());
    Ok(if violated { 1 } else { 0 })
}

fn run_fix(args: FixArgs) -> Result<i32, String> {
    // `fix` (agent loop + worktree isolation + smoke/CI gate, DESIGN.md §6) isn't built yet.
    // Failing loudly beats exiting 0, which a CI step would read as "fixed".
    let _ = args;
    Err("`spectograph fix` is not implemented yet; use `spectograph check`".to_string())
}

pub fn run(cli: Cli) -> Result<ExitCode, String> {
    let code = match cli.command {
        Command::Check(args) => run_check(&args)?,
        Command::Fix(args) => run_fix(args)?,
    };
    Ok(ExitCode::from(code as u8))
}
