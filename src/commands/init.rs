// `polydot init` — create a fresh local config.
//
// Drops a commented-out template at the configured path so trial users
// can poke at polydot without first creating a remote `polydot-config`
// repo. Refuses to clobber an existing file.

use std::path::Path;

use anyhow::{Context, bail};

const STUB_CONFIG: &str = r#"# polydot config
#
# Each top-level table is a managed git repo:
#   repo  = remote URL (https/ssh/file — auth inherited from your git config)
#   clone = where it lives on disk (supports ~ and $VAR expansion)
#   links = list of { from = path-in-repo, to = symlink-target }
#
# Add entries via `polydot repo add` and `polydot link add`, or hand-edit:
#
# [nvim-config]
# repo  = "https://github.com/<you>/nvim-config.git"
# clone = "~/dev/config/nvim-config"
# links = [{ from = ".", to = "~/.config/nvim" }]
"#;

pub fn run(config_path: &Path) -> anyhow::Result<()> {
    if config_path.exists() {
        bail!(
            "config already exists at {} — refusing to overwrite",
            config_path.display()
        );
    }
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dir {}", parent.display()))?;
    }
    std::fs::write(config_path, STUB_CONFIG)
        .with_context(|| format!("writing config to {}", config_path.display()))?;
    println!("created {}", config_path.display());
    println!("edit it to add managed repos, then run `polydot sync` and `polydot link`.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use tempfile::TempDir;

    #[test]
    fn creates_loadable_config_at_path() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config/polydot/config.toml");
        run(&path).unwrap();

        assert!(path.exists());
        // Stub must round-trip through the loader cleanly (comment-only TOML).
        let cfg = Config::load(&path).unwrap();
        assert!(cfg.repos.is_empty());
    }

    #[test]
    fn creates_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("a/b/c/config.toml");
        run(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn refuses_to_overwrite_existing_config() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "# pre-existing").unwrap();

        let err = run(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("already exists"), "unexpected error: {msg}");

        // Original contents preserved.
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "# pre-existing");
    }
}
