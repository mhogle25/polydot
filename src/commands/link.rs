// `polydot link` — create / repair symlinks per config.
//
// For each managed link:
//   - Correct       → no-op
//   - Missing       → create the symlink
//   - WrongTarget   → prompt user (overwrite / backup / adopt / skip / quit)
//   - UnmanagedFile → same prompt (plus diff if it's a regular file)
//   - UnmanagedDir  → same prompt (no diff for dirs)
//   - BrokenSource  → dangling symlink — source was deleted or renamed in
//                     the repo. Offer [r]emove / [s]kip / [q]uit only;
//                     overwrite/backup/adopt all fail to rebuild since
//                     there's no source to point at.
//
// At end-of-run, print a one-line summary of created / resolved / skipped
// counts. `quit` aborts the remaining work but exits 0 — the user asked
// to stop, not to fail.

use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::config::{Config, Link, RepoConfig};
use crate::link::{self, Action, ApplyOutcome, LinkState};
use crate::paths::{SystemEnv, evaluate};
use crate::ui::{Menu, MenuOption};

#[derive(Debug, Default)]
struct Summary {
    created: usize,
    resolved: usize,
    already_correct: usize,
    skipped: usize,
}

impl Summary {
    fn print(&self) {
        println!(
            "{} created, {} resolved, {} already correct, {} skipped",
            self.created, self.resolved, self.already_correct, self.skipped
        );
    }
}

pub fn run(config: &Config) -> anyhow::Result<()> {
    run_with(config, &mut prompt_via_menu)
}

/// Snapshot of everything the conflict-resolution prompter needs to make
/// a decision: the classified state plus the paths involved. Lets the
/// prompter (and the menu it builds) reason about which actions are
/// structurally possible — e.g. Adopt requires the in-repo source to be
/// absent, Diff requires the on-disk target to be a regular file.
pub(crate) struct PromptCtx<'a> {
    pub state: &'a LinkState,
    pub expected_source: &'a Path,
    pub to: &'a Path,
}

/// Test seam: same as [`run`] but with the conflict-resolution prompter
/// injected. Production wires it to the interactive menu; tests pass a
/// scripted closure so the prompt path is exercised without needing a TTY.
pub(crate) fn run_with<F>(config: &Config, prompter: &mut F) -> anyhow::Result<()>
where
    F: FnMut(&PromptCtx<'_>) -> anyhow::Result<Choice>,
{
    if config.repos.is_empty() {
        println!("(no repos configured)");
        return Ok(());
    }
    let env = SystemEnv;
    let mut summary = Summary::default();
    'outer: for (name, repo_cfg) in &config.repos {
        let clone_path = resolve_clone_path(name, repo_cfg, &env)?;
        if !clone_path.exists() {
            eprintln!(
                "skipping `{name}`: clone path {} does not exist (run `polydot sync` first)",
                clone_path.display()
            );
            summary.skipped += repo_cfg.links.len();
            continue;
        }
        for link in &repo_cfg.links {
            match process_link(name, &clone_path, link, &env, prompter)? {
                StepOutcome::Created => summary.created += 1,
                StepOutcome::AlreadyCorrect => summary.already_correct += 1,
                StepOutcome::Resolved => summary.resolved += 1,
                StepOutcome::Skipped => summary.skipped += 1,
                StepOutcome::Quit => break 'outer,
            }
        }
    }
    summary.print();
    Ok(())
}

fn prompt_via_menu(ctx: &PromptCtx<'_>) -> anyhow::Result<Choice> {
    Ok(build_menu(ctx).interact()?)
}

