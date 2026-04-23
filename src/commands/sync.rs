// `polydot sync` — clone missing repos; pull (fast-forward only) existing ones.
//
// Per repo:
//   - Clone path missing → `git::clone` it.
//   - Clone path exists  → `git::fetch`, then `git::try_fast_forward`.
//     - Advanced / AlreadyUpToDate → tally and move on.
//     - Diverged → prompt the user: [m]anual / [s]kip / [a]bort.
//
// The "manual" path drops the user into `$SHELL` at the repo. After the
// shell exits, polydot re-fetches and re-tries the fast-forward. If state
// is still diverged, we re-prompt; otherwise we accept the new outcome.
// This loop lets the user keep retrying without re-running `polydot sync`.
//
// Per-repo errors (network, auth, on-disk weirdness) are reported and tallied
// as `failed`; we keep going so a single broken repo doesn't kill the run.
// `[a]bort` short-circuits the outer loop and exits 0 — the user asked to
// stop, not to fail.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use git2::Repository;

use crate::config::{Config, RepoConfig};
use crate::credentials::Credentials;
use crate::git::{self, FastForward};
use crate::paths::{SystemEnv, evaluate};
use crate::ui::{Menu, MenuOption};

const FALLBACK_SHELL: &str = "/bin/sh";

#[derive(Debug, Default)]
struct Summary {
    cloned: usize,
    advanced: usize,
    already_up_to_date: usize,
    resolved: usize,
    skipped: usize,
    failed: usize,
}

impl Summary {
    fn print(&self) {
        println!(
            "{} cloned, {} advanced, {} already up-to-date, {} resolved, {} skipped, {} failed",
            self.cloned,
            self.advanced,
            self.already_up_to_date,
            self.resolved,
            self.skipped,
            self.failed,
        );
    }
}

pub fn run(config: &Config) -> anyhow::Result<()> {
    let creds = Credentials::load_default().context("loading credentials")?;
    run_with(config, &creds, &mut prompt_via_menu, &mut launch_shell)
}

/// Snapshot passed to the divergence prompter: which repo, where it lives,
/// and a human reason. Lets the prompter render a useful header without
/// reaching back into the git layer.
pub(crate) struct SyncPromptCtx<'a> {
    pub name: &'a str,
    pub clone_path: &'a Path,
    pub reason: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncChoice {
    Manual,
    Skip,
    Abort,
}

#[derive(Debug)]
enum RepoOutcome {
    Cloned,
    Advanced,
    UpToDate,
    Resolved,
    Skipped,
    Aborted,
}

/// Test seam: same as [`run`], but the conflict prompter and shell launcher
/// are injected. Production wires them to the interactive menu and a real
/// `$SHELL` spawn; tests pass scripted closures so the prompt + manual-loop
/// paths are exercised without a TTY or real shell.
pub(crate) fn run_with<P, S>(
    config: &Config,
    creds: &Credentials,
    prompter: &mut P,
    shell_launcher: &mut S,
) -> anyhow::Result<()>
where
    P: FnMut(&SyncPromptCtx<'_>) -> anyhow::Result<SyncChoice>,
    S: FnMut(&Path) -> anyhow::Result<()>,
{
    if config.repos.is_empty() {
        println!("(no repos configured)");
        return Ok(());
    }
    let env = SystemEnv;
    let mut summary = Summary::default();
    'outer: for (name, repo_cfg) in &config.repos {
        let result = process_repo(name, repo_cfg, creds, &env, prompter, shell_launcher);
        match result {
            Ok(RepoOutcome::Cloned) => summary.cloned += 1,
            Ok(RepoOutcome::Advanced) => summary.advanced += 1,
            Ok(RepoOutcome::UpToDate) => summary.already_up_to_date += 1,
            Ok(RepoOutcome::Resolved) => summary.resolved += 1,
            Ok(RepoOutcome::Skipped) => summary.skipped += 1,
            Ok(RepoOutcome::Aborted) => {
                summary.skipped += 1;
                break 'outer;
            }
            Err(e) => {
                eprintln!("error syncing `{name}`: {e:#}");
                summary.failed += 1;
            }
        }
    }
    summary.print();
    Ok(())
}

