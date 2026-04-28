#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use clap::{Parser, Subcommand};

pub mod commands;
pub mod config;
pub mod error;
pub mod git;
pub mod link;
pub mod paths;
pub mod ui;

use config::Config;

#[derive(Parser)]
#[command(
    name = "polydot",
    version,
    about = "Multi-repo dotfile orchestrator: sync, symlink, and version-control a fleet of separate config repos with one CLI."
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
    /// Create a fresh local config. No remote repo needed — useful for trying polydot.
    Init,
    /// Clone the config repo, symlink config.toml into place, then sync + link everything else
    Bootstrap {
        /// HTTPS or file:// URL of the config repo
        url: String,
        /// Directory to clone the config repo into
        /// (default: $XDG_DATA_HOME/polydot/config — typically ~/.local/share/polydot/config)
        #[arg(long)]
        to: Option<PathBuf>,
    },
    /// Clone missing repos. Pull existing repos.
    Sync,
    /// Create or verify symlinks per config.
    Link,
    /// Per-repo summary: clean/dirty, ahead/behind origin, link state.
    Status,
    /// Commit dirty changes + push, across all managed repos.
    ///
    /// With `-m`: shared mode — one message for all dirty repos.
    /// Without: per-repo mode — prompt per dirty repo.
    Save {
        /// Shared commit message used for every repo with dirty changes
        #[arg(long, short)]
        message: Option<String>,
    },
    /// Commit dirty changes across all repos, without pushing. Mode
    /// selection matches `save`.
    Commit {
        /// Shared commit message used for every repo with dirty changes
        #[arg(long, short)]
        message: Option<String>,
    },
    /// Push already-committed work across all repos. No new commits.
    Push,
}

pub fn run() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match dispatch(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
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
        Command::Init => {
            let path = match config_path {
                Some(p) => p,
                None => default_config_path()?,
            };
            commands::init::run(&path)
        }
        Command::Bootstrap { url, to } => {
            let clone_dest = match to {
                Some(p) => p,
                None => default_bootstrap_dest()?,
            };
            let config_symlink = match config_path {
                Some(p) => p,
                None => default_config_path()?,
            };
            commands::bootstrap::run(&url, &clone_dest, &config_symlink)
        }
        Command::Sync => with_config(config_path, commands::sync::run),
        Command::Link => with_config(config_path, commands::link::run),
        Command::Status => with_config(config_path, commands::status::run),
        Command::Save { message } => {
            with_config(config_path, |c| commands::save::run(c, message.as_deref()))
        }
        Command::Commit { message } => with_config(config_path, |c| {
            commands::commit::run(c, message.as_deref())
        }),
        Command::Push => with_config(config_path, commands::push::run),
    }
}

fn with_config<F>(path: Option<PathBuf>, f: F) -> anyhow::Result<()>
where
    F: FnOnce(&Config) -> anyhow::Result<()>,
{
    let path = match path {
        Some(p) => p,
        None => default_config_path()?,
    };
    let config =
        Config::load(&path).with_context(|| format!("loading config from {}", path.display()))?;
    f(&config)
}

fn default_config_path() -> anyhow::Result<PathBuf> {
    let dir = dirs::config_dir().context("could not determine user config dir")?;
    Ok(dir.join("polydot/config.toml"))
}

fn default_bootstrap_dest() -> anyhow::Result<PathBuf> {
    let dir = dirs::data_dir().context("could not determine user data dir")?;
    Ok(dir.join("polydot/config"))
}
