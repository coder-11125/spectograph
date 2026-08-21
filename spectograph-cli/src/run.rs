//! Command execution for the CLI.

use std::path::Path;

use spectograph_analyzer::Analyzer;
use spectograph_core::analysis::{self, BloatTier};
use spectograph_core::cost::CostSource;
use spectograph_core::ir::Language;
use spectograph_core::report::{self, JsonReport};
use spectograph_js::JavaScriptAnalyzer;
use spectograph_py::PythonAnalyzer;
use spectograph_ts::TypeScriptAnalyzer;

use crate::cli::{Cli, Command, FixArgs};

/// Map a language name to an analyzer implementation.
fn analyzer_for(lang: &str) -> Result<Box<dyn Analyzer>, String> {
    match lang {
        "python" | "py" => Ok(Box::new(PythonAnalyzer)),
        "javascript" | "js" => Ok(Box::new(JavaScriptAnalyzer)),
        "typescript" | "ts" => Ok(Box::new(TypeScriptAnalyzer)),
        other => Err(format!(
            "unsupported language '{other}'. Implemented: python, javascript, typescript"
        )),
    }
}

/// Detect a supported language from the root directory by its file extensions.
fn detect_language(root: &Path) -> Option<Language> {
    let candidates = [
        Language::Python,
        Language::JavaScript,
        Language::TypeScript,
        Language::Go,
        Language::Rust,
        Language::Java,
        Language::Kotlin,
        Language::C,
        Language::Cpp,
        Language::Ruby,
    ];
    for lang in candidates {
        if let Ok(files) = spectograph_analyzer::discover::discover_files(root, lang) {
            if !files.is_empty() {
                return Some(lang);
            }
        }
    }
    None
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

#[allow(clippy::too_many_arguments)]
fn run_check(
    root: &Path,
    format: &str,
    language: Option<&str>,
    fail_on: &str,
    resolution: &str,
    pyright_executable: Option<&str>,
    pyright_timeout: u64,
    tsserver_executable: Option<&str>,
    tsserver_timeout: u64,
) -> Result<i32, String> {
    let lang = match language {
        Some(l) => l.to_string(),
        None => detect_language(root)
            .ok_or_else(|| "could not detect a supported language in the root".to_string())?
            .name()
            .to_string(),
    };

    let analyzer = analyzer_for(&lang)?;
    let mut program = if lang.starts_with("python") && resolution == "pyright" {
        // Type-aware resolution via basedpyright.
        let py = PythonAnalyzer;
        py.analyze_with_resolution(
            root,
            spectograph_py::ResolutionMode::Pyright,
            pyright_executable,
            pyright_timeout,
        )
        .map_err(|e| format!("analysis failed: {e}"))?
    } else if lang.starts_with("typescript") && resolution == "tsserver" {
        // Type-aware resolution via typescript-language-server.
        let ts = TypeScriptAnalyzer;
        ts.analyze_with_resolution(
            root,
            spectograph_ts::ResolutionMode::Tsserver,
            tsserver_executable,
            tsserver_timeout,
        )
        .map_err(|e| format!("analysis failed: {e}"))?
    } else {
        if resolution == "pyright" {
            return Err("--resolution pyright is only supported for python currently".to_string());
        }
        if resolution == "tsserver" {
            return Err(
                "--resolution tsserver is only supported for typescript currently".to_string(),
            );
        }
        analyzer
            .analyze(root)
            .map_err(|e| format!("analysis failed: {e}"))?
    };

    // Cost model: static estimate for the vertical slice (no workload yet).
    let model = analyzer.cost_model(&program);
    let _ = CostSource::Estimated;

    let report = analysis::analyze(&mut program, &model);

    match format {
        "json" => {
            let json = JsonReport::from_report(&report);
            println!(
                "{}",
                serde_json::to_string_pretty(&json).map_err(|e| e.to_string())?
            );
        }
        _ => {
            print!("{}", report::render_human(&report, false));
        }
    }

    let min_tier = tier_from_str(fail_on)?;
    let violated = report.findings.iter().any(|f| f.tier >= min_tier);
    Ok(if violated { 1 } else { 0 })
}

fn run_fix(args: FixArgs) -> Result<i32, String> {
    // `fix` is stubbed in this vertical slice. The agent loop + worktree isolation
    // and the smoke/CI gate land in a later milestone (see DESIGN.md §6).
    let _ = args;
    println!(
        "spectograph fix: not yet implemented (agent loop + worktree isolation are a later milestone)."
    );
    Ok(0)
}

pub fn run(cli: Cli) -> Result<std::process::ExitCode, String> {
    let code = match cli.command {
        Command::Check(args) => run_check(
            &args.root,
            &args.format,
            args.language.as_deref(),
            &args.fail_on,
            &args.resolution,
            args.pyright_executable.as_deref(),
            args.pyright_timeout,
            args.tsserver_executable.as_deref(),
            args.tsserver_timeout,
        )?,
        Command::Fix(args) => run_fix(args)?,
    };

    use std::process::ExitCode;
    Ok(ExitCode::from(code as u8))
}
