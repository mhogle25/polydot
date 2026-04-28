// `polydot repo add/rm/list` — config-level CRUD for managed repos.

use std::path::Path;

use crate::config::Config;
use crate::config_edit::{self, AddOutcome};

pub fn add(config_path: &Path, name: &str, repo_url: &str, clone: &str) -> anyhow::Result<()> {
    match config_edit::add_repo(config_path, name, repo_url, clone)? {
        AddOutcome::Added => println!("added [{name}]"),
        AddOutcome::AlreadyExists => println!("[{name}] already configured (no change)"),
    }
    Ok(())
}

pub fn rm(config_path: &Path, name: &str) -> anyhow::Result<()> {
    config_edit::remove_repo(config_path, name)?;
    println!("removed [{name}]");
    Ok(())
}

pub fn list(config: &Config) -> anyhow::Result<()> {
    if config.repos.is_empty() {
        println!("(no repos configured)");
        return Ok(());
    }
    for (name, repo) in &config.repos {
        println!("{name}");
        println!("  repo  {}", repo.repo);
        println!("  clone {}", repo.clone);
    }
    Ok(())
}
