// `polydot bootstrap` — the new-machine entry point.
//
// Flow:
//   1. Validate the config-repo URL scheme (HTTPS or file://).
//   2. Clone the config repo to `clone_dest` (or verify a pre-existing clone
//      at the same path already has a matching `origin`).
//   3. Create a symlink from `config_symlink` → `<clone_dest>/config.toml`.
//      If something else sits at the symlink path, refuse — the user should
//      decide what to do with it via `polydot link` after bootstrap, not here.
//   4. Load the freshly-linked config and invoke `sync` + `link` in-process.
//
// Bootstrap is the only command that runs without a pre-existing config on
// disk; the `--config` flag (if any) tells us where to place the symlink.

use std::path::Path;

use anyhow::{Context, bail};

use crate::commands::{link as link_cmd, sync};
use crate::config::{self, Config};
use crate::git;
use crate::link;

const CONFIG_FILENAME: &str = "config.toml";

pub fn run(url: &str, clone_dest: &Path, config_symlink: &Path) -> anyhow::Result<()> {
    config::validate_repo_url("bootstrap", url).context("validating config repo URL")?;

    let config_source = clone_dest.join(CONFIG_FILENAME);
    refuse_if_already_bootstrapped(&config_source, config_symlink)?;

    ensure_config_repo_cloned(url, clone_dest)?;

    if !config_source.exists() {
        bail!(
            "cloned config repo has no {CONFIG_FILENAME} at {}",
            config_source.display()
        );
    }
    ensure_config_symlink(&config_source, config_symlink)?;

    let cfg = Config::load(config_symlink)
        .with_context(|| format!("loading config from {}", config_symlink.display()))?;
    println!();
    println!("syncing managed repos");
    sync::run(&cfg).context("running sync")?;
    println!();
    println!("linking managed files");
    link_cmd::run(&cfg).context("running link")?;

    Ok(())
}

/// Bootstrap is the new-machine entry point — not an idempotent re-runner.
/// If the config symlink already points at the expected source, tell the
/// user to reach for `sync` / `link` directly instead.
fn refuse_if_already_bootstrapped(
    expected_source: &Path,
    config_symlink: &Path,
) -> anyhow::Result<()> {
    if let link::LinkState::Correct =
        link::link_state(expected_source, config_symlink).context("inspecting config symlink")?
    {
        bail!(
            "already bootstrapped: {} is already linked to {}. \
             Use `polydot sync && polydot link` to refresh.",
            config_symlink.display(),
            expected_source.display(),
        );
    }
    Ok(())
}

fn ensure_config_repo_cloned(url: &str, clone_dest: &Path) -> anyhow::Result<()> {
    if clone_dest.exists() {
        let repo = git::open(clone_dest).with_context(|| {
            format!(
                "clone destination exists at {} but is not a git repo — \
                 move or delete it, then re-run bootstrap",
                clone_dest.display()
            )
        })?;
        let origin = repo
            .find_remote("origin")
            .context("reading origin remote")?;
        let actual = origin
            .url()
            .ok_or_else(|| anyhow::anyhow!("origin has no URL configured"))?;
        if actual != url {
            bail!(
                "existing clone at {} has origin `{actual}`, expected `{url}` — \
                 move or delete it, then re-run bootstrap",
                clone_dest.display(),
            );
        }
        println!(
            "cloned   config  →  {} (already present)",
            clone_dest.display()
        );
        return Ok(());
    }

    if let Some(parent) = clone_dest.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "creating parent dir for clone destination {}",
                parent.display()
            )
        })?;
    }
    println!("cloning  config  →  {}", clone_dest.display());
    git::clone(url, clone_dest).context("cloning config repo")?;
    Ok(())
}

