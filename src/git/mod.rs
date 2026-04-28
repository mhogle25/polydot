// Git operations layer.
//
// Local reads (open, status, ahead/behind, diff, commit, rebase) wrap git2.
// Network ops (clone, fetch, push) shell out to the user's `git` binary so
// auth — SSH keys, credential helpers, GPG signing — is inherited from the
// user's git config rather than reimplemented here.

use std::io::IsTerminal;
use std::path::Path;
use std::process::Command;

use git2::build::CheckoutBuilder;
use git2::{
    BranchType, DiffFormat, DiffLineType, DiffOptions, DiffStatsFormat, IndexAddOption, Oid,
    Repository, StatusOptions,
};

use crate::error::{Error, Result};

/// Snapshot of a repo's local-only git state. Network is never touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitStatus {
    /// Working tree has staged or unstaged modifications, or untracked files.
    pub dirty: bool,
    /// Current branch's short name (`main`), or `None` if HEAD is detached.
    pub branch: Option<String>,
    /// Upstream tracking branch's short name (`origin/main`), if any.
    pub upstream: Option<String>,
    /// `(ahead, behind)` commits relative to upstream. `None` if no upstream
    /// or HEAD is detached.
    pub ahead_behind: Option<(usize, usize)>,
}

/// Outcome of a fast-forward attempt against the configured upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FastForward {
    /// Local branch was advanced to upstream's commit.
    Advanced,
    /// Local already at or ahead of upstream — no work needed.
    AlreadyUpToDate,
    /// Local has commits upstream doesn't (or working tree is dirty).
    /// Caller must resolve manually.
    Diverged,
}

/// Outcome of a push attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushOutcome {
    /// Server accepted the push.
    Pushed,
    /// Server rejected the push (e.g., non-fast-forward). String is the reason.
    Rejected(String),
}

/// Outcome of a rebase attempt against the configured upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseOutcome {
    /// All local commits were replayed onto upstream successfully.
    Completed,
    /// Local has no commits beyond upstream — rebase would be a no-op.
    NothingToDo,
    /// A replay step produced merge conflicts. Rebase was aborted and the
    /// repo restored to its pre-rebase state. The strings name the conflicted
    /// paths as reported by libgit2's index.
    ConflictsAborted(Vec<String>),
}

pub fn open(repo_path: &Path) -> Result<Repository> {
    Repository::open(repo_path).map_err(Error::from)
}

/// Pre-flight: verify the clone's `origin` URL uses a scheme polydot can
/// authenticate. Replaces libgit2's cryptic "unsupported credential type"
/// error with an actionable message pointing at the fix command.
///
/// `expected_url` is the URL from `config.toml` — surfaced in the error as
/// the suggested `git remote set-url` target so the user can copy-paste.
///
/// Only schemes are checked. Both URLs being HTTPS but pointing at different
/// repos is *not* flagged — that's a legitimate user choice (mirror, fork).
pub fn ensure_origin_speakable(repo: &Repository, expected_url: &str) -> Result<()> {
    let remote = repo.find_remote("origin")?;
    let url = remote
        .url()
        .ok_or_else(|| Error::Config("origin has no URL configured".to_string()))?;
    if url.starts_with("https://") || url.starts_with("file://") {
        return Ok(());
    }
    let clone_path = repo
        .workdir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<repo>".to_string());
    Err(Error::Config(format!(
        "origin URL `{url}` uses a scheme polydot can't authenticate \
         (supported: https://, file://).\n\
         \n  \
         config.toml expects: {expected_url}\n\
         \n  \
         Fix: git -C {clone_path} remote set-url origin {expected_url}"
    )))
}

/// Build a `git` Command with non-interactive env: no HTTPS credential
/// prompt, and (when stdin isn't a TTY) no SSH passphrase prompt either.
/// Hooks/cron contexts get a fast failure instead of a hung process.
fn git_command() -> Command {
    let mut cmd = Command::new("git");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    if !std::io::stdin().is_terminal() {
        cmd.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    cmd
}

/// Translate a `Command` invocation result into a Result<()>, attaching
/// the captured stderr on failure so the user sees git's own error message.
fn run_git(cmd: &mut Command, action: &str) -> Result<()> {
    let output = cmd.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::Config("`git` command not found — install git to use polydot".to_string())
        } else {
            Error::Config(format!("invoking git for {action}: {e}"))
        }
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Config(format!(
            "git {action} failed (exit {}): {}",
            output.status.code().unwrap_or(-1),
            stderr.trim()
        )));
    }
    Ok(())
}

/// Clone `url` into `dest`. The clone path's parent must exist.
pub fn clone(url: &str, dest: &Path) -> Result<Repository> {
    run_git(
        git_command().arg("clone").arg(url).arg(dest),
        &format!("clone {url}"),
    )?;
    open(dest)
}

/// Fetch from `origin`, no merge.
pub fn fetch(repo: &Repository) -> Result<()> {
    let workdir = repo
        .workdir()
        .ok_or_else(|| Error::Config("repository has no working directory".to_string()))?;
    run_git(
        git_command()
            .arg("-C")
            .arg(workdir)
            .arg("fetch")
            .arg("origin"),
        "fetch",
    )
}