#[derive(Debug)]
enum StepOutcome {
    Created,
    AlreadyCorrect,
    Resolved,
    Skipped,
    Quit,
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

fn process_link<F>(
    repo_name: &str,
    clone_path: &Path,
    link_cfg: &Link,
    env: &SystemEnv,
    prompter: &mut F,
) -> anyhow::Result<StepOutcome>
where
    F: FnMut(&PromptCtx<'_>) -> anyhow::Result<Choice>,
{
    let to = PathBuf::from(
        evaluate(&link_cfg.to, env)
            .with_context(|| format!("evaluating link target for `{repo_name}`"))?,
    );
    let expected_source = clone_path.join(&link_cfg.from);
    let state = link::link_state(&expected_source, &to)?;
    match state {
        LinkState::Correct => Ok(StepOutcome::AlreadyCorrect),
        LinkState::Missing => {
            link::create(&expected_source, &to)?;
            println!("created  {} → {}", to.display(), expected_source.display());
            Ok(StepOutcome::Created)
        }
        conflict => prompt_and_resolve(repo_name, &expected_source, &to, &conflict, prompter),
    }
}

fn prompt_and_resolve<F>(
    repo_name: &str,
    expected_source: &Path,
    to: &Path,
    conflict: &LinkState,
    prompter: &mut F,
) -> anyhow::Result<StepOutcome>
where
    F: FnMut(&PromptCtx<'_>) -> anyhow::Result<Choice>,
{
    print_conflict_header(repo_name, expected_source, to, conflict);
    let ctx = PromptCtx {
        state: conflict,
        expected_source,
        to,
    };
    loop {
        let choice = prompter(&ctx)?;
        match choice {
            Choice::Diff => {
                print_diff_unavailable();
                continue;
            }
            Choice::Quit => return Ok(StepOutcome::Quit),
            Choice::Action(action) => {
                let outcome = link::apply(expected_source, to, action)?;
                report_apply(&outcome, expected_source, to);
                return Ok(match outcome {
                    ApplyOutcome::Skipped => StepOutcome::Skipped,
                    _ => StepOutcome::Resolved,
                });
            }
        }
    }
}

fn build_menu_for_broken_source() -> Menu<Choice> {
    let options = vec![
        MenuOption::new(
            'r',
            "[r]emove — delete the dangling symlink",
            Choice::Action(Action::Remove),
        ),
        MenuOption::new(
            's',
            "[s]kip   — leave this one alone for now",
            Choice::Action(Action::Skip),
        ),
        MenuOption::new(
            'q',
            "[q]uit   — stop processing remaining links",
            Choice::Quit,
        ),
    ];
    Menu::new(options)
        .default_shortcut('s')
        .cancel_shortcut('q')
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Choice {
    Action(Action),
    Diff,
    Quit,
}

fn build_menu(ctx: &PromptCtx<'_>) -> Menu<Choice> {
    // Broken-source symlinks get a specialized menu — overwrite/backup/adopt
    // all try to rebuild the symlink, which would just recreate the dangling
    // state with no source to point at.
    if matches!(ctx.state, LinkState::BrokenSource { .. }) {
        return build_menu_for_broken_source();
    }
    // Adopt moves the existing target INTO the repo at `expected_source`.
    // If something already lives there, the move would clobber it — so the
    // option is structurally impossible and shouldn't be offered.
    let adopt_possible = !ctx.expected_source.exists();
    // Diff only makes sense when there's something to compare against the
    // repo's version. Symlinks (WrongTarget) and directories don't qualify.
    let diff_available =
        matches!(ctx.state, LinkState::UnmanagedConflict) && is_regular_file(ctx.to);

    let options = vec![
        MenuOption::new(
            'o',
            "[o]verwrite — remove existing, create symlink",
            Choice::Action(Action::Overwrite),
        ),
        MenuOption::new(
            'b',
            "[b]ackup    — rename existing to <path>.bak, then symlink",
            Choice::Action(Action::Backup),
        ),
        MenuOption::new(
            'a',
            "[a]dopt     — move existing INTO the repo, then symlink",
            Choice::Action(Action::Adopt),
        )
        .enabled(adopt_possible),
        MenuOption::new(
            's',
            "[s]kip      — leave this one alone for now",
            Choice::Action(Action::Skip),
        ),
        MenuOption::new(
            'd',
            "[d]iff      — show difference between existing and repo content",
            Choice::Diff,
        )
        .enabled(diff_available),
        MenuOption::new(
            'q',
            "[q]uit      — stop processing remaining links",
            Choice::Quit,
        ),
    ];
    Menu::new(options)
        .default_shortcut('s')
        .cancel_shortcut('q')
}

fn is_regular_file(path: &Path) -> bool {
    matches!(std::fs::symlink_metadata(path), Ok(m) if m.is_file())
}

fn print_conflict_header(repo_name: &str, expected_source: &Path, to: &Path, state: &LinkState) {
    let what = match state {
        LinkState::WrongTarget { actual } => {
            format!("is a symlink → {}", actual.display())
        }
        LinkState::BrokenSource { source } => {
            format!("is a dangling symlink — source missing: {}", source.display())
        }
        LinkState::UnmanagedConflict => match std::fs::symlink_metadata(to) {
            Ok(m) if m.is_dir() => "exists as a directory".to_string(),
            Ok(_) => "exists as a regular file".to_string(),
            Err(_) => "exists".to_string(),
        },
        // Correct / Missing don't reach this branch.
        _ => "is in an unexpected state".to_string(),
    };
    println!();
    println!("conflict ({repo_name}): {} {}", to.display(), what);
    if !matches!(state, LinkState::BrokenSource { .. }) {
        println!("  → would symlink from {}", expected_source.display());
    }
    println!();
}

fn print_diff_unavailable() {
    println!();
    println!("  diff is not implemented yet — pick another action.");
    println!();
}

fn report_apply(outcome: &ApplyOutcome, expected_source: &Path, to: &Path) {
    match outcome {
        ApplyOutcome::Skipped => {
            println!("  skipped: {}", to.display());
        }
        ApplyOutcome::Overwritten => {
            println!(
                "  overwritten: {} → {}",
                to.display(),
                expected_source.display()
            );
        }
        ApplyOutcome::BackedUp { backup_path } => {
            println!("  backed up: {} → {}", to.display(), backup_path.display());
            println!(
                "  symlinked: {} → {}",
                to.display(),
                expected_source.display()
            );
        }
        ApplyOutcome::Adopted => {
            println!(
                "  adopted: {} → {} (then symlinked back)",
                to.display(),
                expected_source.display()
            );
        }
        ApplyOutcome::Removed => {
            println!("  removed: {}", to.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::collections::VecDeque;
    use std::fs;
    use std::os::unix::fs as unix_fs;
    use tempfile::TempDir;

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    /// Scripted prompter: pops choices in order, errors if exhausted.
    fn scripted(choices: Vec<Choice>) -> impl FnMut(&PromptCtx<'_>) -> anyhow::Result<Choice> {
        let mut queue: VecDeque<Choice> = choices.into();
        move |_ctx| {
            queue
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("scripted prompter exhausted"))
        }
    }

    /// Build a config with a single repo whose links are listed in order.
    /// `clone_path` and link `to` paths are baked in as absolute literals
    /// so no env substitution is required at test time.
    fn config_for(clone_path: &Path, links: &[(&str, &Path)]) -> Config {
        let mut toml = format!(
            "[r]\nrepo = \"https://example.com/r.git\"\nclone = \"{}\"\n",
            clone_path.display()
        );
        for (from, to) in links {
            toml.push_str(&format!(
                "\n[[r.links]]\nfrom = \"{from}\"\nto = \"{}\"\n",
                to.display()
            ));
        }
        Config::from_toml_str(&toml).unwrap()
    }

    #[test]
    fn covers_all_six_conflict_states_with_overwrite_backup_skip() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();

        // Sources in the repo.
        write_file(&repo.join("missing-src"), "1");
        write_file(&repo.join("correct-src"), "2");
        write_file(&repo.join("wrong-src"), "3");
        write_file(&repo.join("file-src"), "4");
        write_file(&repo.join("dir-src/inside"), "5");
        write_file(&repo.join("nested-src"), "6");

        // (1) Missing — no prior target.
        let missing_to = home.join("missing");
        // (2) Correct — pre-existing correct symlink.
        let correct_to = home.join("correct");
        unix_fs::symlink(repo.join("correct-src"), &correct_to).unwrap();
        // (3) WrongTarget — symlink points elsewhere.
        let elsewhere = tmp.path().join("elsewhere");
        write_file(&elsewhere, "");
        let wrong_to = home.join("wrong");
        unix_fs::symlink(&elsewhere, &wrong_to).unwrap();
        // (4) UnmanagedConflict (regular file).
        let file_to = home.join("file.toml");
        write_file(&file_to, "user content");
        // (5) UnmanagedConflict (non-empty directory).
        let dir_to = home.join("dir");
        write_file(&dir_to.join("user-data"), "important");
        // (6) Missing where parent doesn't exist.
        let nested_to = home.join("nested/deep/down/file");

        let config = config_for(
            &repo,
            &[
                ("missing-src", &missing_to),
                ("correct-src", &correct_to),
                ("wrong-src", &wrong_to),
                ("file-src", &file_to),
                ("dir-src", &dir_to),
                ("nested-src", &nested_to),
            ],
        );

        // Three prompts (correct + the two missings don't prompt).
        let mut prompter = scripted(vec![
            Choice::Action(Action::Overwrite),
            Choice::Action(Action::Backup),
            Choice::Action(Action::Skip),
        ]);

        run_with(&config, &mut prompter).unwrap();

        // (1) Missing → created.
        assert_eq!(
            link::link_state(&repo.join("missing-src"), &missing_to).unwrap(),
            LinkState::Correct
        );
        // (2) Correct → still correct.
        assert_eq!(
            link::link_state(&repo.join("correct-src"), &correct_to).unwrap(),
            LinkState::Correct
        );
        // (3) WrongTarget → overwritten, now correct.
        assert_eq!(
            link::link_state(&repo.join("wrong-src"), &wrong_to).unwrap(),
            LinkState::Correct
        );
        // (4) Regular file → backed up; .bak holds prior content; link correct.
        assert_eq!(
            link::link_state(&repo.join("file-src"), &file_to).unwrap(),
            LinkState::Correct
        );
        assert_eq!(
            fs::read_to_string(home.join("file.toml.bak")).unwrap(),
            "user content"
        );
        // (5) Skipped → directory still present, no symlink at that path.
        let meta = fs::symlink_metadata(&dir_to).unwrap();
        assert!(meta.is_dir());
        assert!(!meta.file_type().is_symlink());
        assert_eq!(
            fs::read_to_string(dir_to.join("user-data")).unwrap(),
            "important"
        );
        // (6) Missing under non-existent parent → created, parents mkdir-p'd.
        assert_eq!(
            link::link_state(&repo.join("nested-src"), &nested_to).unwrap(),
            LinkState::Correct
        );
    }

    #[test]
    fn adopt_moves_target_into_repo_then_symlinks_back() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let home_file = tmp.path().join("home/.config-target");
        write_file(&home_file, "user-owned");

        let config = config_for(&repo, &[("config-target", &home_file)]);
        let mut prompter = scripted(vec![Choice::Action(Action::Adopt)]);
        run_with(&config, &mut prompter).unwrap();

        assert_eq!(
            fs::read_to_string(repo.join("config-target")).unwrap(),
            "user-owned"
        );
        assert_eq!(
            link::link_state(&repo.join("config-target"), &home_file).unwrap(),
            LinkState::Correct
        );
    }

    #[test]
    fn quit_short_circuits_remaining_links() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        write_file(&repo.join("a"), "");
        write_file(&repo.join("b"), "");
        let conflict_a = tmp.path().join("home/a");
        write_file(&conflict_a, "x");
        let missing_b = tmp.path().join("home/b");

        let config = config_for(&repo, &[("a", &conflict_a), ("b", &missing_b)]);
        let mut prompter = scripted(vec![Choice::Quit]);
        run_with(&config, &mut prompter).unwrap();

        // First link untouched (Quit before any apply).
        assert!(
            !fs::symlink_metadata(&conflict_a)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        // Second link not processed — still missing.
        assert_eq!(
            link::link_state(&repo.join("b"), &missing_b).unwrap(),
            LinkState::Missing
        );
    }

    #[test]
    fn diff_choice_re_prompts_then_skip() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        write_file(&repo.join("src"), "");
        let conflict_to = tmp.path().join("home/file");
        write_file(&conflict_to, "user");

        let config = config_for(&repo, &[("src", &conflict_to)]);
        // Diff is re-prompted; then Skip terminates the loop.
        let mut prompter = scripted(vec![
            Choice::Diff,
            Choice::Diff,
            Choice::Action(Action::Skip),
        ]);
        run_with(&config, &mut prompter).unwrap();

        assert_eq!(fs::read_to_string(&conflict_to).unwrap(), "user");
    }

    fn shortcuts(menu: &Menu<Choice>) -> Vec<char> {
        menu.options().iter().map(|o| o.shortcut).collect()
    }

    #[test]
    fn build_menu_hides_adopt_when_source_already_exists() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("repo/already-here");
        write_file(&source, ""); // source EXISTS — Adopt would clobber
        let to = tmp.path().join("home/file");
        write_file(&to, "user");
        let ctx = PromptCtx {
            state: &LinkState::UnmanagedConflict,
            expected_source: &source,
            to: &to,
        };
        let menu = build_menu(&ctx);
        assert!(!shortcuts(&menu).contains(&'a'));
        // Diff is offered for regular files.
        assert!(shortcuts(&menu).contains(&'d'));
    }

    #[test]
    fn build_menu_offers_adopt_when_source_missing() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("repo/not-yet-there");
        // Source does NOT exist.
        let to = tmp.path().join("home/file");
        write_file(&to, "user");
        let ctx = PromptCtx {
            state: &LinkState::UnmanagedConflict,
            expected_source: &source,
            to: &to,
        };
        let menu = build_menu(&ctx);
        assert!(shortcuts(&menu).contains(&'a'));
    }

    #[test]
    fn build_menu_hides_diff_for_directory_conflicts() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("repo/x");
        let to = tmp.path().join("home/x");
        fs::create_dir_all(&to).unwrap(); // directory, not file
        let ctx = PromptCtx {
            state: &LinkState::UnmanagedConflict,
            expected_source: &source,
            to: &to,
        };
        let menu = build_menu(&ctx);
        assert!(!shortcuts(&menu).contains(&'d'));
    }