fn process_repo<P, S>(
    name: &str,
    repo_cfg: &RepoConfig,
    creds: &Credentials,
    env: &SystemEnv,
    prompter: &mut P,
    shell_launcher: &mut S,
) -> anyhow::Result<RepoOutcome>
where
    P: FnMut(&SyncPromptCtx<'_>) -> anyhow::Result<SyncChoice>,
    S: FnMut(&Path) -> anyhow::Result<()>,
{
    let clone_path = resolve_clone_path(name, repo_cfg, env)?;
    if !clone_path.exists() {
        if let Some(parent) = clone_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating parent dir for `{name}`"))?;
        }
        println!("cloning  {name}  →  {}", clone_path.display());
        git::clone(&repo_cfg.repo, &clone_path, creds)
            .with_context(|| format!("cloning `{name}`"))?;
        println!();
        return Ok(RepoOutcome::Cloned);
    }
    let repo =
        git::open(&clone_path).with_context(|| format!("opening {}", clone_path.display()))?;
    git::ensure_origin_speakable(&repo, &repo_cfg.repo)?;
    git::fetch(&repo, creds).with_context(|| format!("fetching `{name}`"))?;
    match git::try_fast_forward(&repo)? {
        FastForward::Advanced => {
            println!("pulled   {name}");
            println!();
            Ok(RepoOutcome::Advanced)
        }
        FastForward::AlreadyUpToDate => Ok(RepoOutcome::UpToDate),
        FastForward::Diverged => {
            handle_diverged(name, &clone_path, &repo, creds, prompter, shell_launcher)
        }
    }
}

fn handle_diverged<P, S>(
    name: &str,
    clone_path: &Path,
    repo: &Repository,
    creds: &Credentials,
    prompter: &mut P,
    shell_launcher: &mut S,
) -> anyhow::Result<RepoOutcome>
where
    P: FnMut(&SyncPromptCtx<'_>) -> anyhow::Result<SyncChoice>,
    S: FnMut(&Path) -> anyhow::Result<()>,
{
    loop {
        let ctx = SyncPromptCtx {
            name,
            clone_path,
            reason: "local has commits or dirty changes that don't fast-forward",
        };
        match prompter(&ctx)? {
            SyncChoice::Skip => {
                println!("  skipped  {name}");
                return Ok(RepoOutcome::Skipped);
            }
            SyncChoice::Abort => {
                println!("  aborting sync");
                return Ok(RepoOutcome::Aborted);
            }
            SyncChoice::Manual => {
                shell_launcher(clone_path)
                    .with_context(|| format!("launching shell at {}", clone_path.display()))?;
                git::fetch(repo, creds).with_context(|| format!("re-fetching `{name}`"))?;
                match git::try_fast_forward(repo)? {
                    FastForward::Advanced => {
                        println!("  resolved  {name} (fast-forwarded)");
                        println!();
                        return Ok(RepoOutcome::Resolved);
                    }
                    FastForward::AlreadyUpToDate => {
                        println!("  resolved  {name}");
                        println!();
                        return Ok(RepoOutcome::Resolved);
                    }
                    FastForward::Diverged => {
                        println!("  still diverged after manual step — try again or skip");
                        continue;
                    }
                }
            }
        }
    }
}

fn prompt_via_menu(ctx: &SyncPromptCtx<'_>) -> anyhow::Result<SyncChoice> {
    print_diverged_header(ctx);
    Ok(build_menu().interact()?)
}

fn print_diverged_header(ctx: &SyncPromptCtx<'_>) {
    println!("diverged {}", ctx.name);
    println!("   reason: {}", ctx.reason);
    println!("   repo: {}", ctx.clone_path.display());
    println!();
}

fn build_menu() -> Menu<SyncChoice> {
    let options = vec![
        MenuOption::new(
            'm',
            "[m]anual — drop me into a shell at the repo to resolve",
            SyncChoice::Manual,
        ),
        MenuOption::new(
            's',
            "[s]kip   — leave this repo at its pre-pull state",
            SyncChoice::Skip,
        ),
        MenuOption::new(
            'a',
            "[a]bort  — stop syncing remaining repos",
            SyncChoice::Abort,
        ),
    ];
    Menu::new(options)
        .default_shortcut('s')
        .cancel_shortcut('a')
}

fn launch_shell(path: &Path) -> anyhow::Result<()> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| FALLBACK_SHELL.to_string());
    let status = Command::new(&shell)
        .current_dir(path)
        .status()
        .with_context(|| format!("spawning `{shell}` in {}", path.display()))?;
    if !status.success() {
        eprintln!("(shell exited with {status})");
    }
    Ok(())
}

