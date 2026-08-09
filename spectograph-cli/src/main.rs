//! Spectograph CLI: `check` (read-only analysis) and `fix` (mutating, gated).
//!
//! Vertical slice currently wires `check` for Python end to end. `fix` is stubbed
//! (agent loop + worktree isolation land in a later milestone).

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
