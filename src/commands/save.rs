// `polydot save` — commit dirty changes + push, across all managed repos.
//
// Mode:
//   -m "<msg>"   → shared: one commit message for every dirty repo.
//   (no flag)    → per-repo: prompt per dirty repo for a message.
//
// Per repo in shared mode:
//   - Stage all + commit (skipped if clean) → push.
//   - On push rejection: prompt [m]anual / [s]kip / [a]bort.
//
// Per repo in per-repo mode (only for dirty repos):
//   - Show a diff-stat header, prompt [m]essage / [v]iew / [s]kip / [a]bort.
//   - [m]essage stages + commits + pushes with the user-supplied text.
//   - [v]iew prints the full patch and re-prompts.
//   - Skip/Abort behave as in shared mode.
//   - Same rejection flow on push.
//
// On rejection we best-effort fetch for ahead/behind in the prompt header,
// then offer [r]ebase / [m]anual / [s]kip / [a]bort:
//   - [r]ebase replays local commits (including any fresh one from save) onto
//     upstream via libgit2, then re-pushes. Conflict → rebase aborts, repo is
//     restored to pre-rebase state, and we re-prompt so the user can fall
//     through to [m]anual.
//   - [m]anual drops the user into `$SHELL` at the repo. After they exit we
//     re-attempt the push. Still rejected → re-prompt; succeeded → Resolved.
//   - [a]bort short-circuits the outer loop and exits 0.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use git2::Repository;

use crate::config::{Config, RepoConfig};
use crate::credentials::Credentials;
use crate::git::{self, DiffSummary, PushOutcome, RebaseOutcome};
use crate::paths::{SystemEnv, expand};
use crate::ui::line_editor::{self, ReadLineOutcome};
use crate::ui::{Menu, MenuOption};

const FALLBACK_SHELL: &str = "/bin/sh";

#[derive(Debug, Default)]
struct Summary {
    committed: usize,
    pushed: usize,
    up_to_date: usize,
    skipped: usize,
    failed: usize,
}

impl Summary {
    fn print(&self) {
        println!(
            "{} committed, {} pushed, {} up-to-date, {} skipped, {} failed",
            self.committed, self.pushed, self.up_to_date, self.skipped, self.failed,
        );
    }
}

/// How save builds each commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Mode {
    /// One message for every dirty repo (from `-m`).
    Shared(String),
    /// Per-repo prompt (no flag).
    PerRepo,
}

pub fn run(config: &Config, message: Option<&str>) -> anyhow::Result<()> {
    let mode = resolve_mode(message);
    let creds = Credentials::load_default().context("loading credentials")?;
    run_with(
        config,
        &mode,
        &creds,
        &mut prompt_rejected_via_menu,
        &mut prompt_commit_via_menu,
        &mut launch_shell,
    )
}

pub(crate) fn resolve_mode(message: Option<&str>) -> Mode {
    match message {
        Some(msg) => Mode::Shared(msg.to_string()),
        None => Mode::PerRepo,
    }
}

/// Snapshot passed to the rejection prompter: which repo, where it lives,
/// the server's reason for rejecting, whether a fresh local commit is sitting
/// on top of the rejected push (so the prompt can warn the user that
/// aborting/skipping leaves committed-but-unpushed work behind), and — when a
/// best-effort fetch succeeded — how many commits local and upstream are apart.
pub(crate) struct SavePromptCtx<'a> {
    pub name: &'a str,
    pub clone_path: &'a Path,
    pub reason: &'a str,
    pub committed_locally: bool,
    pub ahead: Option<usize>,
    pub behind: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SaveChoice {
    Rebase,
    Manual,
    Skip,
    Abort,
}

/// Snapshot passed to the per-repo commit prompter (per-repo mode).
pub(crate) struct CommitPromptCtx<'a> {
    pub name: &'a str,
    pub clone_path: &'a Path,
    pub stats: &'a DiffSummary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommitChoice {
    Message(String),
    View,
    Skip,
    Abort,
}

