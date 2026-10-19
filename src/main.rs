mod app;
mod config;
mod diff;
mod gh;
mod git;
mod highlight;
mod mcp;
mod pr;
mod pr_ui;
mod search;
mod search_ui;
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
    /// Open the pull request view (review requests and your PRs across
    /// every logged-in gh account). Works outside a git repository.
    Prs,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Some(Command::Update) = cli.command {
        return update::run_cli();
    }

    let path = cli
        .repo
        .unwrap_or_else(|| std::env::current_dir().expect("cannot resolve cwd"));
    let open_prs = matches!(cli.command, Some(Command::Prs));
    // The PR view doesn't need a local repository; everything else does.
    let repo = match GitRepo::discover(&path) {
        Ok(repo) => Some(repo),
        Err(_) if open_prs => None,
        Err(e) => return Err(e),
    };
    let base_flag = cli.base.is_some();
    let base = cli.base.unwrap_or_else(|| {
        repo.as_ref()
            .map(GitRepo::default_base)
            .unwrap_or_else(|| "main".to_string())
    });
    let config = config::Config::load();

    match (cli.command, repo) {
        (Some(Command::Mcp), Some(repo)) => mcp::run(repo),
        (Some(Command::Mcp | Command::Update), _) => unreachable!(),
        (_, repo) => {
            // An explicit --base always forces branch-vs-base, overriding
            // the configured default mode.
            let mode = if base_flag {
                DiffMode::BranchToBase { base }
            } else {
                config.default_mode.to_diff_mode(&base)
            };
            app::run(repo, mode, config, open_prs)
        }
    }
}
