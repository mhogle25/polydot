// `polydot push` — push already-committed work across all managed repos.
//
// Per repo:
//   - Clone path missing → reported as failure (run `polydot sync` first).
//   - Otherwise attempt `git::push`.
//     - Pushed → tally and move on.
//     - Rejected (non-fast-forward) → prompt: [r]ebase / [m]anual / [s]kip / [a]bort.
//
// On rejection we best-effort fetch so the prompt header can report accurate
// ahead/behind counts, then:
//   - [r]ebase replays local commits onto upstream via libgit2. Clean finish
//     → re-push. Conflict → libgit2 aborts the rebase (pre-rebase state is
//     restored), and we re-prompt so the user can try [m]anual.
//   - [m]anual drops the user into `$SHELL` at the repo. After they exit we
//     re-attempt the push. Still rejected → re-prompt; succeeded → Resolved.
//   - [a]bort short-circuits the outer loop and exits 0.
//
// This command does NOT commit anything — it ships only what's already
// committed. For commit + push in one shot, use `polydot save`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use git2::Repository;

use crate::config::{Config, RepoConfig};
use crate::credentials::Credentials;
use crate::git::{self, PushOutcome, RebaseOutcome};
use crate::paths::{SystemEnv, evaluate};
use crate::ui::{Menu, MenuOption};

const FALLBACK_SHELL: &str = "/bin/sh";

#[derive(Debug, Default)]
struct Summary {
    pushed: usize,
    resolved: usize,
    up_to_date: usize,
    skipped: usize,
    failed: usize,
}

impl Summary {
    fn print(&self) {
        println!(
            "{} pushed, {} resolved, {} up-to-date, {} skipped, {} failed",
            self.pushed, self.resolved, self.up_to_date, self.skipped, self.failed,
        );
    }
}

pub fn run(config: &Config) -> anyhow::Result<()> {
    let creds = Credentials::load_default().context("loading credentials")?;
    run_with(config, &creds, &mut prompt_via_menu, &mut launch_shell)
}

/// Snapshot passed to the rejection prompter: which repo, where it lives,
/// the server's reason for rejecting, and — when a best-effort fetch succeeded
/// — how many commits local and upstream are apart. Lets the prompter render
/// a useful header without reaching back into the git layer.
pub(crate) struct PushPromptCtx<'a> {
    pub name: &'a str,
    pub clone_path: &'a Path,
    pub reason: &'a str,
    pub ahead: Option<usize>,
    pub behind: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PushChoice {
    Rebase,
    Manual,
    Skip,
    Abort,
}

#[derive(Debug)]
enum RepoOutcome {
    Pushed,
    Resolved,
    UpToDate,
    Skipped,
    Aborted,
}

