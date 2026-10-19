//! Spectograph CLI: `check` (read-only analysis) and `fix` (mutating, gated).
//!
//! `check` is implemented for Python, JavaScript, and TypeScript. `fix` (agent loop +
//! worktree isolation + smoke/CI gate) is not built yet and exits with an error.

mod cli;
mod run;

use std::process::ExitCode;

use clap::Parser;
use cli::Cli;
use run::run;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}