/// Try to fast-forward the current branch to its upstream. Local-only — call
/// `fetch()` first if you want fresh upstream state.
///
/// Returns `Diverged` if the local branch has commits upstream doesn't, OR
/// if the working tree is dirty (we refuse to clobber uncommitted work).
pub fn try_fast_forward(repo: &Repository) -> Result<FastForward> {
    let head = repo.head()?;
    if !head.is_branch() {
        return Err(Error::Config("HEAD is detached".to_string()));
    }
    let branch_name = head
        .shorthand()
        .ok_or_else(|| Error::Config("HEAD has no shorthand name".to_string()))?
        .to_string();

    let local_branch = repo.find_branch(&branch_name, BranchType::Local)?;
    let upstream = match local_branch.upstream() {
        Ok(u) => u,
        Err(e) if e.code() == git2::ErrorCode::NotFound => {
            return Err(Error::Config(format!(
                "branch `{branch_name}` has no upstream configured"
            )));
        }
        Err(e) => return Err(e.into()),
    };

    let local_oid = head
        .target()
        .ok_or_else(|| Error::Config(format!("branch `{branch_name}` has no target commit")))?;
    let upstream_oid = upstream
        .get()
        .target()
        .ok_or_else(|| Error::Config("upstream has no target commit".to_string()))?;

    if local_oid == upstream_oid {
        return Ok(FastForward::AlreadyUpToDate);
    }

    let (ahead, behind) = repo.graph_ahead_behind(local_oid, upstream_oid)?;
    if behind == 0 {
        // Local is at or ahead of upstream — no fast-forward needed.
        return Ok(FastForward::AlreadyUpToDate);
    }
    if ahead > 0 {
        // Both sides advanced.
        return Ok(FastForward::Diverged);
    }
    if is_dirty(repo)? {
        return Ok(FastForward::Diverged);
    }

    // ahead == 0, behind > 0, clean tree → safe to fast-forward.
    let upstream_commit = repo.find_commit(upstream_oid)?;
    let mut checkout = CheckoutBuilder::new();
    checkout.safe();
    repo.reset(
        upstream_commit.as_object(),
        git2::ResetType::Hard,
        Some(&mut checkout),
    )?;
    drop(local_branch);
    Ok(FastForward::Advanced)
}

/// Summary of working-tree changes against HEAD, suitable for the per-repo
/// save header. `formatted` is `git diff --stat` style output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffSummary {
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
    /// Per-file `--stat` block (multi-line). Empty string if `files_changed == 0`.
    pub formatted: String,
}

/// Diff options tuned for save's "what would get committed" view: includes
/// untracked files, excludes ignored, treats new files as all-insertions.
fn save_diff_options() -> DiffOptions {
    let mut opts = DiffOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .show_untracked_content(true);
    opts
}

/// Stats of workdir+index vs HEAD — the diff that `commit_all` would stage.
/// Returns `None` if HEAD is unborn (no commits yet), since there's no tree
/// to diff against.
pub fn diff_summary(repo: &Repository) -> Result<Option<DiffSummary>> {
    let head_tree = match repo.head() {
        Ok(head) => head.peel_to_tree()?,
        Err(e) if e.code() == git2::ErrorCode::UnbornBranch => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut opts = save_diff_options();
    let diff = repo.diff_tree_to_workdir_with_index(Some(&head_tree), Some(&mut opts))?;
    let stats = diff.stats()?;
    let formatted = if stats.files_changed() == 0 {
        String::new()
    } else {
        stats
            .to_buf(DiffStatsFormat::FULL, 80)?
            .as_str()
            .unwrap_or("")
            .to_string()
    };
    Ok(Some(DiffSummary {
        files_changed: stats.files_changed(),
        insertions: stats.insertions(),
        deletions: stats.deletions(),
        formatted,
    }))
}

/// Print the workdir+index vs HEAD diff as a unified patch to stdout. Used
/// by the per-repo save widget's `[v]iew` action. No-op if HEAD is unborn.
pub fn print_diff(repo: &Repository) -> Result<()> {
    let head_tree = match repo.head() {
        Ok(head) => head.peel_to_tree()?,
        Err(e) if e.code() == git2::ErrorCode::UnbornBranch => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let mut opts = save_diff_options();
    let diff = repo.diff_tree_to_workdir_with_index(Some(&head_tree), Some(&mut opts))?;
    diff.print(DiffFormat::Patch, |_delta, _hunk, line| {
        match line.origin_value() {
            DiffLineType::FileHeader | DiffLineType::HunkHeader | DiffLineType::Binary => {}
            _ => print!("{}", line.origin()),
        }
        print!("{}", std::str::from_utf8(line.content()).unwrap_or(""));
        true
    })?;
    Ok(())
}

/// Stage all changes (new, modified, deleted; respecting `.gitignore`) and
/// commit with `message`. Returns `Some(oid)` if a commit was created, or
/// `None` if the working tree was already clean.
///
/// Equivalent to `git add -A && git commit -m "<message>"`.
pub fn commit_all(repo: &Repository, message: &str) -> Result<Option<Oid>> {
    let mut index = repo.index()?;
    index.add_all(["*"].iter(), IndexAddOption::DEFAULT, None)?;
    index.update_all(["*"].iter(), None)?;
    index.write()?;

    let tree_oid = index.write_tree()?;

    let parent_commit = match repo.head() {
        Ok(head) => Some(head.peel_to_commit()?),
        Err(e) if e.code() == git2::ErrorCode::UnbornBranch => None,
        Err(e) => return Err(e.into()),
    };

    if let Some(parent) = parent_commit.as_ref()
        && parent.tree_id() == tree_oid
    {
        return Ok(None);
    }

    let tree = repo.find_tree(tree_oid)?;
    let sig = repo.signature()?;
    let parents: Vec<&git2::Commit> = parent_commit.iter().collect();
    let oid = repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)?;
    Ok(Some(oid))
}

/// Push the current branch to `origin`, same name on both sides.
pub fn push(repo: &Repository) -> Result<PushOutcome> {
    let head = repo.head()?;
    if !head.is_branch() {
        return Err(Error::Config("HEAD is detached".to_string()));
    }
    let branch_name = head
        .shorthand()
        .ok_or_else(|| Error::Config("HEAD has no shorthand name".to_string()))?
        .to_string();
    let workdir = repo
        .workdir()
        .ok_or_else(|| Error::Config("repository has no working directory".to_string()))?;

    let refspec = format!("refs/heads/{branch_name}:refs/heads/{branch_name}");
    let output = git_command()
        .arg("-C")
        .arg(workdir)
        .args(["push", "--porcelain", "origin"])
        .arg(&refspec)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::Config("`git` command not found — install git to use polydot".to_string())
            } else {
                Error::Config(format!("invoking git for push: {e}"))
            }
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    // `git push --porcelain` emits one line per ref; `!` flag = rejected.
    // Format: <flag>\t<from>:<to>\t<summary> (<reason>)
    if let Some(reason) = parse_push_rejection(&stdout) {
        return Ok(PushOutcome::Rejected(reason));
    }

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Config(format!(
            "git push failed (exit {}): {}",
            output.status.code().unwrap_or(-1),
            stderr.trim()
        )));
    }

    Ok(PushOutcome::Pushed)
}

