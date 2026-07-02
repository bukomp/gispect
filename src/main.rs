mod app;
mod diff;
mod git;
mod highlight;
mod mcp;
mod types;
mod ui;
mod update;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::git::GitRepo;
use crate::types::DiffMode;

#[derive(Parser)]
#[command(name = "gispect", version, about = "Inspect git diffs in a side-by-side TUI")]
struct Cli {
    /// Base branch for branch comparison (defaults to main/master).
    #[arg(short, long)]
    base: Option<String>,

    /// Repository path (defaults to the current directory).
    #[arg(short, long)]
    repo: Option<std::path::PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run as an MCP server (JSON-RPC over stdio).
    Mcp,
    /// Check for a newer upstream commit and reinstall if found.
    Update,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Some(Command::Update) = cli.command {
        return update::run_cli();
    }

    let path = cli
        .repo
        .unwrap_or_else(|| std::env::current_dir().expect("cannot resolve cwd"));
    let repo = GitRepo::discover(&path)?;
    let base = cli.base.unwrap_or_else(|| repo.default_base());

    match cli.command {
        Some(Command::Mcp) => mcp::run(repo),
        Some(Command::Update) => unreachable!(),
        None => app::run(repo, DiffMode::BranchToBase { base }),
    }
}
