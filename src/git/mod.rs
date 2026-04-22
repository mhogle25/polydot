// Git operations layer (wraps git2).
//
// Phase 2 added read-only ops (open, status, ahead/behind).
// Phase 4 adds network ops over HTTPS only (clone, fetch, fast-forward, push).
// Authentication is HTTPS basic-auth with PAT — see `crate::credentials`.

use std::cell::RefCell;
use std::path::Path;

use git2::build::{CheckoutBuilder, RepoBuilder};
use git2::{
    BranchType, Cred, CredentialType, FetchOptions, IndexAddOption, Oid, PushOptions,
    RemoteCallbacks, Repository, StatusOptions,
};

use crate::credentials::Credentials;
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

pub fn open(repo_path: &Path) -> Result<Repository> {
    Repository::open(repo_path).map_err(Error::from)
}

/// Clone `url` into `dest`. The clone path's parent must exist.
pub fn clone(url: &str, dest: &Path, creds: &Credentials) -> Result<Repository> {
    let mut fo = FetchOptions::new();
    fo.remote_callbacks(make_remote_callbacks(creds));
    let mut builder = RepoBuilder::new();
    builder.fetch_options(fo);
    builder.clone(url, dest).map_err(Error::from)
}

/// Fetch from `origin`, no merge.
pub fn fetch(repo: &Repository, creds: &Credentials) -> Result<()> {
    let mut remote = repo.find_remote("origin")?;
    let refspecs: Vec<String> = remote
        .fetch_refspecs()?
        .iter()
        .filter_map(|s| s.map(String::from))
        .collect();
    let mut fo = FetchOptions::new();
    fo.remote_callbacks(make_remote_callbacks(creds));
    remote.fetch(&refspecs, Some(&mut fo), None)?;
    Ok(())
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
pub fn push(repo: &Repository, creds: &Credentials) -> Result<PushOutcome> {
    let head = repo.head()?;
    if !head.is_branch() {
        return Err(Error::Config("HEAD is detached".to_string()));
    }
    let branch_name = head
        .shorthand()
        .ok_or_else(|| Error::Config("HEAD has no shorthand name".to_string()))?
        .to_string();

    let rejection: RefCell<Option<String>> = RefCell::new(None);
    {
        let mut remote = repo.find_remote("origin")?;
        let mut callbacks = make_remote_callbacks(creds);
        callbacks.push_update_reference(|refname, status| {
            if let Some(reason) = status {
                *rejection.borrow_mut() = Some(format!("{refname}: {reason}"));
            }
            Ok(())
        });
        let mut po = PushOptions::new();
        po.remote_callbacks(callbacks);
        let refspec = format!("refs/heads/{branch_name}:refs/heads/{branch_name}");
        match remote.push(&[&refspec], Some(&mut po)) {
            Ok(()) => {}
            Err(e) if e.code() == git2::ErrorCode::NotFastForward => {
                return Ok(PushOutcome::Rejected(e.message().to_string()));
            }
            Err(e) => return Err(e.into()),
        }
    }

    Ok(match rejection.into_inner() {
        Some(reason) => PushOutcome::Rejected(reason),
        None => PushOutcome::Pushed,
    })
}

fn make_remote_callbacks<'a>(creds: &'a Credentials) -> RemoteCallbacks<'a> {
    let mut cb = RemoteCallbacks::new();
    cb.credentials(move |url, _username_from_url, allowed_types| {
        let host = extract_host(url).ok_or_else(|| {
            git2::Error::from_str(&format!("could not extract host from URL: {url}"))
        })?;
        let host_creds = creds.for_host(host).ok_or_else(|| {
            git2::Error::from_str(&format!("no credentials configured for host `{host}`"))
        })?;
        if allowed_types.contains(CredentialType::USER_PASS_PLAINTEXT) {
            Cred::userpass_plaintext(&host_creds.username, &host_creds.token)
        } else {
            Err(git2::Error::from_str(&format!(
                "unsupported credential type {allowed_types:?} for host `{host}`"
            )))
        }
    });
    cb
}

fn extract_host(url: &str) -> Option<&str> {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let after_user = after_scheme
        .rsplit_once('@')
        .map_or(after_scheme, |(_, rest)| rest);
    let host_with_maybe_port = after_user.split('/').next()?;
    let host = host_with_maybe_port.split(':').next()?;
    if host.is_empty() { None } else { Some(host) }
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
        let remote = Repository::init_bare(remote_dir.path()).unwrap();
        // Create local with an initial commit, then push to the bare remote.
        let local = Repository::init(work_dir.path()).unwrap();
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
    fn extract_host_basic_https() {
        assert_eq!(
            extract_host("https://github.com/foo/bar.git"),
            Some("github.com")
        );
    }

    #[test]
    fn extract_host_strips_userinfo() {
        assert_eq!(
            extract_host("https://user:token@github.com/foo/bar.git"),
            Some("github.com")
        );
    }

    #[test]
    fn extract_host_strips_port() {
        assert_eq!(
            extract_host("https://localhost:8080/foo.git"),
            Some("localhost")
        );
    }

    #[test]
    fn extract_host_handles_no_path() {
        assert_eq!(extract_host("https://example.com"), Some("example.com"));
    }

    #[test]
    fn extract_host_returns_none_for_empty_or_malformed() {
        assert_eq!(extract_host(""), None);
        assert_eq!(extract_host("https://"), None);
        assert_eq!(extract_host("https:///path"), None);
    }

    #[test]
    fn clone_creates_repo_at_destination() {
        let (remote, _orig, _orig_path) = fixture_repo();
        let dest_dir = TempDir::new().unwrap();
        let dest = dest_dir.path().join("cloned");
        let url = format!("file://{}", remote.path().display());

        let cloned = clone(&url, &dest, &Credentials::empty()).unwrap();
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

        fetch(&b2_repo, &Credentials::empty()).unwrap();

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
        fetch(&b_repo, &Credentials::empty()).unwrap();
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
        fetch(&b_repo, &Credentials::empty()).unwrap();

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
        fetch(&b_repo, &Credentials::empty()).unwrap();
        let outcome = try_fast_forward(&b_repo).unwrap();
        assert_eq!(outcome, FastForward::Diverged);
    }

    #[test]
    fn push_succeeds_for_fast_forward() {
        let (remote, _, _) = fixture_repo();
        let (_a, _a_path, a_repo) = clone_again(&remote);
        commit_file(&a_repo, "hello.txt", "hi", "from A");
        let outcome = push(&a_repo, &Credentials::empty()).unwrap();
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
        let outcome = push(&b_repo, &Credentials::empty()).unwrap();
        assert!(
            matches!(outcome, PushOutcome::Rejected(_)),
            "got {outcome:?}"
        );
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
    fn no_upstream_yields_none_ahead_behind() {
        // Build a local-only repo with no remote.
        let work = TempDir::new().unwrap();
        let repo = Repository::init(work.path()).unwrap();
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