fn resolve_clone_path(
    name: &str,
    repo_cfg: &RepoConfig,
    env: &SystemEnv,
) -> anyhow::Result<PathBuf> {
    let s = evaluate(&repo_cfg.clone, env)
        .with_context(|| format!("evaluating clone path for `{name}`"))?;
    Ok(PathBuf::from(s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RepoConfig;
    use crate::paths::parse;
    use git2::{BranchType, Repository};
    use std::collections::{BTreeMap, VecDeque};
    use std::fs;
    use tempfile::TempDir;

    fn config_with(repos: Vec<(&str, String, &Path)>) -> Config {
        let mut map = BTreeMap::new();
        for (name, url, clone_path) in repos {
            let clone_expr = parse(&clone_path.display().to_string()).unwrap();
            map.insert(
                name.to_string(),
                RepoConfig {
                    repo: url,
                    clone: clone_expr,
                    links: vec![],
                },
            );
        }
        Config {
            path: None,
            repos: map,
            save: Default::default(),
        }
    }

    fn scripted_prompter(
        choices: Vec<SyncChoice>,
    ) -> impl FnMut(&SyncPromptCtx<'_>) -> anyhow::Result<SyncChoice> {
        let mut queue: VecDeque<SyncChoice> = choices.into();
        move |_ctx| {
            queue
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("scripted prompter exhausted"))
        }
    }

    fn never_called_launcher() -> impl FnMut(&Path) -> anyhow::Result<()> {
        |_path| panic!("shell launcher should not be called")
    }

    fn no_op_launcher() -> impl FnMut(&Path) -> anyhow::Result<()> {
        |_path| Ok(())
    }

    /// Simulates the user resolving by hard-resetting to upstream.
    fn hard_reset_to_upstream_launcher() -> impl FnMut(&Path) -> anyhow::Result<()> {
        |path| {
            let repo = Repository::open(path)?;
            let upstream_oid = repo
                .find_branch("origin/main", BranchType::Remote)?
                .get()
                .target()
                .ok_or_else(|| anyhow::anyhow!("upstream has no target"))?;
            let commit = repo.find_commit(upstream_oid)?;
            repo.reset(commit.as_object(), git2::ResetType::Hard, None)?;
            Ok(())
        }
    }

    /// Bare-remote tempdir; returns the dir handle and a `file://` URL.
    fn bare_remote() -> (TempDir, String) {
        let dir = TempDir::new().unwrap();
        Repository::init_bare(dir.path()).unwrap();
        let url = format!("file://{}", dir.path().display());
        (dir, url)
    }

    /// Clones `url` to a fresh tempdir, configures identity, returns
    /// the dir handle, the path, and the open Repository.
    fn clone_to_tempdir(url: &str) -> (TempDir, PathBuf, Repository) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_path_buf();
        let repo = Repository::clone(url, &path).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "Test").unwrap();
        cfg.set_str("user.email", "test@example.com").unwrap();
        (dir, path, repo)
    }

    fn commit_file(repo: &Repository, name: &str, content: &str, msg: &str) {
        let workdir = repo.workdir().unwrap();
        fs::write(workdir.join(name), content).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(name)).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = repo.signature().unwrap();
        let parents: Vec<_> = repo
            .head()
            .ok()
            .and_then(|h| h.peel_to_commit().ok())
            .into_iter()
            .collect();
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parent_refs)
            .unwrap();
    }

    fn push_main(repo: &Repository) {
        let mut remote = repo.find_remote("origin").unwrap();
        remote
            .push(&["refs/heads/main:refs/heads/main"], None)
            .unwrap();
    }

    fn ensure_main_and_upstream(repo: &Repository) {
        let head_name = repo.head().unwrap().shorthand().unwrap().to_string();
        if head_name != "main" {
            let head_commit = repo
                .find_commit(repo.head().unwrap().target().unwrap())
                .unwrap();
            repo.branch("main", &head_commit, true).unwrap();
            repo.set_head("refs/heads/main").unwrap();
        }
        let mut local = repo.find_branch("main", BranchType::Local).unwrap();
        let _ = local.set_upstream(Some("origin/main"));
    }

    /// Builds a remote with one initial commit on `main`, plus a clone of it.
    fn fixture_remote_and_clone() -> (TempDir, String, TempDir, PathBuf, Repository) {
        let (remote_dir, url) = bare_remote();

        // Seed the remote: clone, commit, push.
        let seed_dir = TempDir::new().unwrap();
        let seed = Repository::init(seed_dir.path()).unwrap();
        let mut cfg = seed.config().unwrap();
        cfg.set_str("user.name", "Test").unwrap();
        cfg.set_str("user.email", "test@example.com").unwrap();
        commit_file(&seed, "README.md", "hi\n", "init");
        let head_name = seed.head().unwrap().shorthand().unwrap().to_string();
        if head_name != "main" {
            let head_commit = seed
                .find_commit(seed.head().unwrap().target().unwrap())
                .unwrap();
            seed.branch("main", &head_commit, true).unwrap();
            seed.set_head("refs/heads/main").unwrap();
        }
        seed.remote("origin", &url).unwrap();
        let mut origin = seed.find_remote("origin").unwrap();
        origin
            .push(&["refs/heads/main:refs/heads/main"], None)
            .unwrap();

        let (clone_dir, clone_path, clone_repo) = clone_to_tempdir(&url);
        ensure_main_and_upstream(&clone_repo);
        (remote_dir, url, clone_dir, clone_path, clone_repo)
    }

    #[test]
    fn clones_missing_repo() {
        let (_remote, url) = bare_remote();
        // Seed remote so the clone has something to fetch.
        let (_seed_dir, _seed_path, seed_repo) = clone_to_tempdir(&url);
        commit_file(&seed_repo, "README.md", "hi\n", "init");
        ensure_main_and_upstream(&seed_repo);
        push_main(&seed_repo);

        let target = TempDir::new().unwrap();
        let dest = target.path().join("fresh");
        let config = config_with(vec![("r", url, &dest)]);

        run_with(
            &config,
            &Credentials::empty(),
            &mut scripted_prompter(vec![]),
            &mut never_called_launcher(),
        )
        .unwrap();

        assert!(dest.join(".git").exists(), "clone should have created .git");
        assert!(dest.join("README.md").exists());
    }

    #[test]
    fn fast_forwards_existing_repo_when_upstream_advanced() {
        let (_remote, url, _b_dir, b_path, _b_repo_drop) = fixture_remote_and_clone();
        // A clones, commits, pushes — upstream now ahead of B.
        let (_a_dir, _a_path, a_repo) = clone_to_tempdir(&url);
        commit_file(&a_repo, "later.txt", "x", "later");
        push_main(&a_repo);

        let config = config_with(vec![("r", url, &b_path)]);
        run_with(
            &config,
            &Credentials::empty(),
            &mut scripted_prompter(vec![]),
            &mut never_called_launcher(),
        )
        .unwrap();

        assert!(
            b_path.join("later.txt").exists(),
            "B should have advanced to upstream"
        );
    }

    #[test]
    fn no_op_when_already_up_to_date() {
        let (_remote, url, _b_dir, b_path, _b_repo) = fixture_remote_and_clone();
        let config = config_with(vec![("r", url, &b_path)]);
        run_with(
            &config,
            &Credentials::empty(),
            &mut scripted_prompter(vec![]),
            &mut never_called_launcher(),
        )
        .unwrap();
        // Nothing to assert beyond "no panic, no prompter call".
    }

    #[test]
    fn skip_choice_leaves_diverged_repo_alone() {
        let (_remote, url, _b_dir, b_path, b_repo) = fixture_remote_and_clone();
        let (_a_dir, _a_path, a_repo) = clone_to_tempdir(&url);
        commit_file(&a_repo, "from-a.txt", "a", "from A");
        push_main(&a_repo);
        commit_file(&b_repo, "from-b.txt", "b", "from B");
        let local_oid_before = b_repo.head().unwrap().target().unwrap();

        let config = config_with(vec![("r", url, &b_path)]);
        run_with(
            &config,
            &Credentials::empty(),
            &mut scripted_prompter(vec![SyncChoice::Skip]),
            &mut never_called_launcher(),
        )
        .unwrap();

        let local_oid_after = b_repo.head().unwrap().target().unwrap();
        assert_eq!(local_oid_before, local_oid_after, "B's HEAD must not move");
        assert!(b_path.join("from-b.txt").exists());
        assert!(!b_path.join("from-a.txt").exists());
    }

    #[test]
    fn abort_short_circuits_remaining_repos() {
        // Two diverged repos. Abort on first → second never processed.
        let (_remote1, url1, _b1_dir, b1_path, b1_repo) = fixture_remote_and_clone();
        let (_a1_dir, _a1_path, a1_repo) = clone_to_tempdir(&url1);
        commit_file(&a1_repo, "a1.txt", "a", "a1");
        push_main(&a1_repo);
        commit_file(&b1_repo, "b1.txt", "b", "b1");

        let (_remote2, url2, _b2_dir, b2_path, b2_repo) = fixture_remote_and_clone();
        let (_a2_dir, _a2_path, a2_repo) = clone_to_tempdir(&url2);
        commit_file(&a2_repo, "a2.txt", "a", "a2");
        push_main(&a2_repo);
        commit_file(&b2_repo, "b2.txt", "b", "b2");
        let b2_head_before = b2_repo.head().unwrap().target().unwrap();

        let config = config_with(vec![("aaa", url1, &b1_path), ("zzz", url2, &b2_path)]);
        run_with(
            &config,
            &Credentials::empty(),
            &mut scripted_prompter(vec![SyncChoice::Abort]),
            &mut never_called_launcher(),
        )
        .unwrap();

        // Second repo's HEAD must not have moved (never processed).
        let b2_head_after = b2_repo.head().unwrap().target().unwrap();
        assert_eq!(b2_head_before, b2_head_after);
        // It also must not have A's commit pulled.
        assert!(!b2_path.join("a2.txt").exists());
    }

    #[test]
    fn manual_then_resolve_via_hard_reset() {
        let (_remote, url, _b_dir, b_path, b_repo) = fixture_remote_and_clone();
        let (_a_dir, _a_path, a_repo) = clone_to_tempdir(&url);
        commit_file(&a_repo, "from-a.txt", "a", "from A");
        push_main(&a_repo);
        commit_file(&b_repo, "from-b.txt", "b", "from B");

        let config = config_with(vec![("r", url, &b_path)]);
        run_with(
            &config,
            &Credentials::empty(),
            &mut scripted_prompter(vec![SyncChoice::Manual]),
            &mut hard_reset_to_upstream_launcher(),
        )
        .unwrap();

        // Hard reset put B at upstream; B's commit is gone.
        assert!(b_path.join("from-a.txt").exists());
        assert!(!b_path.join("from-b.txt").exists());
    }

    #[test]
    fn manual_then_still_diverged_re_prompts_then_skip() {
        let (_remote, url, _b_dir, b_path, b_repo) = fixture_remote_and_clone();
        let (_a_dir, _a_path, a_repo) = clone_to_tempdir(&url);
        commit_file(&a_repo, "from-a.txt", "a", "from A");
        push_main(&a_repo);
        commit_file(&b_repo, "from-b.txt", "b", "from B");

        let config = config_with(vec![("r", url, &b_path)]);
        // First Manual → no-op launcher → still diverged → re-prompt → Skip.
        run_with(
            &config,
            &Credentials::empty(),
            &mut scripted_prompter(vec![SyncChoice::Manual, SyncChoice::Skip]),
            &mut no_op_launcher(),
        )
        .unwrap();

        // B's commit is still present — Skip didn't touch the tree.
        assert!(b_path.join("from-b.txt").exists());
    }

    #[test]
    fn continues_after_per_repo_failure() {
        // First repo: clone path exists but isn't a git repo → open() fails.
        let bogus_dir = TempDir::new().unwrap();
        let bogus_path = bogus_dir.path().to_path_buf();
        fs::write(bogus_path.join("not-a-git-thing"), "x").unwrap();

        // Second repo: healthy, will be advanced.
        let (_remote, url, _b_dir, b_path, _b_repo) = fixture_remote_and_clone();
        let (_a_dir, _a_path, a_repo) = clone_to_tempdir(&url);
        commit_file(&a_repo, "later.txt", "x", "later");
        push_main(&a_repo);

        let config = config_with(vec![
            (
                "aaa-broken",
                "https://example.com/x.git".to_string(),
                &bogus_path,
            ),
            ("zzz-good", url, &b_path),
        ]);
        run_with(
            &config,
            &Credentials::empty(),
            &mut scripted_prompter(vec![]),
            &mut never_called_launcher(),
        )
        .unwrap();

        // Despite the broken first repo, second repo advanced.
        assert!(b_path.join("later.txt").exists());
    }

    #[test]
    fn empty_config_is_a_no_op() {
        let config = config_with(vec![]);
        run_with(
            &config,
            &Credentials::empty(),
            &mut scripted_prompter(vec![]),
            &mut never_called_launcher(),
        )
        .unwrap();
    }
}