/// Scan `git push --porcelain` output for a rejected ref. Returns the
/// per-ref reason if any line is `!`-flagged.
fn parse_push_rejection(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix('!') {
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// Rebase the current branch onto its configured upstream, in-tree.
///
/// Caller is responsible for running [`fetch`] first if fresh upstream state
/// matters — this function works purely against what's already in the repo's
/// refs. On conflict the rebase is aborted and the repo is restored to its
/// pre-rebase state (HEAD, index, and working tree all rolled back).
///
/// Refuses to run with a dirty working tree — libgit2 would overwrite the
/// user's uncommitted work during checkout.
pub fn rebase_onto_upstream(repo: &Repository) -> Result<RebaseOutcome> {
    let head = repo.head()?;
    if !head.is_branch() {
        return Err(Error::Config("HEAD is detached".to_string()));
    }
    let branch_name = head
        .shorthand()
        .ok_or_else(|| Error::Config("HEAD has no shorthand name".to_string()))?
        .to_string();

    let local_branch = repo.find_branch(&branch_name, BranchType::Local)?;
    let upstream = match local_branch.upstream() {
        Ok(u) => u,
        Err(e) if e.code() == git2::ErrorCode::NotFound => {
            return Err(Error::Config(format!(
                "branch `{branch_name}` has no upstream configured"
            )));
        }
        Err(e) => return Err(e.into()),
    };

    let local_oid = head
        .target()
        .ok_or_else(|| Error::Config(format!("branch `{branch_name}` has no target commit")))?;
    let upstream_oid = upstream
        .get()
        .target()
        .ok_or_else(|| Error::Config("upstream has no target commit".to_string()))?;

    let (ahead, behind) = repo.graph_ahead_behind(local_oid, upstream_oid)?;
    if ahead == 0 || behind == 0 {
        // Nothing to replay (already at / ahead of upstream) or strictly
        // behind (caller should fast-forward instead). Either way, not our job.
        return Ok(RebaseOutcome::NothingToDo);
    }

    if is_dirty(repo)? {
        return Err(Error::Config(
            "working tree has uncommitted changes — cannot rebase".to_string(),
        ));
    }

    // reference_to_annotated_commit (vs find_annotated_commit) preserves the
    // ref name. Without it, libgit2's rebase_finish leaves HEAD detached
    // because it doesn't know which branch to move to the new commit.
    let local_ref = local_branch.into_reference();
    let local_annotated = repo.reference_to_annotated_commit(&local_ref)?;
    let upstream_annotated = repo.find_annotated_commit(upstream_oid)?;

    let mut rebase = repo.rebase(
        Some(&local_annotated),
        Some(&upstream_annotated),
        None,
        None,
    )?;
    let sig = repo.signature()?;

    loop {
        let step = match rebase.next() {
            None => break,
            Some(Ok(op)) => op,
            Some(Err(e)) => {
                let _ = rebase.abort();
                return Err(e.into());
            }
        };
        let _ = step;
        let conflicted = collect_conflicted_paths(repo)?;
        if !conflicted.is_empty() {
            let _ = rebase.abort();
            return Ok(RebaseOutcome::ConflictsAborted(conflicted));
        }
        rebase.commit(None, &sig, None)?;
    }
    rebase.finish(Some(&sig))?;
    Ok(RebaseOutcome::Completed)
}

/// Paths libgit2 currently flags as conflicted in the repo index. Used only
/// during rebase to decide whether to abort; empty list = clean apply.
fn collect_conflicted_paths(repo: &Repository) -> Result<Vec<String>> {
    let index = repo.index()?;
    if !index.has_conflicts() {
        return Ok(Vec::new());
    }
    let mut paths: Vec<String> = Vec::new();
    for entry in index.conflicts()?.flatten() {
        let raw = entry
            .our
            .as_ref()
            .or(entry.their.as_ref())
            .or(entry.ancestor.as_ref());
        if let Some(e) = raw
            && let Ok(s) = std::str::from_utf8(&e.path)
        {
            paths.push(s.to_string());
        }
    }
    Ok(paths)
}

pub fn status(repo: &Repository) -> Result<GitStatus> {
    let dirty = is_dirty(repo)?;
    let (branch, upstream, ahead_behind) = head_tracking(repo)?;
    Ok(GitStatus {
        dirty,
        branch,
        upstream,
        ahead_behind,
    })
}

fn is_dirty(repo: &Repository) -> Result<bool> {
    let mut opts = StatusOptions::new();
    opts.include_untracked(true)
        .include_ignored(false)
        .exclude_submodules(true);
    let statuses = repo.statuses(Some(&mut opts))?;
    Ok(!statuses.is_empty())
}

/// `(ahead, behind)` commit counts of local relative to its upstream.
type AheadBehind = (usize, usize);

/// Returns `(branch_name, upstream_name, ahead_behind)`.
///
/// All three are `None` for a detached HEAD. `upstream` and `ahead_behind`
/// are `None` if the local branch has no configured upstream — common for
/// freshly-created branches that haven't been pushed yet.
fn head_tracking(
    repo: &Repository,
) -> Result<(Option<String>, Option<String>, Option<AheadBehind>)> {
    let head = match repo.head() {
        Ok(h) => h,
        // Unborn branch (no commits yet) — treat like detached for display.
        Err(e) if e.code() == git2::ErrorCode::UnbornBranch => return Ok((None, None, None)),
        Err(e) => return Err(e.into()),
    };
    if !head.is_branch() {
        return Ok((None, None, None));
    }
    let Some(branch_name) = head.shorthand().map(str::to_string) else {
        return Ok((None, None, None));
    };

    let local = repo.find_branch(&branch_name, BranchType::Local)?;
    let upstream_branch = match local.upstream() {
        Ok(u) => u,
        Err(e) if e.code() == git2::ErrorCode::NotFound => {
            return Ok((Some(branch_name), None, None));
        }
        Err(e) => return Err(e.into()),
    };

    let upstream_name = upstream_branch
        .name()?
        .map(str::to_string)
        .unwrap_or_else(|| "<invalid utf-8>".to_string());

    let local_oid = head
        .target()
        .ok_or_else(|| Error::Config(format!("branch `{branch_name}` has no target commit")))?;
    let upstream_oid = upstream_branch
        .get()
        .target()
        .ok_or_else(|| Error::Config(format!("upstream `{upstream_name}` has no target commit")))?;
    let ahead_behind = repo.graph_ahead_behind(local_oid, upstream_oid)?;

    Ok((Some(branch_name), Some(upstream_name), Some(ahead_behind)))
}

// Cross-module test helpers. Exposed so the command-module test suites can
// initialize git repos with a predictable default branch — CI runners set
// `init.defaultBranch = master`, which breaks tests that assume `main`.
#[cfg(test)]
pub(crate) mod test_support {
    use git2::{Repository, RepositoryInitOptions};
    use std::path::Path;

    pub fn init(path: &Path) -> Repository {
        let mut opts = RepositoryInitOptions::new();
        opts.initial_head("main");
        Repository::init_opts(path, &opts).unwrap()
    }

    pub fn init_bare(path: &Path) -> Repository {
        let mut opts = RepositoryInitOptions::new();
        opts.bare(true).initial_head("main");
        Repository::init_opts(path, &opts).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// Creates a bare "remote" and a clone of it with one initial commit on `main`.
    /// Returns `(workdir_temp, work_path)` where the clone lives.
    fn fixture_repo() -> (TempDir, TempDir, PathBuf) {
        let remote_dir = TempDir::new().unwrap();
        let work_dir = TempDir::new().unwrap();

        // Init bare remote.
        let remote = super::test_support::init_bare(remote_dir.path());
        // Create local with an initial commit, then push to the bare remote.
        let local = super::test_support::init(work_dir.path());
        // Configure identity locally so commits don't depend on global config.
        let mut cfg = local.config().unwrap();
        cfg.set_str("user.name", "Test").unwrap();
        cfg.set_str("user.email", "test@example.com").unwrap();

        // Initial commit.
        fs::write(work_dir.path().join("README.md"), "hi\n").unwrap();
        let mut index = local.index().unwrap();
        index.add_path(Path::new("README.md")).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = local.find_tree(tree_oid).unwrap();
        let sig = local.signature().unwrap();
        local
            .commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();

        // Rename default branch to main if needed. git2 init defaults vary.
        let head_name = local.head().unwrap().shorthand().unwrap().to_string();
        if head_name != "main" {
            let head_commit = local
                .find_commit(local.head().unwrap().target().unwrap())
                .unwrap();
            local.branch("main", &head_commit, true).unwrap();
            local.set_head("refs/heads/main").unwrap();
        }

        // Wire `origin` to the bare remote, push main, set upstream.
        local
            .remote("origin", &format!("file://{}", remote.path().display()))
            .unwrap();
        let mut remote_handle = local.find_remote("origin").unwrap();
        remote_handle
            .push(&["refs/heads/main:refs/heads/main"], None)
            .unwrap();
        let mut local_branch = local.find_branch("main", BranchType::Local).unwrap();
        local_branch.set_upstream(Some("origin/main")).unwrap();

        let work_path = work_dir.path().to_path_buf();
        (remote_dir, work_dir, work_path)
    }

    #[test]
    fn clean_repo_reports_clean_with_zero_ahead_behind() {
        let (_remote, _work, path) = fixture_repo();
        let repo = open(&path).unwrap();
        let s = status(&repo).unwrap();
        assert!(!s.dirty, "expected clean working tree");
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!(s.upstream.as_deref(), Some("origin/main"));
        assert_eq!(s.ahead_behind, Some((0, 0)));
    }

    #[test]
    fn untracked_file_makes_repo_dirty() {
        let (_remote, _work, path) = fixture_repo();
        fs::write(path.join("untracked.txt"), "x").unwrap();
        let repo = open(&path).unwrap();
        assert!(status(&repo).unwrap().dirty);
    }

    #[test]
    fn modified_tracked_file_makes_repo_dirty() {
        let (_remote, _work, path) = fixture_repo();
        fs::write(path.join("README.md"), "changed\n").unwrap();
        let repo = open(&path).unwrap();
        assert!(status(&repo).unwrap().dirty);
    }

    #[test]
    fn local_commit_shows_one_ahead() {
        let (_remote, _work, path) = fixture_repo();
        let repo = open(&path).unwrap();
        // Make a second commit locally only.
        fs::write(path.join("two.md"), "second\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("two.md")).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = repo.signature().unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "second", &tree, &[&parent])
            .unwrap();

        let s = status(&repo).unwrap();
        assert_eq!(s.ahead_behind, Some((1, 0)));
    }

    fn commit_file(repo: &Repository, filename: &str, content: &str, message: &str) {
        let workdir = repo.workdir().unwrap();
        fs::write(workdir.join(filename), content).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(filename)).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = repo.signature().unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[&parent])
            .unwrap();
    }

    fn clone_again(remote: &TempDir) -> (TempDir, PathBuf, Repository) {
        let work = TempDir::new().unwrap();
        let path = work.path().to_path_buf();
        let url = format!("file://{}", remote.path().display());
        let repo = Repository::clone(&url, &path).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "Test").unwrap();
        cfg.set_str("user.email", "test@example.com").unwrap();
        (work, path, repo)
    }

    fn push_main(repo: &Repository) {
        let mut remote = repo.find_remote("origin").unwrap();
        remote
            .push(&["refs/heads/main:refs/heads/main"], None)
            .unwrap();
    }

    #[test]
    fn clone_creates_repo_at_destination() {
        let (remote, _orig, _orig_path) = fixture_repo();
        let dest_dir = TempDir::new().unwrap();
        let dest = dest_dir.path().join("cloned");
        let url = format!("file://{}", remote.path().display());

        let cloned = clone(&url, &dest).unwrap();
        assert!(cloned.workdir().unwrap().exists());
        assert!(cloned.head().unwrap().target().is_some());
    }

    #[test]
    fn fetch_brings_in_remote_commits_without_advancing_local() {
        let (remote, _, _) = fixture_repo();
        let (_a, _a_path, a_repo) = clone_again(&remote);
        commit_file(&a_repo, "hello.txt", "hi", "from A");
        push_main(&a_repo);

        let (_b, _b_path, b_repo) = clone_again(&remote);
        // Need to clone B *before* A pushes, otherwise B already has A's commit.
        // Re-do with correct ordering:
        drop(b_repo);
        let local_before = TempDir::new().unwrap();
        // Sequence: B clones first, then A pushes a new commit, then B fetches.
        // (The test above already demonstrated A's push reaches the bare remote.)
        // We rebuild B by cloning fresh — but now it'll see A's commit. So let's
        // construct a "stale" B by cloning before A pushes. Easiest: do it all in order.
        // Skip re-using remote; build a fresh scenario.
        let _ = local_before;

        // Fresh scenario:
        let (remote2, _, _) = fixture_repo();
        let (_b2, _b2_path, b2_repo) = clone_again(&remote2);
        let (_a2, _a2_path, a2_repo) = clone_again(&remote2);
        commit_file(&a2_repo, "later.txt", "x", "later commit");
        push_main(&a2_repo);

        // Before fetch, B's view of origin/main is still at the original commit.
        let local_before_fetch = b2_repo.head().unwrap().target().unwrap();
        let upstream_before = b2_repo
            .find_branch("origin/main", BranchType::Remote)
            .unwrap()
            .get()
            .target()
            .unwrap();
        assert_eq!(local_before_fetch, upstream_before);

        fetch(&b2_repo).unwrap();

        let upstream_after = b2_repo
            .find_branch("origin/main", BranchType::Remote)
            .unwrap()
            .get()
            .target()
            .unwrap();
        let local_after = b2_repo.head().unwrap().target().unwrap();
        assert_ne!(
            upstream_after, upstream_before,
            "fetch should move origin/main"
        );
        assert_eq!(
            local_after, local_before_fetch,
            "local should not move on fetch"
        );
    }

    #[test]
    fn fast_forward_advances_when_only_upstream_has_commits() {
        let (remote, _, _) = fixture_repo();
        let (_b, b_path, _b_repo_drop) = clone_again(&remote);
        let (_a, _a_path, a_repo) = clone_again(&remote);
        commit_file(&a_repo, "later.txt", "x", "later");
        push_main(&a_repo);

        let b_repo = open(&b_path).unwrap();
        fetch(&b_repo).unwrap();
        let outcome = try_fast_forward(&b_repo).unwrap();
        assert_eq!(outcome, FastForward::Advanced);

        let local = b_repo.head().unwrap().target().unwrap();
        let upstream = b_repo
            .find_branch("origin/main", BranchType::Remote)
            .unwrap()
            .get()
            .target()
            .unwrap();
        assert_eq!(local, upstream);
        assert!(b_path.join("later.txt").exists());
    }

    #[test]
    fn fast_forward_already_up_to_date_when_local_equals_upstream() {
        let (_remote, _work, path) = fixture_repo();
        let repo = open(&path).unwrap();
        let outcome = try_fast_forward(&repo).unwrap();
        assert_eq!(outcome, FastForward::AlreadyUpToDate);
    }

    #[test]
    fn fast_forward_already_up_to_date_when_local_strictly_ahead() {
        let (_remote, _work, path) = fixture_repo();
        let repo = open(&path).unwrap();
        commit_file(&repo, "local-only.txt", "x", "local only");
        let outcome = try_fast_forward(&repo).unwrap();
        assert_eq!(outcome, FastForward::AlreadyUpToDate);
    }

    #[test]
    fn fast_forward_diverged_when_local_and_upstream_both_advance() {
        let (remote, _, _) = fixture_repo();
        let (_b, b_path, _b_repo_drop) = clone_again(&remote);
        let (_a, _a_path, a_repo) = clone_again(&remote);
        commit_file(&a_repo, "from-a.txt", "a", "from A");
        push_main(&a_repo);

        let b_repo = open(&b_path).unwrap();
        commit_file(&b_repo, "from-b.txt", "b", "from B");
        fetch(&b_repo).unwrap();

        let outcome = try_fast_forward(&b_repo).unwrap();
        assert_eq!(outcome, FastForward::Diverged);
    }

    #[test]
    fn fast_forward_diverged_when_tree_is_dirty() {
        let (remote, _, _) = fixture_repo();
        let (_b, b_path, _b_repo_drop) = clone_again(&remote);
        let (_a, _a_path, a_repo) = clone_again(&remote);
        commit_file(&a_repo, "from-a.txt", "a", "from A");
        push_main(&a_repo);

        fs::write(b_path.join("dirty.txt"), "uncommitted").unwrap();

        let b_repo = open(&b_path).unwrap();
        fetch(&b_repo).unwrap();
        let outcome = try_fast_forward(&b_repo).unwrap();
        assert_eq!(outcome, FastForward::Diverged);
    }

    #[test]
    fn push_succeeds_for_fast_forward() {
        let (remote, _, _) = fixture_repo();
        let (_a, _a_path, a_repo) = clone_again(&remote);
        commit_file(&a_repo, "hello.txt", "hi", "from A");
        let outcome = push(&a_repo).unwrap();
        assert_eq!(outcome, PushOutcome::Pushed);
    }

    #[test]
    fn push_returns_rejected_on_non_fast_forward() {
        let (remote, _, _) = fixture_repo();
        let (_b, _b_path, b_repo) = clone_again(&remote);
        let (_a, _a_path, a_repo) = clone_again(&remote);
        commit_file(&a_repo, "from-a.txt", "a", "from A");
        push_main(&a_repo);

        commit_file(&b_repo, "from-b.txt", "b", "from B");
        let outcome = push(&b_repo).unwrap();
        assert!(
            matches!(outcome, PushOutcome::Rejected(_)),
            "got {outcome:?}"
        );
    }

    #[test]
    fn rebase_replays_local_commits_when_both_sides_advance() {
        let (remote, _, _) = fixture_repo();
        let (_b, b_path, _b_drop) = clone_again(&remote);
        let (_a, _a_path, a_repo) = clone_again(&remote);
        commit_file(&a_repo, "from-a.txt", "a", "from A");
        push_main(&a_repo);

        let b_repo = open(&b_path).unwrap();
        commit_file(&b_repo, "from-b.txt", "b", "from B");
        fetch(&b_repo).unwrap();

        let outcome = rebase_onto_upstream(&b_repo).unwrap();
        assert_eq!(outcome, RebaseOutcome::Completed);

        // Both A's and B's commits are present; HEAD sits on top of A.
        assert!(b_path.join("from-a.txt").exists(), "A's commit replayed");
        assert!(b_path.join("from-b.txt").exists(), "B's commit preserved");

        let s = status(&b_repo).unwrap();
        assert_eq!(
            s.ahead_behind,
            Some((1, 0)),
            "local is one ahead of upstream"
        );
        assert!(!s.dirty);
    }

    #[test]
    fn rebase_nothing_to_do_when_local_matches_upstream() {
        let (_remote, _work, path) = fixture_repo();
        let repo = open(&path).unwrap();
        let outcome = rebase_onto_upstream(&repo).unwrap();
        assert_eq!(outcome, RebaseOutcome::NothingToDo);
    }

    #[test]
    fn rebase_nothing_to_do_when_local_strictly_ahead() {
        let (_remote, _work, path) = fixture_repo();
        let repo = open(&path).unwrap();
        commit_file(&repo, "local-only.txt", "x", "local only");
        let outcome = rebase_onto_upstream(&repo).unwrap();
        assert_eq!(outcome, RebaseOutcome::NothingToDo);
    }

    #[test]
    fn rebase_aborts_and_restores_repo_on_conflict() {
        // A and B both edit the same file; B's replay onto A's commit hits a
        // conflict. Rebase should abort and leave B pointing at its pre-rebase HEAD.
        let (remote, _, _) = fixture_repo();
        let (_b, b_path, _b_drop) = clone_again(&remote);
        let (_a, _a_path, a_repo) = clone_again(&remote);
        commit_file(&a_repo, "README.md", "a\nwins\n", "A version");
        push_main(&a_repo);

        let b_repo = open(&b_path).unwrap();
        commit_file(&b_repo, "README.md", "b\nwins\n", "B version");
        let head_before = b_repo.head().unwrap().target().unwrap();
        fetch(&b_repo).unwrap();

        let outcome = rebase_onto_upstream(&b_repo).unwrap();
        match outcome {
            RebaseOutcome::ConflictsAborted(paths) => {
                assert!(
                    paths.iter().any(|p| p == "README.md"),
                    "expected README.md in conflict list, got {paths:?}",
                );
            }
            other => panic!("expected ConflictsAborted, got {other:?}"),
        }

        // Pre-rebase state restored.
        let head_after = b_repo.head().unwrap().target().unwrap();
        assert_eq!(head_after, head_before, "HEAD rolled back after abort");
        assert!(!status(&b_repo).unwrap().dirty, "tree is clean after abort");
    }

    #[test]
    fn rebase_refuses_when_working_tree_is_dirty() {
        let (remote, _, _) = fixture_repo();
        let (_b, b_path, _b_drop) = clone_again(&remote);
        let (_a, _a_path, a_repo) = clone_again(&remote);
        commit_file(&a_repo, "later.txt", "x", "later");
        push_main(&a_repo);

        let b_repo = open(&b_path).unwrap();
        // Local commit so ahead>0, plus a dirty file so the refusal path fires.
        commit_file(&b_repo, "b-commit.txt", "b", "b commit");
        fs::write(b_path.join("dirty.txt"), "uncommitted").unwrap();
        fetch(&b_repo).unwrap();

        let err = rebase_onto_upstream(&b_repo).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, Error::Config(_)));
        assert!(msg.contains("uncommitted"), "got: {msg}");
    }

    #[test]
    fn commit_all_stages_untracked_and_creates_commit() {
        let (_remote, _work, path) = fixture_repo();
        let repo = open(&path).unwrap();
        let head_before = repo.head().unwrap().target().unwrap();

        fs::write(path.join("new.txt"), "fresh\n").unwrap();
        let oid = commit_all(&repo, "add new").unwrap().unwrap();

        let head_after = repo.head().unwrap().target().unwrap();
        assert_eq!(head_after, oid);
        assert_ne!(head_after, head_before);
        assert!(!status(&repo).unwrap().dirty);
    }

    #[test]
    fn commit_all_picks_up_modifications_and_deletions() {
        let (_remote, _work, path) = fixture_repo();
        let repo = open(&path).unwrap();

        // Modify README, delete nothing yet, add a file then delete it next commit.
        fs::write(path.join("README.md"), "changed\n").unwrap();
        fs::write(path.join("delete-me.txt"), "x").unwrap();
        commit_all(&repo, "stage initial").unwrap().unwrap();

        // Now delete the file and modify README again.
        fs::remove_file(path.join("delete-me.txt")).unwrap();
        fs::write(path.join("README.md"), "changed twice\n").unwrap();
        let oid = commit_all(&repo, "delete + modify").unwrap().unwrap();

        let commit = repo.find_commit(oid).unwrap();
        let tree = commit.tree().unwrap();
        assert!(tree.get_name("README.md").is_some());
        assert!(tree.get_name("delete-me.txt").is_none());
        assert!(!status(&repo).unwrap().dirty);
    }

    #[test]
    fn commit_all_returns_none_when_tree_is_clean() {
        let (_remote, _work, path) = fixture_repo();
        let repo = open(&path).unwrap();
        let head_before = repo.head().unwrap().target().unwrap();

        let result = commit_all(&repo, "no changes").unwrap();
        assert!(result.is_none());
        let head_after = repo.head().unwrap().target().unwrap();
        assert_eq!(head_after, head_before);
    }

    #[test]
    fn commit_all_respects_gitignore() {
        let (_remote, _work, path) = fixture_repo();
        let repo = open(&path).unwrap();

        fs::write(path.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(path.join("ignored.txt"), "x").unwrap();
        fs::write(path.join("kept.txt"), "y").unwrap();

        let oid = commit_all(&repo, "add files").unwrap().unwrap();
        let tree = repo.find_commit(oid).unwrap().tree().unwrap();
        assert!(tree.get_name(".gitignore").is_some());
        assert!(tree.get_name("kept.txt").is_some());
        assert!(tree.get_name("ignored.txt").is_none());
    }

    #[test]
    fn diff_summary_is_zeroed_for_clean_tree() {
        let (_remote, _work, path) = fixture_repo();
        let repo = open(&path).unwrap();
        let summary = diff_summary(&repo).unwrap().unwrap();
        assert_eq!(summary.files_changed, 0);
        assert_eq!(summary.insertions, 0);
        assert_eq!(summary.deletions, 0);
        assert!(summary.formatted.is_empty());
    }

    #[test]
    fn diff_summary_counts_modifications_and_untracked() {
        let (_remote, _work, path) = fixture_repo();
        let repo = open(&path).unwrap();
        // Modify tracked file (one insertion + one deletion vs original).
        fs::write(path.join("README.md"), "changed\nadded\n").unwrap();
        // Add an untracked file.
        fs::write(path.join("new.txt"), "fresh\n").unwrap();
        let summary = diff_summary(&repo).unwrap().unwrap();
        assert_eq!(summary.files_changed, 2);
        assert!(summary.insertions >= 1);
        assert!(summary.deletions >= 1);
        assert!(summary.formatted.contains("README.md"));
        assert!(summary.formatted.contains("new.txt"));
    }

    #[test]
    fn diff_summary_respects_gitignore() {
        let (_remote, _work, path) = fixture_repo();
        let repo = open(&path).unwrap();
        fs::write(path.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(path.join("ignored.txt"), "x").unwrap();
        let summary = diff_summary(&repo).unwrap().unwrap();
        // .gitignore appears (untracked) but ignored.txt does not.
        assert!(summary.formatted.contains(".gitignore"));
        assert!(!summary.formatted.contains("ignored.txt"));
    }

    #[test]
    fn print_diff_runs_without_error_on_dirty_tree() {
        let (_remote, _work, path) = fixture_repo();
        let repo = open(&path).unwrap();
        fs::write(path.join("README.md"), "changed\n").unwrap();
        // Just ensure it doesn't error; we don't capture stdout in unit tests.
        print_diff(&repo).unwrap();
    }

    #[test]
    fn ensure_origin_speakable_accepts_file_origin() {
        let (_remote, _work, path) = fixture_repo();
        let repo = open(&path).unwrap();
        ensure_origin_speakable(&repo, "https://example.com/x.git").unwrap();
    }

    #[test]
    fn ensure_origin_speakable_accepts_https_origin() {
        let (_remote, _work, path) = fixture_repo();
        let repo = open(&path).unwrap();
        repo.remote_set_url("origin", "https://example.com/x.git")
            .unwrap();
        ensure_origin_speakable(&repo, "https://example.com/x.git").unwrap();
    }

    #[test]
    fn ensure_origin_speakable_rejects_ssh_scp_style() {
        let (_remote, _work, path) = fixture_repo();
        let repo = open(&path).unwrap();
        repo.remote_set_url("origin", "git@github.com:owner/repo.git")
            .unwrap();
        let err = ensure_origin_speakable(&repo, "https://github.com/owner/repo.git").unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, Error::Config(_)));
        assert!(msg.contains("git@github.com:owner/repo.git"));
        assert!(msg.contains("https://"));
        assert!(msg.contains("file://"));
        assert!(msg.contains("config.toml expects: https://github.com/owner/repo.git"));
        assert!(msg.contains("remote set-url origin https://github.com/owner/repo.git"));
    }

    #[test]
    fn ensure_origin_speakable_rejects_ssh_scheme() {
        let (_remote, _work, path) = fixture_repo();
        let repo = open(&path).unwrap();
        repo.remote_set_url("origin", "ssh://git@github.com/owner/repo.git")
            .unwrap();
        let err = ensure_origin_speakable(&repo, "https://github.com/owner/repo.git").unwrap_err();
        assert!(matches!(err, Error::Config(_)));
    }

    #[test]
    fn ensure_origin_speakable_rejects_plain_http() {
        let (_remote, _work, path) = fixture_repo();
        let repo = open(&path).unwrap();
        repo.remote_set_url("origin", "http://example.com/x.git")
            .unwrap();
        let err = ensure_origin_speakable(&repo, "https://example.com/x.git").unwrap_err();
        assert!(matches!(err, Error::Config(_)));
    }

    #[test]
    fn no_upstream_yields_none_ahead_behind() {
        // Build a local-only repo with no remote.
        let work = TempDir::new().unwrap();
        let repo = super::test_support::init(work.path());
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "Test").unwrap();
        cfg.set_str("user.email", "test@example.com").unwrap();
        fs::write(work.path().join("a.txt"), "a").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("a.txt")).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = repo.signature().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();

        let s = status(&repo).unwrap();
        assert!(s.branch.is_some());
        assert_eq!(s.upstream, None);
        assert_eq!(s.ahead_behind, None);
    }
}