/// Test seam: same as [`run`], but the rejection prompter and shell launcher
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
    P: FnMut(&PushPromptCtx<'_>) -> anyhow::Result<PushChoice>,
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
            Ok(RepoOutcome::Pushed) => summary.pushed += 1,
            Ok(RepoOutcome::Resolved) => summary.resolved += 1,
            Ok(RepoOutcome::UpToDate) => summary.up_to_date += 1,
            Ok(RepoOutcome::Skipped) => summary.skipped += 1,
            Ok(RepoOutcome::Aborted) => {
                summary.skipped += 1;
                break 'outer;
            }
            Err(e) => {
                eprintln!("error pushing `{name}`: {e:#}");
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
    P: FnMut(&PushPromptCtx<'_>) -> anyhow::Result<PushChoice>,
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
    git::ensure_origin_speakable(&repo, &repo_cfg.repo)?;
    if !has_unpushed_commits(&repo) {
        println!("up-to-date  {name}");
        return Ok(RepoOutcome::UpToDate);
    }
    match git::push(&repo, creds).with_context(|| format!("pushing `{name}`"))? {
        PushOutcome::Pushed => {
            println!("pushed      {name}");
            println!();
            Ok(RepoOutcome::Pushed)
        }
        PushOutcome::Rejected(reason) => handle_rejected(
            name,
            &clone_path,
            &repo,
            creds,
            &reason,
            prompter,
            shell_launcher,
        ),
    }
}

/// True if local has commits the upstream lacks. If upstream tracking info
/// is missing (no upstream, or `git::status` errored), err on the side of
/// attempting the push — a no-op is harmless, a missed push is not.
fn has_unpushed_commits(repo: &Repository) -> bool {
    match git::status(repo) {
        Ok(s) => match s.ahead_behind {
            Some((ahead, _)) => ahead > 0,
            None => true,
        },
        Err(_) => true,
    }
}

fn handle_rejected<P, S>(
    name: &str,
    clone_path: &Path,
    repo: &Repository,
    creds: &Credentials,
    initial_reason: &str,
    prompter: &mut P,
    shell_launcher: &mut S,
) -> anyhow::Result<RepoOutcome>
where
    P: FnMut(&PushPromptCtx<'_>) -> anyhow::Result<PushChoice>,
    S: FnMut(&Path) -> anyhow::Result<()>,
{
    // Best-effort fetch so the prompt header has accurate divergence counts.
    // A fetch failure here is non-fatal — the prompt simply omits the numbers.
    let _ = git::fetch(repo, creds);
    let mut current_reason = initial_reason.to_string();
    loop {
        let (ahead, behind) = divergence(repo);
        let ctx = PushPromptCtx {
            name,
            clone_path,
            reason: &current_reason,
            ahead,
            behind,
        };
        match prompter(&ctx)? {
            PushChoice::Skip => {
                println!("  skipped  {name}");
                return Ok(RepoOutcome::Skipped);
            }
            PushChoice::Abort => {
                println!("  aborting push");
                return Ok(RepoOutcome::Aborted);
            }
            PushChoice::Rebase => match run_rebase_then_push(name, repo, creds)? {
                RebaseStep::Resolved => return Ok(RepoOutcome::Resolved),
                RebaseStep::StillRejected(reason) => {
                    current_reason = reason;
                    let _ = git::fetch(repo, creds);
                    continue;
                }
                RebaseStep::Retry => continue,
            },
            PushChoice::Manual => {
                shell_launcher(clone_path)
                    .with_context(|| format!("launching shell at {}", clone_path.display()))?;
                match git::push(repo, creds).with_context(|| format!("re-pushing `{name}`"))? {
                    PushOutcome::Pushed => {
                        println!("  resolved  {name} (pushed)");
                        println!();
                        return Ok(RepoOutcome::Resolved);
                    }
                    PushOutcome::Rejected(reason) => {
                        println!("  push still rejected: {reason}");
                        current_reason = reason;
                        let _ = git::fetch(repo, creds);
                        continue;
                    }
                }
            }
        }
    }
}

/// Local-only ahead/behind relative to the configured upstream, or `(None, None)`
/// if it can't be determined (no upstream, detached HEAD, etc.).
fn divergence(repo: &Repository) -> (Option<usize>, Option<usize>) {
    match git::status(repo) {
        Ok(s) => match s.ahead_behind {
            Some((a, b)) => (Some(a), Some(b)),
            None => (None, None),
        },
        Err(_) => (None, None),
    }
}

/// Outcome of a [r]ebase-then-retry step. Decoupled from `RepoOutcome` so the
/// caller stays in control of the prompt loop.
enum RebaseStep {
    /// Rebase applied cleanly and the follow-up push succeeded.
    Resolved,
    /// Rebase applied but push still rejected. Caller re-prompts.
    StillRejected(String),
    /// Rebase didn't apply (conflicts aborted it, or nothing to do, or it
    /// errored out). Repo is in the same shape as before the attempt. Caller
    /// re-prompts so the user can pick another action.
    Retry,
}

fn run_rebase_then_push(
    name: &str,
    repo: &Repository,
    creds: &Credentials,
) -> anyhow::Result<RebaseStep> {
    match git::rebase_onto_upstream(repo).with_context(|| format!("rebasing `{name}`")) {
        Ok(RebaseOutcome::Completed) => {
            println!("  rebased  {name}");
            match git::push(repo, creds).with_context(|| format!("re-pushing `{name}`"))? {
                PushOutcome::Pushed => {
                    println!("  resolved  {name} (rebased + pushed)");
                    println!();
                    Ok(RebaseStep::Resolved)
                }
                PushOutcome::Rejected(reason) => {
                    println!("  push still rejected after rebase: {reason}");
                    Ok(RebaseStep::StillRejected(reason))
                }
            }
        }
        Ok(RebaseOutcome::NothingToDo) => {
            // Upstream advanced between rejection and our fetch; a plain
            // push may succeed now. One-shot retry before falling back.
            match git::push(repo, creds).with_context(|| format!("re-pushing `{name}`"))? {
                PushOutcome::Pushed => {
                    println!("  resolved  {name} (pushed)");
                    println!();
                    Ok(RebaseStep::Resolved)
                }
                PushOutcome::Rejected(reason) => Ok(RebaseStep::StillRejected(reason)),
            }
        }
        Ok(RebaseOutcome::ConflictsAborted(paths)) => {
            println!("  rebase aborted — conflicts in {} file(s):", paths.len());
            for p in &paths {
                println!("    - {p}");
            }
            println!("  try [m]anual to resolve by hand");
            println!();
            Ok(RebaseStep::Retry)
        }
        Err(e) => {
            eprintln!("  rebase failed: {e:#}");
            println!();
            Ok(RebaseStep::Retry)
        }
    }
}

fn prompt_via_menu(ctx: &PushPromptCtx<'_>) -> anyhow::Result<PushChoice> {
    print_rejected_header(ctx);
    Ok(build_menu().interact()?)
}

fn print_rejected_header(ctx: &PushPromptCtx<'_>) {
    println!("rejected    {}", ctx.name);
    println!("   reason: {}", ctx.reason);
    println!("   repo: {}", ctx.clone_path.display());
    if let (Some(a), Some(b)) = (ctx.ahead, ctx.behind) {
        println!("   local: {a} ahead, {b} behind upstream");
    }
    println!();
}

fn build_menu() -> Menu<PushChoice> {
    let options = vec![
        MenuOption::new(
            'r',
            "[r]ebase — replay local commits onto upstream, then re-push",
            PushChoice::Rebase,
        ),
        MenuOption::new(
            'm',
            "[m]anual — drop me into a shell at the repo to resolve",
            PushChoice::Manual,
        ),
        MenuOption::new('s', "[s]kip   — leave this repo unpushed", PushChoice::Skip),
        MenuOption::new(
            'a',
            "[a]bort  — stop pushing remaining repos",
            PushChoice::Abort,
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
        choices: Vec<PushChoice>,
    ) -> impl FnMut(&PushPromptCtx<'_>) -> anyhow::Result<PushChoice> {
        let mut queue: VecDeque<PushChoice> = choices.into();
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

    /// Simulates the user resolving the rejection by hard-resetting onto
    /// upstream then replaying their work — the simplest sim is a hard
    /// reset to upstream (drops local commits, push will be a no-op success).
    fn hard_reset_to_upstream_launcher() -> impl FnMut(&Path) -> anyhow::Result<()> {
        |path| {
            let repo = Repository::open(path)?;
            // Make sure we have fresh upstream knowledge.
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
        crate::git::test_support::init_bare(dir.path());
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
        let seed = crate::git::test_support::init(seed_dir.path());
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
    fn pushes_local_commits_when_remote_is_behind() {
        let (remote, url, _b_dir, b_path, b_repo) = fixture_remote_and_clone();
        commit_file(&b_repo, "new.txt", "x", "new commit");

        let config = config_with(vec![("r", url, &b_path)]);
        run_with(
            &config,
            &Credentials::empty(),
            &mut scripted_prompter(vec![]),
            &mut never_called_launcher(),
        )
        .unwrap();

        // Remote should now have b's commit; verify by re-cloning.
        let (_verify_dir, verify_path, _verify_repo) =
            clone_to_tempdir(&format!("file://{}", remote.path().display()));
        assert!(verify_path.join("new.txt").exists());
    }

    #[test]
    fn no_op_when_already_up_to_date() {
        // Local and upstream agree → push is a no-op success.
        let (_remote, url, _b_dir, b_path, _b_repo) = fixture_remote_and_clone();
        let config = config_with(vec![("r", url, &b_path)]);
        run_with(
            &config,
            &Credentials::empty(),
            &mut scripted_prompter(vec![]),
            &mut never_called_launcher(),
        )
        .unwrap();
    }

    /// White-box: when local matches upstream, `process_repo` reports
    /// UpToDate and never invokes the network push.
    #[test]
    fn process_repo_returns_up_to_date_when_nothing_to_ship() {
        let (_remote, url, _b_dir, b_path, _b_repo) = fixture_remote_and_clone();
        let env = SystemEnv;
        let mut map = BTreeMap::new();
        map.insert(
            "r".to_string(),
            RepoConfig {
                repo: url.clone(),
                clone: parse(&b_path.display().to_string()).unwrap(),
                links: vec![],
            },
        );
        let outcome = process_repo(
            "r",
            &map["r"],
            &Credentials::empty(),
            &env,
            &mut scripted_prompter(vec![]),
            &mut never_called_launcher(),
        )
        .unwrap();
        assert!(matches!(outcome, RepoOutcome::UpToDate), "{outcome:?}");
    }

    #[test]
    fn process_repo_pushes_when_local_ahead() {
        let (_remote, url, _b_dir, b_path, b_repo) = fixture_remote_and_clone();
        commit_file(&b_repo, "ahead.txt", "x", "ahead");
        let env = SystemEnv;
        let mut map = BTreeMap::new();
        map.insert(
            "r".to_string(),
            RepoConfig {
                repo: url.clone(),
                clone: parse(&b_path.display().to_string()).unwrap(),
                links: vec![],
            },
        );
        let outcome = process_repo(
            "r",
            &map["r"],
            &Credentials::empty(),
            &env,
            &mut scripted_prompter(vec![]),
            &mut never_called_launcher(),
        )
        .unwrap();
        assert!(matches!(outcome, RepoOutcome::Pushed), "{outcome:?}");
    }

    #[test]
    fn missing_clone_path_is_a_per_repo_failure() {
        // Repo b is missing; repo c is healthy. Run continues.
        let bogus = TempDir::new().unwrap().path().join("never-cloned");
        let (_remote, url, _c_dir, c_path, c_repo) = fixture_remote_and_clone();
        commit_file(&c_repo, "c.txt", "x", "c");

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
            &Credentials::empty(),
            &mut scripted_prompter(vec![]),
            &mut never_called_launcher(),
        )
        .unwrap();
        // Healthy repo's commit still made it.
        let (_verify_dir, verify_path, _verify_repo) =
            clone_to_tempdir(&format!("file://{}", _remote.path().display()));
        assert!(verify_path.join("c.txt").exists());
    }

    #[test]
    fn skip_choice_leaves_rejected_repo_unpushed() {
        // B clones, A pushes ahead, B commits — push is non-FF.
        let (_remote, url, _b_dir, b_path, b_repo) = fixture_remote_and_clone();
        let (_a_dir, _a_path, a_repo) = clone_to_tempdir(&url);
        commit_file(&a_repo, "from-a.txt", "a", "from A");
        push_main(&a_repo);
        commit_file(&b_repo, "from-b.txt", "b", "from B");
        let b_head_before = b_repo.head().unwrap().target().unwrap();

        let config = config_with(vec![("r", url, &b_path)]);
        run_with(
            &config,
            &Credentials::empty(),
            &mut scripted_prompter(vec![PushChoice::Skip]),
            &mut never_called_launcher(),
        )
        .unwrap();

        // B's HEAD did not move (we did not pull).
        let b_head_after = b_repo.head().unwrap().target().unwrap();
        assert_eq!(b_head_before, b_head_after);
        // B's commit is NOT on remote.
        let (_verify_dir, verify_path, _verify_repo) =
            clone_to_tempdir(&format!("file://{}", _remote.path().display()));
        assert!(!verify_path.join("from-b.txt").exists());
    }

    #[test]
    fn abort_short_circuits_remaining_repos() {
        let (_remote1, url1, _b1_dir, b1_path, b1_repo) = fixture_remote_and_clone();
        let (_a1_dir, _a1_path, a1_repo) = clone_to_tempdir(&url1);
        commit_file(&a1_repo, "a1.txt", "a", "a1");
        push_main(&a1_repo);
        commit_file(&b1_repo, "b1.txt", "b", "b1");

        let (remote2, url2, _b2_dir, b2_path, b2_repo) = fixture_remote_and_clone();
        commit_file(&b2_repo, "b2.txt", "x", "b2"); // pushable on its own
        let b2_head_before = b2_repo.head().unwrap().target().unwrap();

        let config = config_with(vec![("aaa", url1, &b1_path), ("zzz", url2, &b2_path)]);
        run_with(
            &config,
            &Credentials::empty(),
            &mut scripted_prompter(vec![PushChoice::Abort]),
            &mut never_called_launcher(),
        )
        .unwrap();

        // Second repo never got pushed (abort fired on first).
        let b2_head_after = b2_repo.head().unwrap().target().unwrap();
        assert_eq!(b2_head_before, b2_head_after);
        let (_verify_dir, verify_path, _verify_repo) =
            clone_to_tempdir(&format!("file://{}", remote2.path().display()));
        assert!(!verify_path.join("b2.txt").exists());
    }

    #[test]
    fn manual_then_resolve_pushes_after_user_action() {
        // B is rejected; manual launcher hard-resets to upstream;
        // re-push then is a no-op success → Resolved.
        let (_remote, url, _b_dir, b_path, b_repo) = fixture_remote_and_clone();
        let (_a_dir, _a_path, a_repo) = clone_to_tempdir(&url);
        commit_file(&a_repo, "from-a.txt", "a", "from A");
        push_main(&a_repo);
        commit_file(&b_repo, "from-b.txt", "b", "from B");

        let config = config_with(vec![("r", url, &b_path)]);
        run_with(
            &config,
            &Credentials::empty(),
            &mut scripted_prompter(vec![PushChoice::Manual]),
            &mut hard_reset_to_upstream_launcher(),
        )
        .unwrap();

        // Hard-reset dropped B's commit; B is now at A's commit.
        assert!(!b_path.join("from-b.txt").exists());
        assert!(b_path.join("from-a.txt").exists());
    }

    #[test]
    fn manual_then_still_rejected_re_prompts_then_skip() {
        let (_remote, url, _b_dir, b_path, b_repo) = fixture_remote_and_clone();
        let (_a_dir, _a_path, a_repo) = clone_to_tempdir(&url);
        commit_file(&a_repo, "from-a.txt", "a", "from A");
        push_main(&a_repo);
        commit_file(&b_repo, "from-b.txt", "b", "from B");

        let config = config_with(vec![("r", url, &b_path)]);
        // Manual → no-op launcher → still rejected → Skip.
        run_with(
            &config,
            &Credentials::empty(),
            &mut scripted_prompter(vec![PushChoice::Manual, PushChoice::Skip]),
            &mut no_op_launcher(),
        )
        .unwrap();

        // B's commit still local, not on remote.
        let (_verify_dir, verify_path, _verify_repo) =
            clone_to_tempdir(&format!("file://{}", _remote.path().display()));
        assert!(!verify_path.join("from-b.txt").exists());
    }

    #[test]
    fn rebase_choice_replays_and_pushes() {
        // B rejects on push (non-FF). User picks [r]ebase → libgit2 replays
        // B's commit onto A's, then re-push succeeds → Resolved.
        let (remote, url, _b_dir, b_path, b_repo) = fixture_remote_and_clone();
        let (_a_dir, _a_path, a_repo) = clone_to_tempdir(&url);
        commit_file(&a_repo, "from-a.txt", "a", "from A");
        push_main(&a_repo);
        commit_file(&b_repo, "from-b.txt", "b", "from B");

        let config = config_with(vec![("r", url, &b_path)]);
        run_with(
            &config,
            &Credentials::empty(),
            &mut scripted_prompter(vec![PushChoice::Rebase]),
            &mut never_called_launcher(),
        )
        .unwrap();

        // Remote now has BOTH A's and B's commits.
        let (_v_dir, v_path, _v_repo) =
            clone_to_tempdir(&format!("file://{}", remote.path().display()));
        assert!(v_path.join("from-a.txt").exists());
        assert!(v_path.join("from-b.txt").exists());
    }

    #[test]
    fn rebase_conflict_re_prompts_then_skip_leaves_local_intact() {
        // A and B both modify README.md. Rebase will conflict → aborted.
        // User then picks [s]kip; B's local HEAD must still point at its
        // original commit (abort restored pre-rebase state).
        let (remote, url, _b_dir, b_path, b_repo) = fixture_remote_and_clone();
        let (_a_dir, _a_path, a_repo) = clone_to_tempdir(&url);
        commit_file(&a_repo, "README.md", "a-wins\n", "A version");
        push_main(&a_repo);
        commit_file(&b_repo, "README.md", "b-wins\n", "B version");
        let head_before = b_repo.head().unwrap().target().unwrap();

        let config = config_with(vec![("r", url, &b_path)]);
        run_with(
            &config,
            &Credentials::empty(),
            &mut scripted_prompter(vec![PushChoice::Rebase, PushChoice::Skip]),
            &mut never_called_launcher(),
        )
        .unwrap();

        // B's HEAD is still its pre-rebase commit.
        let head_after = b_repo.head().unwrap().target().unwrap();
        assert_eq!(head_after, head_before);
        // Remote does NOT have B's version.
        let (_v_dir, v_path, _v_repo) =
            clone_to_tempdir(&format!("file://{}", remote.path().display()));
        let readme_on_remote = std::fs::read_to_string(v_path.join("README.md")).unwrap();
        assert_eq!(readme_on_remote, "a-wins\n");
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