    #[test]
    fn build_menu_hides_diff_for_wrong_target_symlinks() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("repo/x");
        let elsewhere = tmp.path().join("elsewhere");
        write_file(&elsewhere, "");
        let to = tmp.path().join("home/x");
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        unix_fs::symlink(&elsewhere, &to).unwrap();
        let ctx = PromptCtx {
            state: &LinkState::WrongTarget {
                actual: elsewhere.clone(),
            },
            expected_source: &source,
            to: &to,
        };
        let menu = build_menu(&ctx);
        assert!(!shortcuts(&menu).contains(&'d'));
    }

    #[test]
    fn build_menu_for_broken_source_offers_only_remove_skip_quit() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("repo/gone");
        // Source is deliberately absent.
        let to = tmp.path().join("home/link");
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        unix_fs::symlink(&source, &to).unwrap();
        let ctx = PromptCtx {
            state: &LinkState::BrokenSource {
                source: source.clone(),
            },
            expected_source: &source,
            to: &to,
        };
        let menu = build_menu(&ctx);
        assert_eq!(shortcuts(&menu), vec!['r', 's', 'q']);
    }

    #[test]
    fn broken_source_remove_deletes_dangling_symlink() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        // Dangling symlink: source path in repo does not exist.
        let to = tmp.path().join("home/dangling");
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        unix_fs::symlink(repo.join("gone"), &to).unwrap();

        let config = config_for(&repo, &[("gone", &to)]);
        let mut prompter = scripted(vec![Choice::Action(Action::Remove)]);
        run_with(&config, &mut prompter).unwrap();

        assert!(fs::symlink_metadata(&to).is_err());
    }

    #[test]
    fn broken_source_skip_leaves_dangling_symlink_in_place() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let to = tmp.path().join("home/dangling");
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        unix_fs::symlink(repo.join("gone"), &to).unwrap();

        let config = config_for(&repo, &[("gone", &to)]);
        let mut prompter = scripted(vec![Choice::Action(Action::Skip)]);
        run_with(&config, &mut prompter).unwrap();

        // Symlink still present (dangling).
        let meta = fs::symlink_metadata(&to).unwrap();
        assert!(meta.file_type().is_symlink());
    }

    #[test]
    fn skips_repo_when_clone_path_missing() {
        let tmp = TempDir::new().unwrap();
        let nonexistent = tmp.path().join("never-cloned");
        let to = tmp.path().join("home/x");

        let config = config_for(&nonexistent, &[("src", &to)]);
        // Empty prompter: must never be called.
        let mut prompter = scripted(vec![]);
        run_with(&config, &mut prompter).unwrap();
        assert!(!to.exists());
    }
}