#[derive(Debug)]
enum RepoOutcome {
    /// Tree was clean and push succeeded (or was a no-op).
    Pushed,
    /// New commit created and push succeeded.
    CommittedAndPushed,
    /// Nothing to commit and local matches upstream — push skipped entirely.
    UpToDate,
    /// Per-repo mode: user chose [s]kip before committing — no work touched.
    SkippedClean,
    /// Push rejected, user chose [s]kip. Commit may still be sitting locally.
    Skipped { committed: bool },
    /// User chose [a]bort during the rejection prompt.
    Aborted { committed: bool },
    /// Per-repo mode: user chose [a]bort before committing — no work touched.
    AbortedClean,
    /// Push rejected, user resolved via [m]anual then re-push succeeded.
    Resolved { committed: bool },
}

/// Test seam: same as [`run`], but the rejection prompter, commit prompter,
/// and shell launcher are injected. Production wires them to interactive
/// menus and a real `$SHELL` spawn; tests pass scripted closures so the
/// prompt + manual-loop paths are exercised without a TTY or real shell.
pub(crate) fn run_with<P, C, S>(
    config: &Config,
    mode: &Mode,
    creds: &Credentials,
    rejection_prompter: &mut P,
    commit_prompter: &mut C,
    shell_launcher: &mut S,
) -> anyhow::Result<()>
where
    P: FnMut(&SavePromptCtx<'_>) -> anyhow::Result<SaveChoice>,
    C: FnMut(&CommitPromptCtx<'_>) -> anyhow::Result<CommitChoice>,
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
            mode,
            creds,
            &env,
            rejection_prompter,
            commit_prompter,
            shell_launcher,
        );
        match result {
            Ok(RepoOutcome::Pushed) => summary.pushed += 1,
            Ok(RepoOutcome::CommittedAndPushed) => {
                summary.committed += 1;
                summary.pushed += 1;
            }
            Ok(RepoOutcome::UpToDate) => summary.up_to_date += 1,
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
            Ok(RepoOutcome::SkippedClean) => summary.skipped += 1,
            Ok(RepoOutcome::Aborted { committed }) => {
                if committed {
                    summary.committed += 1;
                }
                summary.skipped += 1;
                break 'outer;
            }
            Ok(RepoOutcome::AbortedClean) => {
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

#[allow(clippy::too_many_arguments)]
fn process_repo<P, C, S>(
    name: &str,
    repo_cfg: &RepoConfig,
    mode: &Mode,
    creds: &Credentials,
    env: &SystemEnv,
    rejection_prompter: &mut P,
    commit_prompter: &mut C,
    shell_launcher: &mut S,
) -> anyhow::Result<RepoOutcome>
where
    P: FnMut(&SavePromptCtx<'_>) -> anyhow::Result<SaveChoice>,
    C: FnMut(&CommitPromptCtx<'_>) -> anyhow::Result<CommitChoice>,
    S: FnMut(&Path) -> anyhow::Result<()>,
{
    let (repo, clone_path, committed) =
        match commit_phase(name, repo_cfg, mode, env, commit_prompter)? {
            CommitPhaseOutcome::Committed { repo, clone_path } => (repo, clone_path, true),
            CommitPhaseOutcome::NothingToCommit { repo, clone_path } => (repo, clone_path, false),
            CommitPhaseOutcome::UserSkipped => return Ok(RepoOutcome::SkippedClean),
            CommitPhaseOutcome::UserAborted => return Ok(RepoOutcome::AbortedClean),
        };

    if !committed && !has_unpushed_commits(&repo) {
        println!("up-to-date          {name}");
        return Ok(RepoOutcome::UpToDate);
    }

    match git::push(&repo, creds).with_context(|| format!("pushing `{name}`"))? {
        PushOutcome::Pushed => {
            if committed {
                println!("committed + pushed  {name}");
                println!();
                Ok(RepoOutcome::CommittedAndPushed)
            } else {
                println!("pushed              {name}");
                println!();
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
            rejection_prompter,
            shell_launcher,
        ),
    }
}

/// Outcome of the commit-only phase shared by `save` and `commit`. Both
/// commands need to turn a repo + a mode into either "a fresh commit was
/// made", "tree was clean", or "user bailed out of per-repo mode". `save`
/// then proceeds to push; `commit` stops here.
pub(crate) enum CommitPhaseOutcome {
    /// A new commit was created on top of HEAD.
    Committed {
        repo: Repository,
        clone_path: PathBuf,
    },
    /// Tree was clean; no commit made. May still have unpushed commits —
    /// the caller decides what that means for them.
    NothingToCommit {
        repo: Repository,
        clone_path: PathBuf,
    },
    /// Per-repo mode: user chose [s]kip at the message prompt.
    UserSkipped,
    /// Per-repo mode: user chose [a]bort at the message prompt.
    UserAborted,
}

pub(crate) fn commit_phase<C>(
    name: &str,
    repo_cfg: &RepoConfig,
    mode: &Mode,
    env: &SystemEnv,
    commit_prompter: &mut C,
) -> anyhow::Result<CommitPhaseOutcome>
where
    C: FnMut(&CommitPromptCtx<'_>) -> anyhow::Result<CommitChoice>,
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

    let message = match mode {
        Mode::Shared(msg) => Some(msg.clone()),
        Mode::PerRepo => match resolve_per_repo_message(name, &clone_path, &repo, commit_prompter)?
        {
            PerRepoOutcome::Message(msg) => Some(msg),
            PerRepoOutcome::Skip => return Ok(CommitPhaseOutcome::UserSkipped),
            PerRepoOutcome::Abort => return Ok(CommitPhaseOutcome::UserAborted),
            // Tree clean → fall through; caller (save) may still push pre-existing commits.
            PerRepoOutcome::NothingToCommit => None,
        },
    };

    let committed = match message {
        Some(msg) => git::commit_all(&repo, &msg)
            .with_context(|| format!("committing dirty changes in `{name}`"))?
            .is_some(),
        None => false,
    };

    if committed {
        Ok(CommitPhaseOutcome::Committed { repo, clone_path })
    } else {
        Ok(CommitPhaseOutcome::NothingToCommit { repo, clone_path })
    }
}

/// True if local has commits the upstream lacks (or upstream state is
/// unknown — in which case we err on the side of attempting the push, since
/// a no-op push is harmless and a missed push is not).
fn has_unpushed_commits(repo: &Repository) -> bool {
    match git::status(repo) {
        Ok(s) => match s.ahead_behind {
            Some((ahead, _)) => ahead > 0,
            None => true,
        },
        Err(_) => true,
    }
}

enum PerRepoOutcome {
    Message(String),
    Skip,
    Abort,
    NothingToCommit,
}

/// Per-repo prompt loop: show diff stats, accept [m]essage/[v]iew/[s]kip/[a]bort.
/// Returns once the user produces a final decision (message / skip / abort)
/// or the tree is clean (no header, no prompt — just push existing commits).
fn resolve_per_repo_message<C>(
    name: &str,
    clone_path: &Path,
    repo: &Repository,
    commit_prompter: &mut C,
) -> anyhow::Result<PerRepoOutcome>
where
    C: FnMut(&CommitPromptCtx<'_>) -> anyhow::Result<CommitChoice>,
{
    let stats = git::diff_summary(repo)
        .with_context(|| format!("computing diff for `{name}`"))?
        .unwrap_or(DiffSummary {
            files_changed: 0,
            insertions: 0,
            deletions: 0,
            formatted: String::new(),
        });
    if stats.files_changed == 0 {
        return Ok(PerRepoOutcome::NothingToCommit);
    }
    let ctx = CommitPromptCtx {
        name,
        clone_path,
        stats: &stats,
    };
    print_commit_header(&ctx);
    loop {
        match commit_prompter(&ctx)? {
            CommitChoice::Message(msg) => return Ok(PerRepoOutcome::Message(msg)),
            CommitChoice::Skip => {
                println!("  skipped  {name}");
                return Ok(PerRepoOutcome::Skip);
            }
            CommitChoice::Abort => {
                println!("  aborting save");
                return Ok(PerRepoOutcome::Abort);
            }
            CommitChoice::View => {
                println!("{RULE}");
                git::print_diff(repo).with_context(|| format!("rendering diff for `{name}`"))?;
                println!("{RULE}");
                println!();
                continue;
            }
        }
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
    rejection_prompter: &mut P,
    shell_launcher: &mut S,
) -> anyhow::Result<RepoOutcome>
where
    P: FnMut(&SavePromptCtx<'_>) -> anyhow::Result<SaveChoice>,
    S: FnMut(&Path) -> anyhow::Result<()>,
{
    // Best-effort fetch so the prompt header can report accurate divergence.
    let _ = git::fetch(repo, creds);
    let mut current_reason = initial_reason.to_string();
    loop {
        let (ahead, behind) = divergence(repo);
        let ctx = SavePromptCtx {
            name,
            clone_path,
            reason: &current_reason,
            committed_locally: committed,
            ahead,
            behind,
        };
        match rejection_prompter(&ctx)? {
            SaveChoice::Skip => {
                println!("  skipped  {name}");
                return Ok(RepoOutcome::Skipped { committed });
            }
            SaveChoice::Abort => {
                println!("  aborting save");
                return Ok(RepoOutcome::Aborted { committed });
            }
            SaveChoice::Rebase => match run_rebase_then_push(name, repo, creds, committed)? {
                RebaseStep::Resolved => return Ok(RepoOutcome::Resolved { committed }),
                RebaseStep::StillRejected(reason) => {
                    current_reason = reason;
                    let _ = git::fetch(repo, creds);
                    continue;
                }
                RebaseStep::Retry => continue,
            },
            SaveChoice::Manual => {
                shell_launcher(clone_path)
                    .with_context(|| format!("launching shell at {}", clone_path.display()))?;
                match git::push(repo, creds).with_context(|| format!("re-pushing `{name}`"))? {
                    PushOutcome::Pushed => {
                        println!("  resolved  {name} (pushed)");
                        println!();
                        return Ok(RepoOutcome::Resolved { committed });
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
    committed: bool,
) -> anyhow::Result<RebaseStep> {
    match git::rebase_onto_upstream(repo).with_context(|| format!("rebasing `{name}`")) {
        Ok(RebaseOutcome::Completed) => {
            println!("  rebased  {name}");
            match git::push(repo, creds).with_context(|| format!("re-pushing `{name}`"))? {
                PushOutcome::Pushed => {
                    let suffix = if committed {
                        "rebased + pushed, commit preserved"
                    } else {
                        "rebased + pushed"
                    };
                    println!("  resolved  {name} ({suffix})");
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

fn prompt_rejected_via_menu(ctx: &SavePromptCtx<'_>) -> anyhow::Result<SaveChoice> {
    print_rejected_header(ctx);
    Ok(build_rejected_menu().interact()?)
}

fn print_rejected_header(ctx: &SavePromptCtx<'_>) {
    println!("rejected            {}", ctx.name);
    println!("   reason: {}", ctx.reason);
    println!("   repo: {}", ctx.clone_path.display());
    if let (Some(a), Some(b)) = (ctx.ahead, ctx.behind) {
        println!("   local: {a} ahead, {b} behind upstream");
    }
    if ctx.committed_locally {
        println!("   (your changes ARE committed locally; only the push was rejected)");
    }
    println!();
}

fn build_rejected_menu() -> Menu<SaveChoice> {
    let options = vec![
        MenuOption::new(
            'r',
            "[r]ebase — replay local commits onto upstream, then re-push",
            SaveChoice::Rebase,
        ),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommitMenuChoice {
    Message,
    View,
    Skip,
    Abort,
}

/// Production commit prompter: show menu, and on `[m]essage` read a single
/// line from stdin. `[v]iew` returns to the caller loop to print the diff
/// and re-prompt. Header is printed once by the caller before the first
/// prompt — re-prompts after `[v]iew` go straight to the menu.
pub(crate) fn prompt_commit_via_menu(_ctx: &CommitPromptCtx<'_>) -> anyhow::Result<CommitChoice> {
    let menu = Menu::new(vec![
        MenuOption::new(
            'm',
            "[m]essage — type a commit message",
            CommitMenuChoice::Message,
        ),
        MenuOption::new('v', "[v]iew    — show full diff", CommitMenuChoice::View),
        MenuOption::new(
            's',
            "[s]kip    — leave this repo as-is",
            CommitMenuChoice::Skip,
        ),
        MenuOption::new(
            'a',
            "[a]bort   — stop saving remaining repos",
            CommitMenuChoice::Abort,
        ),
    ])
    .default_shortcut('m')
    .cancel_shortcut('a');
    Ok(match menu.interact()? {
        CommitMenuChoice::Message => CommitChoice::Message(read_commit_message()?),
        CommitMenuChoice::View => CommitChoice::View,
        CommitMenuChoice::Skip => CommitChoice::Skip,
        CommitMenuChoice::Abort => CommitChoice::Abort,
    })
}

const RULE: &str = "==================================================";

fn print_commit_header(ctx: &CommitPromptCtx<'_>) {
    println!("dirty               {}", ctx.name);
    for line in ctx.stats.formatted.lines() {
        println!("   {}", line.trim_start());
    }
    println!("   repo: {}", ctx.clone_path.display());
    println!();
}

fn read_commit_message() -> anyhow::Result<String> {
    match line_editor::read_line("message> ").context("reading commit message")? {
        ReadLineOutcome::Line(s) => {
            let trimmed = s.trim().to_string();
            if trimmed.is_empty() {
                anyhow::bail!("commit message cannot be empty");
            }
            Ok(trimmed)
        }
        ReadLineOutcome::Cancelled => anyhow::bail!("commit message input cancelled"),
        ReadLineOutcome::Eof => anyhow::bail!("commit message input ended unexpectedly (EOF)"),
    }
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
    let s = expand(&repo_cfg.clone, env)
        .with_context(|| format!("evaluating clone path for `{name}`"))?;
    Ok(PathBuf::from(s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RepoConfig;
    use git2::{BranchType, Repository};
    use std::collections::{BTreeMap, VecDeque};
    use std::fs;
    use tempfile::TempDir;

    fn config_with(repos: Vec<(&str, String, &Path)>) -> Config {
        let mut map = BTreeMap::new();
        for (name, url, clone_path) in repos {
            map.insert(
                name.to_string(),
                RepoConfig {
                    repo: url,
                    clone: clone_path.to_string_lossy().into_owned(),
                    links: vec![],
                },
            );
        }
        Config {
            path: None,
            repos: map,
        }
    }

    fn shared(msg: &str) -> Mode {
        Mode::Shared(msg.to_string())
    }

    fn scripted_rejection_prompter(
        choices: Vec<SaveChoice>,
    ) -> impl FnMut(&SavePromptCtx<'_>) -> anyhow::Result<SaveChoice> {
        let mut queue: VecDeque<SaveChoice> = choices.into();
        move |_ctx| {
            queue
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("scripted rejection prompter exhausted"))
        }
    }

    fn scripted_commit_prompter(
        choices: Vec<CommitChoice>,
    ) -> impl FnMut(&CommitPromptCtx<'_>) -> anyhow::Result<CommitChoice> {
        let mut queue: VecDeque<CommitChoice> = choices.into();
        move |_ctx| {
            queue
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("scripted commit prompter exhausted"))
        }
    }

    fn never_called_commit_prompter()
    -> impl FnMut(&CommitPromptCtx<'_>) -> anyhow::Result<CommitChoice> {
        |_ctx| panic!("commit prompter should not be called in shared mode")
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
    fn commits_dirty_changes_then_pushes() {
        let (remote, url, _b_dir, b_path, _b_repo) = fixture_remote_and_clone();
        fs::write(b_path.join("notes.md"), "fresh\n").unwrap();

        let config = config_with(vec![("r", url, &b_path)]);
        run_with(
            &config,
            &shared("add notes"),
            &Credentials::empty(),
            &mut scripted_rejection_prompter(vec![]),
            &mut never_called_commit_prompter(),
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
            &shared("should not be used"),
            &Credentials::empty(),
            &mut scripted_rejection_prompter(vec![]),
            &mut never_called_commit_prompter(),
            &mut never_called_launcher(),
        )
        .unwrap();

        // No new commit was made.
        let head_after = b_repo.head().unwrap().target().unwrap();
        assert_eq!(head_before, head_after);
    }

    /// White-box: when nothing changed and nothing is sitting unpushed locally,
    /// `process_repo` should report `UpToDate` rather than `Pushed`.
    #[test]
    fn process_repo_returns_up_to_date_when_nothing_to_ship() {
        let (_remote, url, _b_dir, b_path, _b_repo) = fixture_remote_and_clone();
        let env = SystemEnv;
        let mut map = BTreeMap::new();
        map.insert(
            "r".to_string(),
            RepoConfig {
                repo: url.clone(),
                clone: b_path.to_string_lossy().into_owned(),
                links: vec![],
            },
        );
        let outcome = process_repo(
            "r",
            &map["r"],
            &shared("unused"),
            &Credentials::empty(),
            &env,
            &mut scripted_rejection_prompter(vec![]),
            &mut never_called_commit_prompter(),
            &mut never_called_launcher(),
        )
        .unwrap();
        assert!(matches!(outcome, RepoOutcome::UpToDate), "{outcome:?}");
    }

    /// White-box: clean tree but a local commit ahead of upstream → push runs
    /// (returns Pushed, not UpToDate).
    #[test]
    fn process_repo_pushes_when_orphan_commit_present() {
        let (_remote, url, _b_dir, b_path, b_repo) = fixture_remote_and_clone();
        commit_file(&b_repo, "earlier.txt", "x", "earlier commit");
        let env = SystemEnv;
        let mut map = BTreeMap::new();
        map.insert(
            "r".to_string(),
            RepoConfig {
                repo: url.clone(),
                clone: b_path.to_string_lossy().into_owned(),
                links: vec![],
            },
        );
        let outcome = process_repo(
            "r",
            &map["r"],
            &shared("unused"),
            &Credentials::empty(),
            &env,
            &mut scripted_rejection_prompter(vec![]),
            &mut never_called_commit_prompter(),
            &mut never_called_launcher(),
        )
        .unwrap();
        assert!(matches!(outcome, RepoOutcome::Pushed), "{outcome:?}");
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
            &shared("unused"),
            &Credentials::empty(),
            &mut scripted_rejection_prompter(vec![]),
            &mut never_called_commit_prompter(),
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
            &shared("add c"),
            &Credentials::empty(),
            &mut scripted_rejection_prompter(vec![]),
            &mut never_called_commit_prompter(),
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
            &shared("from B"),
            &Credentials::empty(),
            &mut scripted_rejection_prompter(vec![SaveChoice::Skip]),
            &mut never_called_commit_prompter(),
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
            &shared("msg"),
            &Credentials::empty(),
            &mut scripted_rejection_prompter(vec![SaveChoice::Abort]),
            &mut never_called_commit_prompter(),
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
            &shared("from B"),
            &Credentials::empty(),
            &mut scripted_rejection_prompter(vec![SaveChoice::Manual]),
            &mut never_called_commit_prompter(),
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
            &shared("from B"),
            &Credentials::empty(),
            &mut scripted_rejection_prompter(vec![SaveChoice::Manual, SaveChoice::Skip]),
            &mut never_called_commit_prompter(),
            &mut no_op_launcher(),
        )
        .unwrap();

        // B's commit still local, not on remote.
        let (_v_dir, v_path, _v_repo) =
            clone_to_tempdir(&format!("file://{}", remote.path().display()));
        assert!(!v_path.join("from-b.txt").exists());
    }

    #[test]
    fn rebase_choice_replays_local_commit_and_pushes() {
        // B has a fresh dirty file. A pushes an unrelated file ahead. Save
        // commits B's change, push rejects (non-FF). User picks [r]ebase →
        // B's commit replays onto A's, re-push succeeds. Both files land.
        let (remote, url, _b_dir, b_path, _b_repo) = fixture_remote_and_clone();
        let (_a_dir, _a_path, a_repo) = clone_to_tempdir(&url);
        commit_file(&a_repo, "from-a.txt", "a", "from A");
        push_main(&a_repo);
        fs::write(b_path.join("from-b.txt"), "b").unwrap();

        let config = config_with(vec![("r", url, &b_path)]);
        run_with(
            &config,
            &shared("from B"),
            &Credentials::empty(),
            &mut scripted_rejection_prompter(vec![SaveChoice::Rebase]),
            &mut never_called_commit_prompter(),
            &mut never_called_launcher(),
        )
        .unwrap();

        let (_v_dir, v_path, _v_repo) =
            clone_to_tempdir(&format!("file://{}", remote.path().display()));
        assert!(v_path.join("from-a.txt").exists());
        assert!(v_path.join("from-b.txt").exists());
    }

    #[test]
    fn rebase_conflict_re_prompts_then_skip_keeps_local_commit() {
        // A and B both modify README.md. Save commits B's change, push
        // rejects. User picks [r]ebase → conflict → rebase aborted. User
        // then picks [s]kip. B's local HEAD must still be its fresh commit
        // (rebase abort restored pre-rebase state).
        let (remote, url, _b_dir, b_path, b_repo) = fixture_remote_and_clone();
        let (_a_dir, _a_path, a_repo) = clone_to_tempdir(&url);
        commit_file(&a_repo, "README.md", "a-wins\n", "A version");
        push_main(&a_repo);
        fs::write(b_path.join("README.md"), "b-wins\n").unwrap();

        let config = config_with(vec![("r", url, &b_path)]);
        run_with(
            &config,
            &shared("B's version"),
            &Credentials::empty(),
            &mut scripted_rejection_prompter(vec![SaveChoice::Rebase, SaveChoice::Skip]),
            &mut never_called_commit_prompter(),
            &mut never_called_launcher(),
        )
        .unwrap();

        // B has committed its change locally and it's still there.
        let head_msg = b_repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .message()
            .unwrap()
            .to_string();
        assert_eq!(head_msg, "B's version");
        let readme_local = std::fs::read_to_string(b_path.join("README.md")).unwrap();
        assert_eq!(readme_local, "b-wins\n");

        // Remote still has A's version.
        let (_v_dir, v_path, _v_repo) =
            clone_to_tempdir(&format!("file://{}", remote.path().display()));
        let readme_remote = std::fs::read_to_string(v_path.join("README.md")).unwrap();
        assert_eq!(readme_remote, "a-wins\n");
    }

    #[test]
    fn empty_config_is_a_no_op() {
        let config = config_with(vec![]);
        run_with(
            &config,
            &shared("msg"),
            &Credentials::empty(),
            &mut scripted_rejection_prompter(vec![]),
            &mut never_called_commit_prompter(),
            &mut never_called_launcher(),
        )
        .unwrap();
    }

    // ---- Mode resolution ----

    #[test]
    fn resolve_mode_message_is_shared() {
        assert_eq!(resolve_mode(Some("hi")), Mode::Shared("hi".to_string()));
    }

    #[test]
    fn resolve_mode_none_is_per_repo() {
        assert_eq!(resolve_mode(None), Mode::PerRepo);
    }

    // ---- Per-repo mode flow ----

    #[test]
    fn per_repo_message_commits_and_pushes() {
        let (remote, url, _b_dir, b_path, _b_repo) = fixture_remote_and_clone();
        fs::write(b_path.join("notes.md"), "fresh\n").unwrap();

        let config = config_with(vec![("r", url, &b_path)]);
        run_with(
            &config,
            &Mode::PerRepo,
            &Credentials::empty(),
            &mut scripted_rejection_prompter(vec![]),
            &mut scripted_commit_prompter(vec![CommitChoice::Message(
                "interactive notes".to_string(),
            )]),
            &mut never_called_launcher(),
        )
        .unwrap();

        let (_v_dir, v_path, _v_repo) =
            clone_to_tempdir(&format!("file://{}", remote.path().display()));
        assert!(v_path.join("notes.md").exists());
    }

    #[test]
    fn per_repo_skip_leaves_dirty_tree_alone() {
        let (remote, url, _b_dir, b_path, b_repo) = fixture_remote_and_clone();
        fs::write(b_path.join("untracked.txt"), "x").unwrap();
        let head_before = b_repo.head().unwrap().target().unwrap();

        let config = config_with(vec![("r", url, &b_path)]);
        run_with(
            &config,
            &Mode::PerRepo,
            &Credentials::empty(),
            &mut scripted_rejection_prompter(vec![]),
            &mut scripted_commit_prompter(vec![CommitChoice::Skip]),
            &mut never_called_launcher(),
        )
        .unwrap();

        let head_after = b_repo.head().unwrap().target().unwrap();
        assert_eq!(head_before, head_after);
        // Untracked file still on disk.
        assert!(b_path.join("untracked.txt").exists());
        // Remote untouched.
        let (_v_dir, v_path, _v_repo) =
            clone_to_tempdir(&format!("file://{}", remote.path().display()));
        assert!(!v_path.join("untracked.txt").exists());
    }

    #[test]
    fn per_repo_view_then_message_commits() {
        // First [v]iew prints the diff (from print_diff), then re-prompts;
        // user then provides a message. End state: committed + pushed.
        let (remote, url, _b_dir, b_path, _b_repo) = fixture_remote_and_clone();
        fs::write(b_path.join("notes.md"), "fresh\n").unwrap();

        let config = config_with(vec![("r", url, &b_path)]);
        run_with(
            &config,
            &Mode::PerRepo,
            &Credentials::empty(),
            &mut scripted_rejection_prompter(vec![]),
            &mut scripted_commit_prompter(vec![
                CommitChoice::View,
                CommitChoice::View,
                CommitChoice::Message("after view".to_string()),
            ]),
            &mut never_called_launcher(),
        )
        .unwrap();

        let (_v_dir, v_path, _v_repo) =
            clone_to_tempdir(&format!("file://{}", remote.path().display()));
        assert!(v_path.join("notes.md").exists());
    }

    #[test]
    fn per_repo_abort_short_circuits() {
        let (_remote1, url1, _b1_dir, b1_path, _b1_repo) = fixture_remote_and_clone();
        fs::write(b1_path.join("a.txt"), "x").unwrap();
        let (remote2, url2, _b2_dir, b2_path, _b2_repo) = fixture_remote_and_clone();
        fs::write(b2_path.join("b.txt"), "x").unwrap();

        let config = config_with(vec![("aaa", url1, &b1_path), ("zzz", url2, &b2_path)]);
        run_with(
            &config,
            &Mode::PerRepo,
            &Credentials::empty(),
            &mut scripted_rejection_prompter(vec![]),
            &mut scripted_commit_prompter(vec![CommitChoice::Abort]),
            &mut never_called_launcher(),
        )
        .unwrap();

        // Second repo never processed.
        let (_v_dir, v_path, _v_repo) =
            clone_to_tempdir(&format!("file://{}", remote2.path().display()));
        assert!(!v_path.join("b.txt").exists());
    }

    #[test]
    fn per_repo_clean_repo_skips_prompt_and_pushes_pre_existing_commits() {
        // Tree clean but local ahead of upstream (orphan commit). Per-repo
        // mode should not prompt — just push the existing commit.
        let (remote, url, _b_dir, b_path, b_repo) = fixture_remote_and_clone();
        commit_file(&b_repo, "earlier.txt", "x", "earlier commit");

        let config = config_with(vec![("r", url, &b_path)]);
        run_with(
            &config,
            &Mode::PerRepo,
            &Credentials::empty(),
            &mut scripted_rejection_prompter(vec![]),
            &mut never_called_commit_prompter(),
            &mut never_called_launcher(),
        )
        .unwrap();

        let (_v_dir, v_path, _v_repo) =
            clone_to_tempdir(&format!("file://{}", remote.path().display()));
        assert!(v_path.join("earlier.txt").exists());
    }
}
