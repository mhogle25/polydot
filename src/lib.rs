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
pub mod config_edit;
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
    /// Create or verify symlinks per config (no subcommand), or edit link entries.
    Link {
        #[command(subcommand)]
        action: Option<LinkAction>,
    },
    /// Edit managed-repo entries in config.
    Repo {
        #[command(subcommand)]
        action: RepoAction,
    },
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

#[derive(Subcommand)]
enum LinkAction {
    /// Add a link entry to a managed repo.
    Add {
        /// Name of the managed repo (must exist in config).
        repo: String,
        /// Path within the repo (relative to its clone path).
        from: String,
        /// Symlink target on disk. Supports `~` and `$VAR` expansion.
        to: String,
        /// Move the file currently at `to` into the repo at `from`, then symlink.
        #[arg(long)]
        adopt: bool,
    },
    /// Remove a link entry. The on-disk symlink is left alone — use
    /// `polydot link` afterward if you want it cleaned up.
    Rm {
        /// Name of the managed repo.
        repo: String,
        /// Path within the repo (the link's `from` field).
        from: String,
    },
    /// List configured links (all repos, or one).
    List {
        /// Optional repo filter.
        repo: Option<String>,
    },
}

#[derive(Subcommand)]
enum RepoAction {
    /// Add a managed repo entry.
    Add {
        /// Short name for the repo (the TOML table name).
        name: String,
        /// Remote URL (https, ssh, git@..., or file://).
        #[arg(long)]
        repo: String,
        /// Local clone path. Supports `~` and `$VAR` expansion.
        #[arg(long)]
        clone: String,
    },
    /// Remove a repo entry (and all its links). Files on disk are
    /// untouched.
    Rm {
        /// Repo name.
        name: String,
    },
    /// List configured repos.
    List,
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
        Command::Link { action: None } => with_config(config_path, commands::link::run),
        Command::Link {
            action:
                Some(LinkAction::Add {
                    repo,
                    from,
                    to,
                    adopt,
                }),
        } => {
            let path = config_path_or_default(config_path)?;
            commands::link::add(&path, &repo, &from, &to, adopt)
        }
        Command::Link {
            action: Some(LinkAction::Rm { repo, from }),
        } => {
            let path = config_path_or_default(config_path)?;
            commands::link::rm(&path, &repo, &from)
        }
        Command::Link {
            action: Some(LinkAction::List { repo }),
        } => with_config(config_path, |c| commands::link::list(c, repo.as_deref())),
        Command::Repo {
            action: RepoAction::Add { name, repo, clone },
        } => {
            let path = config_path_or_default(config_path)?;
            commands::repo::add(&path, &name, &repo, &clone)
        }
        Command::Repo {
            action: RepoAction::Rm { name },
        } => {
            let path = config_path_or_default(config_path)?;
            commands::repo::rm(&path, &name)
        }
        Command::Repo {
            action: RepoAction::List,
        } => with_config(config_path, commands::repo::list),
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
    let path = config_path_or_default(path)?;
    let config =
        Config::load(&path).with_context(|| format!("loading config from {}", path.display()))?;
    f(&config)
}

fn config_path_or_default(path: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    match path {
        Some(p) => Ok(p),
        None => default_config_path(),
    }
}

fn default_config_path() -> anyhow::Result<PathBuf> {
    let dir = dirs::config_dir().context("could not determine user config dir")?;
    Ok(dir.join("polydot/config.toml"))
}

fn default_bootstrap_dest() -> anyhow::Result<PathBuf> {
    let dir = dirs::data_dir().context("could not determine user data dir")?;
    Ok(dir.join("polydot/config"))
}
