//! Command-line interface definitions.

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
    /// Mutating: refactor the codebase lean in an isolated worktree (stubbed).
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

    /// Language to analyze (e.g. python). If omitted, auto-detect from the root.
    #[arg(long)]
    pub language: Option<String>,

    /// Fail (nonzero exit) only for this tier or stricter: guaranteed, probable, high_cost.
    #[arg(long, default_value = "guaranteed")]
    pub fail_on: String,

    /// Reference-resolution backend: static (default), pyright (type-aware
    /// Python), or tsserver (type-aware TypeScript).
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
