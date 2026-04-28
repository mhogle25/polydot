// `polydot commit` — stage + commit dirty changes across all managed repos.
//
// Same mode selection as `save` (shared `-m` or per-repo no-flag). The
// difference is that commit stops after the commit step — nothing is
// pushed, no divergence prompt is raised. Any freshly-made commits sit on
// local `main` until the user runs `push` (or `save` on a later cycle).
//
// Intended for offline workflows or review-before-push patterns. For the
// combined commit+push flow, use `save`.

use crate::commands::save::{
    self, CommitChoice, CommitPhaseOutcome, CommitPromptCtx, Mode, resolve_mode,
};
use crate::config::Config;
use crate::paths::SystemEnv;

#[derive(Debug, Default)]
struct Summary {
    committed: usize,
    nothing_to_commit: usize,
    skipped: usize,
    failed: usize,
}

impl Summary {
    fn print(&self) {
        println!(
            "{} committed, {} nothing-to-commit, {} skipped, {} failed",
            self.committed, self.nothing_to_commit, self.skipped, self.failed,
        );
    }
}

pub fn run(config: &Config, message: Option<&str>) -> anyhow::Result<()> {
    let mode = resolve_mode(message);
    run_with(config, &mode, &mut save::prompt_commit_via_menu)
}

/// Test seam: same as [`run`] but with the commit prompter injected. Matches
/// the shape of `save::run_with` so tests can exercise the per-repo prompt
/// path without a TTY.
pub(crate) fn run_with<C>(
    config: &Config,
    mode: &Mode,
    commit_prompter: &mut C,
) -> anyhow::Result<()>
where
    C: FnMut(&CommitPromptCtx<'_>) -> anyhow::Result<CommitChoice>,
{
    if config.repos.is_empty() {
        println!("(no repos configured)");
        return Ok(());
    }
    let env = SystemEnv;
    let mut summary = Summary::default();
    'outer: for (name, repo_cfg) in &config.repos {
        match save::commit_phase(name, repo_cfg, mode, &env, commit_prompter) {
            Ok(CommitPhaseOutcome::Committed { .. }) => {
                println!("committed           {name}");
                println!();
                summary.committed += 1;
            }
            Ok(CommitPhaseOutcome::NothingToCommit { .. }) => {
                println!("nothing-to-commit   {name}");
                summary.nothing_to_commit += 1;
            }
            Ok(CommitPhaseOutcome::UserSkipped) => {
                summary.skipped += 1;
            }
            Ok(CommitPhaseOutcome::UserAborted) => {
                summary.skipped += 1;
                break 'outer;
            }
            Err(e) => {
                eprintln!("error committing `{name}`: {e:#}");
                summary.failed += 1;
            }
        }
    }
    summary.print();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::save::CommitChoice;
    use crate::config::{Config, RepoConfig};
    use git2::{BranchType, Repository};
    use std::collections::{BTreeMap, VecDeque};
    use std::fs;
    use std::path::{Path, PathBuf};
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
    fn commits_dirty_tree_without_pushing() {
        let (remote, url, _b_dir, b_path, b_repo) = fixture_remote_and_clone();
        fs::write(b_path.join("notes.md"), "fresh\n").unwrap();

        let config = config_with(vec![("r", url, &b_path)]);
        run_with(
            &config,
            &shared("add notes"),
            &mut never_called_commit_prompter(),
        )
        .unwrap();

        // HEAD advanced by exactly one commit.
        let head_msg = b_repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .message()
            .unwrap()
            .to_string();
        assert_eq!(head_msg, "add notes");

        // Remote was NOT touched — commit does not push.
        let (_v_dir, v_path, _v_repo) =
            clone_to_tempdir(&format!("file://{}", remote.path().display()));
        assert!(!v_path.join("notes.md").exists());
    }

    #[test]
    fn clean_tree_is_nothing_to_commit() {
        let (_remote, url, _b_dir, b_path, b_repo) = fixture_remote_and_clone();
        let head_before = b_repo.head().unwrap().target().unwrap();

        let config = config_with(vec![("r", url, &b_path)]);
        run_with(
            &config,
            &shared("should not be used"),
            &mut never_called_commit_prompter(),
        )
        .unwrap();

        // HEAD unchanged — no commit happened.
        let head_after = b_repo.head().unwrap().target().unwrap();
        assert_eq!(head_before, head_after);
    }

    #[test]
    fn per_repo_message_commits_without_pushing() {
        let (remote, url, _b_dir, b_path, b_repo) = fixture_remote_and_clone();
        fs::write(b_path.join("notes.md"), "fresh\n").unwrap();

        let config = config_with(vec![("r", url, &b_path)]);
        run_with(
            &config,
            &Mode::PerRepo,
            &mut scripted_commit_prompter(vec![CommitChoice::Message("interactive".to_string())]),
        )
        .unwrap();

        let head_msg = b_repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .message()
            .unwrap()
            .to_string();
        assert_eq!(head_msg, "interactive");

        let (_v_dir, v_path, _v_repo) =
            clone_to_tempdir(&format!("file://{}", remote.path().display()));
        assert!(!v_path.join("notes.md").exists());
    }

    #[test]
    fn per_repo_skip_leaves_tree_dirty() {
        let (_remote, url, _b_dir, b_path, b_repo) = fixture_remote_and_clone();
        fs::write(b_path.join("untracked.txt"), "x").unwrap();
        let head_before = b_repo.head().unwrap().target().unwrap();

        let config = config_with(vec![("r", url, &b_path)]);
        run_with(
            &config,
            &Mode::PerRepo,
            &mut scripted_commit_prompter(vec![CommitChoice::Skip]),
        )
        .unwrap();

        let head_after = b_repo.head().unwrap().target().unwrap();
        assert_eq!(head_before, head_after);
        assert!(b_path.join("untracked.txt").exists());
    }

    #[test]
    fn per_repo_abort_short_circuits_remaining_repos() {
        let (_r1, url1, _b1_dir, b1_path, b1_repo) = fixture_remote_and_clone();
        fs::write(b1_path.join("a.txt"), "x").unwrap();
        let (_r2, url2, _b2_dir, b2_path, b2_repo) = fixture_remote_and_clone();
        fs::write(b2_path.join("b.txt"), "x").unwrap();
        let b2_head_before = b2_repo.head().unwrap().target().unwrap();

        let config = config_with(vec![("aaa", url1, &b1_path), ("zzz", url2, &b2_path)]);
        run_with(
            &config,
            &Mode::PerRepo,
            &mut scripted_commit_prompter(vec![CommitChoice::Abort]),
        )
        .unwrap();

        // First repo: not committed (user aborted).
        let b1_head = b1_repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(b1_head.message().unwrap(), "init");
        // Second repo: never processed.
        let b2_head_after = b2_repo.head().unwrap().target().unwrap();
        assert_eq!(b2_head_before, b2_head_after);
    }

    #[test]
    fn per_repo_view_then_message_commits() {
        let (_remote, url, _b_dir, b_path, b_repo) = fixture_remote_and_clone();
        fs::write(b_path.join("notes.md"), "fresh\n").unwrap();

        let config = config_with(vec![("r", url, &b_path)]);
        run_with(
            &config,
            &Mode::PerRepo,
            &mut scripted_commit_prompter(vec![
                CommitChoice::View,
                CommitChoice::Message("after view".to_string()),
            ]),
        )
        .unwrap();

        let head_msg = b_repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .message()
            .unwrap()
            .to_string();
        assert_eq!(head_msg, "after view");
    }

    #[test]
    fn per_repo_clean_repo_does_not_prompt() {
        let (_remote, url, _b_dir, b_path, b_repo) = fixture_remote_and_clone();
        let head_before = b_repo.head().unwrap().target().unwrap();

        let config = config_with(vec![("r", url, &b_path)]);
        run_with(&config, &Mode::PerRepo, &mut never_called_commit_prompter()).unwrap();

        // No commit, no prompt invocation.
        let head_after = b_repo.head().unwrap().target().unwrap();
        assert_eq!(head_before, head_after);
    }

    #[test]
    fn missing_clone_path_is_a_per_repo_failure() {
        let bogus = TempDir::new().unwrap().path().join("never-cloned");
        let (_remote, url, _c_dir, c_path, c_repo) = fixture_remote_and_clone();
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
            &mut never_called_commit_prompter(),
        )
        .unwrap();

        // Second repo still committed despite first one failing.
        let head_msg = c_repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .message()
            .unwrap()
            .to_string();
        assert_eq!(head_msg, "add c");
    }

    #[test]
    fn empty_config_is_a_no_op() {
        let config = config_with(vec![]);
        run_with(&config, &shared("msg"), &mut never_called_commit_prompter()).unwrap();
    }

    #[test]
    fn shared_mode_commits_multiple_repos_with_same_message() {
        let (_r1, url1, _b1_dir, b1_path, b1_repo) = fixture_remote_and_clone();
        fs::write(b1_path.join("a.txt"), "x").unwrap();
        let (_r2, url2, _b2_dir, b2_path, b2_repo) = fixture_remote_and_clone();
        fs::write(b2_path.join("b.txt"), "y").unwrap();

        let config = config_with(vec![("aaa", url1, &b1_path), ("zzz", url2, &b2_path)]);
        run_with(
            &config,
            &shared("batch commit"),
            &mut never_called_commit_prompter(),
        )
        .unwrap();

        for repo in [&b1_repo, &b2_repo] {
            let msg = repo
                .head()
                .unwrap()
                .peel_to_commit()
                .unwrap()
                .message()
                .unwrap()
                .to_string();
            assert_eq!(msg, "batch commit");
        }
    }

    #[test]
    fn no_flag_defaults_to_per_repo() {
        let (_remote, url, _b_dir, b_path, b_repo) = fixture_remote_and_clone();
        fs::write(b_path.join("notes.md"), "fresh\n").unwrap();

        let config = config_with(vec![("r", url, &b_path)]);

        run_with(
            &config,
            &Mode::PerRepo,
            &mut scripted_commit_prompter(vec![CommitChoice::Skip]),
        )
        .unwrap();

        // Tree still dirty, no commit made.
        assert!(b_path.join("notes.md").exists());
        let head_msg = b_repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .message()
            .unwrap()
            .to_string();
        assert_eq!(head_msg, "init");
    }
}
