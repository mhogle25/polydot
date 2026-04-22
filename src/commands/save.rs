// `polydot save` (Phase 5 pass 1) — commit dirty changes + push, across all
// managed repos, using a single shared commit message supplied via `-m`.
//
// Per repo:
//   - Clone path missing → reported as failure (run `polydot sync` first).
//   - Stage all + commit (skipped if working tree is already clean).
//   - Push to origin.
//     - Pushed → tally and move on.
//     - Rejected (non-fast-forward) → prompt: [m]anual / [s]kip / [a]bort.
//
// "Manual" drops the user into `$SHELL` at the repo. After they exit, we
// re-attempt the push. Still rejected → re-prompt; succeeded → Resolved.
// `[a]bort` short-circuits the outer loop and exits 0.
//
// Pass 2 will add per-repo interactive mode (free-text commit messages and
// a `[r]ebase` option on the divergence prompt). For pass 1, only shared
// mode is supported and `-m` is required.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use git2::Repository;

use crate::config::{Config, RepoConfig};
use crate::credentials::Credentials;
use crate::git::{self, PushOutcome};
use crate::paths::{SystemEnv, evaluate};
use crate::ui::{Menu, MenuOption};

const FALLBACK_SHELL: &str = "/bin/sh";

#[derive(Debug, Default)]
struct Summary {
    committed: usize,
    pushed: usize,
    skipped: usize,
    failed: usize,
}

impl Summary {
    fn print(&self) {
        println!(
            "{} committed, {} pushed, {} skipped, {} failed",
            self.committed, self.pushed, self.skipped, self.failed,
        );
    }
}

pub fn run(config: &Config, message: &str) -> anyhow::Result<()> {
    let creds = Credentials::load_default().context("loading credentials")?;
    run_with(config, message, &creds, &mut prompt_via_menu, &mut launch_shell)
}

/// Snapshot passed to the rejection prompter: which repo, where it lives,
/// the server's reason for rejecting, and whether a fresh local commit is
/// sitting on top of the rejected push (so the prompt can warn the user
/// that aborting/skipping leaves committed-but-unpushed work behind).
pub(crate) struct SavePromptCtx<'a> {
    pub name: &'a str,
    pub clone_path: &'a Path,
    pub reason: &'a str,
    pub committed_locally: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SaveChoice {
    Manual,
    Skip,
    Abort,
}

#[derive(Debug)]
enum RepoOutcome {
    /// Tree was clean and push succeeded (or was a no-op).
    Pushed,
    /// New commit created and push succeeded.
    CommittedAndPushed,
    /// Push rejected, user chose [s]kip. Commit may still be sitting locally.
    Skipped { committed: bool },
    /// User chose [a]bort during the rejection prompt.
    Aborted { committed: bool },
    /// Push rejected, user resolved via [m]anual then re-push succeeded.
    Resolved { committed: bool },
}

