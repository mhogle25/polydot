// Git operations layer (wraps git2).
//
// Phase 2: read-only ops only — open, dirty-check, ahead/behind against
// the local upstream. No fetch, no clone, no commit. Anything that hits
// the network or mutates the repo waits for Phases 4-5.

use std::path::Path;

use git2::{BranchType, Repository, StatusOptions};

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

pub fn open(repo_path: &Path) -> Result<Repository> {
    Repository::open(repo_path).map_err(Error::from)
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
