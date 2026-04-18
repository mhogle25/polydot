#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use clap::{Parser, Subcommand};

mod commands;
mod config;
mod error;
mod git;
mod link;
mod paths;
mod ui;

#[derive(Parser)]
#[command(
    name = "polydot",
    version,
    about = "Git orchestrator for managing N dotfile repos"
)]
struct Cli {
    /// Enable verbose logging
    #[arg(long, short, global = true)]
    verbose: bool,

    /// Path to config file (default: ~/.config/polydot/config.toml)
    #[arg(long, short, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Clone the config repo, symlink config.toml into place, then sync + link everything else
    Bootstrap {
        /// SSH or HTTPS URL of the polydot-config repo
        url: String,
    },
    /// Clone missing repos. Pull existing repos.
    Sync,
    /// Create or verify symlinks per config.
    Link,
    /// Per-repo summary: clean/dirty, ahead/behind origin, link state.
    Status,
    /// Commit dirty changes + push, across all managed repos.
    Save {
        /// Force shared commit message regardless of default mode
        #[arg(long, short)]
        message: Option<String>,
        /// Force interactive (per-repo prompts) regardless of default mode
        #[arg(long, short)]
        interactive: bool,
    },
    /// Push already-committed work across all repos. No new commits.
    Push,
}

fn main() {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    if let Err(error) = dispatch(cli) {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn init_tracing(verbose: bool) {
    let default_filter = if verbose {
        "polydot=debug"
    } else {
        "polydot=warn"
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

fn dispatch(cli: Cli) -> anyhow::Result<()> {
    let config_path = cli.config;
    match cli.command {
        Command::Bootstrap { url } => commands::bootstrap::run(&url),
        Command::Sync => commands::sync::run(config_path.as_deref()),
        Command::Link => commands::link::run(config_path.as_deref()),
        Command::Status => commands::status::run(config_path.as_deref()),
        Command::Save {
            message,
            interactive,
        } => commands::save::run(config_path.as_deref(), message.as_deref(), interactive),
        Command::Push => commands::push::run(config_path.as_deref()),
    }
}