/// Test seam: same as [`run`], but the rejection prompter and shell launcher
/// are injected. Production wires them to the interactive menu and a real
/// `$SHELL` spawn; tests pass scripted closures so the prompt + manual-loop
/// paths are exercised without a TTY or real shell.
pub(crate) fn run_with<P, S>(
    config: &Config,
    message: &str,
    creds: &Credentials,
    prompter: &mut P,
    shell_launcher: &mut S,
) -> anyhow::Result<()>
where
    P: FnMut(&SavePromptCtx<'_>) -> anyhow::Result<SaveChoice>,
    S: FnMut(&Path) -> anyhow::Result<()>,
{
    if config.repos.is_empty() {
        println!("(no repos configured)");
        return Ok(());
    }
    let env = SystemEnv;
    let mut summary = Summary::default();
    'outer: for (name, repo_cfg) in &config.repos {
        let result = process_repo(
            name,
            repo_cfg,
            message,
            creds,
            &env,
            prompter,
            shell_launcher,
        );
        match result {
            Ok(RepoOutcome::Pushed) => summary.pushed += 1,
            Ok(RepoOutcome::CommittedAndPushed) => {
                summary.committed += 1;
                summary.pushed += 1;
            }
            Ok(RepoOutcome::Resolved { committed }) => {
                if committed {
                    summary.committed += 1;
                }
                summary.pushed += 1;
            }
            Ok(RepoOutcome::Skipped { committed }) => {
                if committed {
                    summary.committed += 1;
                }
                summary.skipped += 1;
            }
            Ok(RepoOutcome::Aborted { committed }) => {
                if committed {
                    summary.committed += 1;
                }
                summary.skipped += 1;
                break 'outer;
            }
            Err(e) => {
                eprintln!("error saving `{name}`: {e:#}");
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
    message: &str,
    creds: &Credentials,
    env: &SystemEnv,
    prompter: &mut P,
    shell_launcher: &mut S,
) -> anyhow::Result<RepoOutcome>
where
    P: FnMut(&SavePromptCtx<'_>) -> anyhow::Result<SaveChoice>,
    S: FnMut(&Path) -> anyhow::Result<()>,
{
    let clone_path = resolve_clone_path(name, repo_cfg, env)?;
    if !clone_path.exists() {
        anyhow::bail!(
            "clone path {} does not exist — run `polydot sync` first",
            clone_path.display()
        );
    }
    let repo =
        git::open(&clone_path).with_context(|| format!("opening {}", clone_path.display()))?;

    let committed = git::commit_all(&repo, message)
        .with_context(|| format!("committing dirty changes in `{name}`"))?
        .is_some();

    match git::push(&repo, creds).with_context(|| format!("pushing `{name}`"))? {
        PushOutcome::Pushed => {
            if committed {
                println!("committed + pushed  {name}");
                Ok(RepoOutcome::CommittedAndPushed)
            } else {
                println!("pushed              {name}");
                Ok(RepoOutcome::Pushed)
            }
        }
        PushOutcome::Rejected(reason) => handle_rejected(
            name,
            &clone_path,
            &repo,
            creds,
            committed,
            &reason,
            prompter,
            shell_launcher,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_rejected<P, S>(
    name: &str,
    clone_path: &Path,
    repo: &Repository,
    creds: &Credentials,
    committed: bool,
    initial_reason: &str,
    prompter: &mut P,
    shell_launcher: &mut S,
) -> anyhow::Result<RepoOutcome>
where
    P: FnMut(&SavePromptCtx<'_>) -> anyhow::Result<SaveChoice>,
    S: FnMut(&Path) -> anyhow::Result<()>,
{
    let mut current_reason = initial_reason.to_string();
    loop {
        let ctx = SavePromptCtx {
            name,
            clone_path,
            reason: &current_reason,
            committed_locally: committed,
        };
        match prompter(&ctx)? {
            SaveChoice::Skip => {
                println!("  skipped  {name}");
                return Ok(RepoOutcome::Skipped { committed });
            }
            SaveChoice::Abort => {
                println!("  aborting save");
                return Ok(RepoOutcome::Aborted { committed });
            }
            SaveChoice::Manual => {
                shell_launcher(clone_path)
                    .with_context(|| format!("launching shell at {}", clone_path.display()))?;
                match git::push(repo, creds).with_context(|| format!("re-pushing `{name}`"))? {
                    PushOutcome::Pushed => {
                        println!("  resolved  {name} (pushed)");
                        return Ok(RepoOutcome::Resolved { committed });
                    }
                    PushOutcome::Rejected(reason) => {
                        println!("  push still rejected: {reason}");
                        current_reason = reason;
                        continue;
                    }
                }
            }
        }
    }
}

fn prompt_via_menu(ctx: &SavePromptCtx<'_>) -> anyhow::Result<SaveChoice> {
    print_rejected_header(ctx);
    Ok(build_menu().interact()?)
}

fn print_rejected_header(ctx: &SavePromptCtx<'_>) {
    println!();
    println!("=== {} ===", ctx.name);
    println!("Push rejected: {}", ctx.reason);
    println!("  repo: {}", ctx.clone_path.display());
    if ctx.committed_locally {
        println!("  (your changes ARE committed locally; only the push was rejected)");
    }
    println!();
}

fn build_menu() -> Menu<SaveChoice> {
    let options = vec![
        MenuOption::new(
            'm',
            "[m]anual — drop me into a shell at the repo to resolve",
            SaveChoice::Manual,
        ),
        MenuOption::new('s', "[s]kip   — leave this repo as-is", SaveChoice::Skip),
        MenuOption::new(
            'a',
            "[a]bort  — stop saving remaining repos",
            SaveChoice::Abort,
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
        }
    }

    fn scripted_prompter(
        choices: Vec<SaveChoice>,
    ) -> impl FnMut(&SavePromptCtx<'_>) -> anyhow::Result<SaveChoice> {
        let mut queue: VecDeque<SaveChoice> = choices.into();
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

    /// Drops local commits on top of the prior push attempt by hard-resetting
    /// to upstream. Re-push then becomes a no-op success → Resolved.
    fn hard_reset_to_upstream_launcher() -> impl FnMut(&Path) -> anyhow::Result<()> {
        |path| {
            let repo = Repository::open(path)?;
            let mut remote = repo.find_remote("origin")?;
            let refspecs: Vec<String> = remote
                .fetch_refspecs()?
                .iter()
                .filter_map(|s| s.map(String::from))
                .collect();
            remote.fetch(&refspecs, None, None)?;
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

    fn bare_remote() -> (TempDir, String) {
        let dir = TempDir::new().unwrap();
        Repository::init_bare(dir.path()).unwrap();
        let url = format!("file://{}", dir.path().display());
        (dir, url)
    }

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

    /// Returns (remote_dir, url, clone_dir, clone_path, clone_repo) where
    /// the clone has one initial commit on `main` matching the remote.
    fn fixture_remote_and_clone() -> (TempDir, String, TempDir, PathBuf, Repository) {
        let (remote_dir, url) = bare_remote();
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
    fn commits_dirty_changes_then_pushes() {
        let (remote, url, _b_dir, b_path, _b_repo) = fixture_remote_and_clone();
        fs::write(b_path.join("notes.md"), "fresh\n").unwrap();

        let config = config_with(vec![("r", url, &b_path)]);
        run_with(
            &config,
            "add notes",
            &Credentials::empty(),
            &mut scripted_prompter(vec![]),
            &mut never_called_launcher(),
        )
        .unwrap();

        // Verify remote has the new file.
        let (_v_dir, v_path, _v_repo) =
            clone_to_tempdir(&format!("file://{}", remote.path().display()));
        assert!(v_path.join("notes.md").exists());
    }

    #[test]
    fn no_op_when_clean_and_up_to_date() {
        let (_remote, url, _b_dir, b_path, b_repo) = fixture_remote_and_clone();
        let head_before = b_repo.head().unwrap().target().unwrap();

        let config = config_with(vec![("r", url, &b_path)]);
        run_with(
            &config,
            "should not be used",
            &Credentials::empty(),
            &mut scripted_prompter(vec![]),
            &mut never_called_launcher(),
        )
        .unwrap();

        // No new commit was made.
        let head_after = b_repo.head().unwrap().target().unwrap();
        assert_eq!(head_before, head_after);
    }

    #[test]
    fn pushes_pre_existing_commit_when_tree_is_clean() {
        // Simulates: previous run committed but failed to push. Now tree is
        // clean but local is ahead — save should still push.
        let (remote, url, _b_dir, b_path, b_repo) = fixture_remote_and_clone();
        commit_file(&b_repo, "earlier.txt", "x", "earlier commit");
        // Tree clean now; local ahead of upstream by one.

        let config = config_with(vec![("r", url, &b_path)]);
        run_with(
            &config,
            "unused",
            &Credentials::empty(),
            &mut scripted_prompter(vec![]),
            &mut never_called_launcher(),
        )
        .unwrap();

        let (_v_dir, v_path, _v_repo) =
            clone_to_tempdir(&format!("file://{}", remote.path().display()));
        assert!(v_path.join("earlier.txt").exists());
    }

    #[test]
    fn missing_clone_path_is_a_per_repo_failure() {
        let bogus = TempDir::new().unwrap().path().join("never-cloned");
        let (remote, url, _c_dir, c_path, _c_repo) = fixture_remote_and_clone();
        fs::write(c_path.join("c.txt"), "x").unwrap();

        let config = config_with(vec![
            (
                "aaa-missing",
                "https://example.com/x.git".to_string(),
                &bogus,
            ),
            ("zzz-good", url, &c_path),
        ]);
        run_with(
            &config,
            "add c",
            &Credentials::empty(),
            &mut scripted_prompter(vec![]),
            &mut never_called_launcher(),
        )
        .unwrap();

        let (_v_dir, v_path, _v_repo) =
            clone_to_tempdir(&format!("file://{}", remote.path().display()));
        assert!(v_path.join("c.txt").exists());
    }

    #[test]
    fn skip_choice_leaves_committed_changes_local_only() {
        // B clones, A pushes ahead, B has dirty changes — commit succeeds,
        // push rejects → user skips. Commit stays local; remote untouched.
        let (remote, url, _b_dir, b_path, b_repo) = fixture_remote_and_clone();
        let (_a_dir, _a_path, a_repo) = clone_to_tempdir(&url);
        commit_file(&a_repo, "from-a.txt", "a", "from A");
        push_main(&a_repo);
        fs::write(b_path.join("from-b.txt"), "b").unwrap();

        let config = config_with(vec![("r", url, &b_path)]);
        run_with(
            &config,
            "from B",
            &Credentials::empty(),
            &mut scripted_prompter(vec![SaveChoice::Skip]),
            &mut never_called_launcher(),
        )
        .unwrap();

        // B has a new commit locally.
        let head_msg = b_repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .message()
            .unwrap()
            .to_string();
        assert_eq!(head_msg, "from B");

        // Remote does NOT have B's file.
        let (_v_dir, v_path, _v_repo) =
            clone_to_tempdir(&format!("file://{}", remote.path().display()));
        assert!(!v_path.join("from-b.txt").exists());
    }

    #[test]
    fn abort_short_circuits_remaining_repos() {
        let (_remote1, url1, _b1_dir, b1_path, _b1_repo) = fixture_remote_and_clone();
        let (_a1_dir, _a1_path, a1_repo) = clone_to_tempdir(&url1);
        commit_file(&a1_repo, "a1.txt", "a", "a1");
        push_main(&a1_repo);
        fs::write(b1_path.join("b1.txt"), "b").unwrap(); // dirty, will reject

        let (remote2, url2, _b2_dir, b2_path, _b2_repo) = fixture_remote_and_clone();
        fs::write(b2_path.join("b2.txt"), "x").unwrap(); // would push cleanly

        let config = config_with(vec![("aaa", url1, &b1_path), ("zzz", url2, &b2_path)]);
        run_with(
            &config,
            "msg",
            &Credentials::empty(),
            &mut scripted_prompter(vec![SaveChoice::Abort]),
            &mut never_called_launcher(),
        )
        .unwrap();

        // Second repo never ran.
        let (_v_dir, v_path, _v_repo) =
            clone_to_tempdir(&format!("file://{}", remote2.path().display()));
        assert!(!v_path.join("b2.txt").exists());
    }

    #[test]
    fn manual_then_resolve_pushes_after_user_action() {
        let (_remote, url, _b_dir, b_path, _b_repo) = fixture_remote_and_clone();
        let (_a_dir, _a_path, a_repo) = clone_to_tempdir(&url);
        commit_file(&a_repo, "from-a.txt", "a", "from A");
        push_main(&a_repo);
        fs::write(b_path.join("from-b.txt"), "b").unwrap();

        let config = config_with(vec![("r", url, &b_path)]);
        run_with(
            &config,
            "from B",
            &Credentials::empty(),
            &mut scripted_prompter(vec![SaveChoice::Manual]),
            &mut hard_reset_to_upstream_launcher(),
        )
        .unwrap();

        // Hard-reset dropped B's commit AND its working tree — B now matches A.
        assert!(!b_path.join("from-b.txt").exists());
        assert!(b_path.join("from-a.txt").exists());
    }

    #[test]
    fn manual_then_still_rejected_re_prompts_then_skip() {
        let (remote, url, _b_dir, b_path, _b_repo) = fixture_remote_and_clone();
        let (_a_dir, _a_path, a_repo) = clone_to_tempdir(&url);
        commit_file(&a_repo, "from-a.txt", "a", "from A");
        push_main(&a_repo);
        fs::write(b_path.join("from-b.txt"), "b").unwrap();

        let config = config_with(vec![("r", url, &b_path)]);
        run_with(
            &config,
            "from B",
            &Credentials::empty(),
            &mut scripted_prompter(vec![SaveChoice::Manual, SaveChoice::Skip]),
            &mut no_op_launcher(),
        )
        .unwrap();

        // B's commit still local, not on remote.
        let (_v_dir, v_path, _v_repo) =
            clone_to_tempdir(&format!("file://{}", remote.path().display()));
        assert!(!v_path.join("from-b.txt").exists());
    }

    #[test]
    fn empty_config_is_a_no_op() {
        let config = config_with(vec![]);
        run_with(
            &config,
            "msg",
            &Credentials::empty(),
            &mut scripted_prompter(vec![]),
            &mut never_called_launcher(),
        )
        .unwrap();
    }
}