fn ensure_config_symlink(source: &Path, link_path: &Path) -> anyhow::Result<()> {
    match link::link_state(source, link_path).context("inspecting config symlink path")? {
        link::LinkState::Correct => {
            // `refuse_if_already_bootstrapped` runs first, so reaching this
            // arm means the symlink was created mid-flight (shouldn't happen
            // in practice). Treat as a no-op rather than re-failing.
            Ok(())
        }
        link::LinkState::Missing => {
            link::create(source, link_path)
                .with_context(|| format!("creating symlink at {}", link_path.display()))?;
            println!("linked   config  →  {}", link_path.display());
            Ok(())
        }
        link::LinkState::WrongTarget { actual } => bail!(
            "config symlink at {} points at {} — expected {}. \
             Remove or fix the existing symlink and re-run bootstrap.",
            link_path.display(),
            actual.display(),
            source.display(),
        ),
        link::LinkState::BrokenSource { source: dangling } => bail!(
            "config symlink at {} points at {} but that file does not exist. \
             Remove the stale symlink and re-run bootstrap.",
            link_path.display(),
            dangling.display(),
        ),
        link::LinkState::UnmanagedConflict => bail!(
            "{} already exists and is not a symlink — move or delete it, then re-run bootstrap",
            link_path.display(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::Repository;
    use std::fs;
    use tempfile::TempDir;

    /// Create a bare remote seeded with a minimal config.toml that declares
    /// zero managed repos. Returns `(tempdir-handle, file:// URL)`.
    fn bare_remote_with_config(config_contents: &str) -> (TempDir, String) {
        let remote_dir = TempDir::new().unwrap();
        crate::git::test_support::init_bare(remote_dir.path());
        let url = format!("file://{}", remote_dir.path().display());

        // Seed the remote by cloning into a tempdir, committing, pushing.
        let seed_dir = TempDir::new().unwrap();
        let repo = Repository::clone(&url, seed_dir.path()).unwrap();
        {
            let mut cfg = repo.config().unwrap();
            cfg.set_str("user.name", "Test").unwrap();
            cfg.set_str("user.email", "test@example.com").unwrap();
        }
        fs::write(seed_dir.path().join(CONFIG_FILENAME), config_contents).unwrap();

        let mut index = repo.index().unwrap();
        index
            .add_path(std::path::Path::new(CONFIG_FILENAME))
            .unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = repo.signature().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "seed", &tree, &[])
            .unwrap();

        // Ensure branch is `main` and push.
        let head = repo.head().unwrap();
        let head_name = head.shorthand().unwrap().to_string();
        if head_name != "main" {
            let head_commit = repo.find_commit(head.target().unwrap()).unwrap();
            repo.branch("main", &head_commit, true).unwrap();
            repo.set_head("refs/heads/main").unwrap();
        }
        let mut origin = repo.find_remote("origin").unwrap();
        origin
            .push(&["refs/heads/main:refs/heads/main"], None)
            .unwrap();

        (remote_dir, url)
    }

    const EMPTY_CONFIG: &str = "";

    #[test]
    fn clones_and_symlinks_on_fresh_machine() {
        let (_remote, url) = bare_remote_with_config(EMPTY_CONFIG);
        let workdir = TempDir::new().unwrap();
        let clone_dest = workdir.path().join("share/polydot/config");
        let config_symlink = workdir.path().join("config/polydot/config.toml");

        run(&url, &clone_dest, &config_symlink).unwrap();

        assert!(clone_dest.join(".git").exists());
        assert!(clone_dest.join(CONFIG_FILENAME).exists());
        let meta = fs::symlink_metadata(&config_symlink).unwrap();
        assert!(meta.file_type().is_symlink());
        assert_eq!(
            fs::canonicalize(&config_symlink).unwrap(),
            fs::canonicalize(clone_dest.join(CONFIG_FILENAME)).unwrap(),
        );
    }

    #[test]
    fn refuses_rerun_when_already_bootstrapped() {
        let (_remote, url) = bare_remote_with_config(EMPTY_CONFIG);
        let workdir = TempDir::new().unwrap();
        let clone_dest = workdir.path().join("share/polydot/config");
        let config_symlink = workdir.path().join("config/polydot/config.toml");

        run(&url, &clone_dest, &config_symlink).unwrap();
        let err = run(&url, &clone_dest, &config_symlink).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("already bootstrapped"),
            "unexpected error: {msg}"
        );
        assert!(
            msg.contains("sync") && msg.contains("link"),
            "error should point user at sync and link: {msg}"
        );
    }

    #[test]
    fn rejects_non_git_clone_dest() {
        let (_remote, url) = bare_remote_with_config(EMPTY_CONFIG);
        let workdir = TempDir::new().unwrap();
        let clone_dest = workdir.path().join("clone");
        fs::create_dir_all(&clone_dest).unwrap();
        fs::write(clone_dest.join("stray"), "").unwrap();
        let config_symlink = workdir.path().join("config.toml");

        let err = run(&url, &clone_dest, &config_symlink).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("not a git repo"), "unexpected error: {msg}");
    }

    #[test]
    fn rejects_existing_clone_with_mismatched_origin() {
        let (_remote_a, url_a) = bare_remote_with_config(EMPTY_CONFIG);
        let (_remote_b, url_b) = bare_remote_with_config(EMPTY_CONFIG);
        let workdir = TempDir::new().unwrap();
        let clone_dest = workdir.path().join("clone");
        Repository::clone(&url_a, &clone_dest).unwrap();
        let config_symlink = workdir.path().join("config.toml");

        let err = run(&url_b, &clone_dest, &config_symlink).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("has origin"), "unexpected error: {msg}");
        assert!(msg.contains(&url_a), "error should mention actual origin");
    }

    #[test]
    fn rejects_symlink_pointing_elsewhere() {
        let (_remote, url) = bare_remote_with_config(EMPTY_CONFIG);
        let workdir = TempDir::new().unwrap();
        let clone_dest = workdir.path().join("clone");
        let config_symlink = workdir.path().join("config.toml");
        let decoy = workdir.path().join("decoy.toml");
        fs::write(&decoy, "").unwrap();
        std::os::unix::fs::symlink(&decoy, &config_symlink).unwrap();

        let err = run(&url, &clone_dest, &config_symlink).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("points at"), "unexpected error: {msg}");
    }

    #[test]
    fn rejects_plain_file_at_symlink_path() {
        let (_remote, url) = bare_remote_with_config(EMPTY_CONFIG);
        let workdir = TempDir::new().unwrap();
        let clone_dest = workdir.path().join("clone");
        let config_symlink = workdir.path().join("config.toml");
        fs::create_dir_all(config_symlink.parent().unwrap()).unwrap();
        fs::write(&config_symlink, "pre-existing").unwrap();

        let err = run(&url, &clone_dest, &config_symlink).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("not a symlink"), "unexpected error: {msg}");
    }

    #[test]
    fn invokes_sync_and_link_after_bootstrap() {
        // Config with one managed repo; bootstrap should clone it via sync
        // and then (trivially) run link — no symlinks configured, so link
        // is a no-op but must not error.
        let (_managed_remote, managed_url) = bare_remote_with_config("");
        // Use a placeholder in the config, fill it in below.
        let workdir = TempDir::new().unwrap();
        let managed_clone = workdir.path().join("managed-clone");
        let config_contents = format!(
            "[managed]\nrepo = \"{}\"\nclone = \"{}\"\n",
            managed_url,
            managed_clone.display(),
        );

        let (_config_remote, config_url) = bare_remote_with_config(&config_contents);
        let clone_dest = workdir.path().join("share/polydot/config");
        let config_symlink = workdir.path().join("config/polydot/config.toml");

        run(&config_url, &clone_dest, &config_symlink).unwrap();

        assert!(clone_dest.join(CONFIG_FILENAME).exists());
        assert!(
            managed_clone.join(".git").exists(),
            "sync should have cloned the managed repo at {}",
            managed_clone.display()
        );
    }
}
