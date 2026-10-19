//! Command-line interface definitions.
//!
//! Exit codes (also documented in `README.md`): `0` clean, `1` findings at or above
//! `--fail-on` (or diagnostics with `--fail-on-diagnostics`), `2` usage or analysis error.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "spectograph", version, about = "Find where bloat lives.")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Read-only: build the cost/usage model and report bloat.
    Check(CheckArgs),
    /// Mutating: refactor the codebase lean in an isolated worktree (not implemented yet).
    Fix(FixArgs),
}

#[derive(Args, Debug)]
pub struct CheckArgs {
    /// Root directory to analyze. Defaults to the current directory.
    #[arg(default_value = ".")]
    pub root: PathBuf,

    /// Output format.
    #[arg(long, default_value = "human", value_parser = ["human", "json"])]
    pub format: String,

    /// Language to analyze. If omitted, the language with the most source files under the
    /// root is analyzed (and a note is printed to stderr if others are present).
    #[arg(long, value_parser = ["python", "py", "javascript", "js", "typescript", "ts"])]
    pub language: Option<String>,

    /// Fail (nonzero exit) only for this tier or stricter.
    #[arg(long, default_value = "guaranteed", value_parser = ["guaranteed", "probable", "high_cost"])]
    pub fail_on: String,

    /// Also fail (exit 1) when framework diagnostics (React/Angular/Vue prop and binding
    /// mismatches) are reported. Off by default: they are informational.
    #[arg(long)]
    pub fail_on_diagnostics: bool,

    /// Reference-resolution backend: static (default), pyright (type-aware
    /// Python), or tsserver (type-aware TypeScript/JavaScript).
    #[arg(long, default_value = "static", value_parser = ["static", "pyright", "tsserver"])]
    pub resolution: String,

    /// Path to the basedpyright executable (used with --resolution pyright).
    /// Defaults to `basedpyright` on PATH.
    #[arg(long)]
    pub pyright_executable: Option<String>,

    /// Timeout (ms) for the pyright LSP analysis when using --resolution pyright.
    #[arg(long, default_value = "15000")]
    pub pyright_timeout: u64,

    /// Path to the typescript-language-server executable (used with
    /// --resolution tsserver). Defaults to `typescript-language-server` on PATH.
    #[arg(long)]
    pub tsserver_executable: Option<String>,

    /// Timeout (ms) for the tsserver LSP analysis when using --resolution tsserver.
    #[arg(long, default_value = "15000")]
    pub tsserver_timeout: u64,
}

#[derive(Args, Debug)]
pub struct FixArgs {
    /// Root directory to refactor.
    #[arg(default_value = ".")]
    pub root: PathBuf,

    /// Maximum iterations of the check->fix->check loop.
    #[arg(long, default_value = "10")]
    pub max_iterations: u32,

    /// Bloat score considered "lean enough" to stop.
    #[arg(long, default_value = "1.0")]
    pub lean_threshold: f64,
}
