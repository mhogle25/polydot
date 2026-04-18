// Symlink state inspection and write operations.
//
// Read-only: `link_state` classifies what's at `to` vs the expected source.
// Write: `create` makes a fresh symlink; `apply` resolves a conflict by
// applying one of the four file-mutating actions (Overwrite / Backup /
// Adopt / Skip). Prompting belongs to the command driver — this module
// stays pure-filesystem and unit-testable against tempdirs.

use std::fs;
use std::io;
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

const BACKUP_SUFFIX: &str = "bak";
const BACKUP_MAX_TRIES: usize = 1024;

/// Classification of the filesystem state at a link's `to` path, relative
/// to the expected source path inside the managed repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkState {
    /// Symlink exists and resolves to the expected source.
    Correct,
    /// Symlink exists but points elsewhere. Carries the actual target.
    WrongTarget { actual: PathBuf },
    /// Nothing exists at `to`.
    Missing,
    /// `to` exists as a regular file or directory — not a symlink at all.
    UnmanagedConflict,
}

/// Inspect `to` and classify it against the expected source `expected_source`.
///
/// Both paths should be absolute. The expected source is normally
/// `repo_clone_path.join(link.from)`. Network/git state is irrelevant here.
pub fn link_state(expected_source: &Path, to: &Path) -> Result<LinkState> {
    let meta = match fs::symlink_metadata(to) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(LinkState::Missing),
        Err(e) => return Err(e.into()),
    };
    if !meta.file_type().is_symlink() {
        return Ok(LinkState::UnmanagedConflict);
    }

    let raw_target = fs::read_link(to)?;
    let actual_abs = absolutize_symlink_target(&raw_target, to);
    if same_path(&actual_abs, expected_source) {
        Ok(LinkState::Correct)
    } else {
        Ok(LinkState::WrongTarget { actual: actual_abs })
    }
}

/// Resolve a symlink's literal target against the symlink's parent dir,
/// so relative targets become absolute. Does not require the target to exist.
fn absolutize_symlink_target(raw_target: &Path, link_path: &Path) -> PathBuf {
    if raw_target.is_absolute() {
        raw_target.to_path_buf()
    } else {
        link_path
            .parent()
            .unwrap_or(Path::new("/"))
            .join(raw_target)
    }
}

/// Path equality that prefers canonical form when both sides exist on disk
/// (collapses `..`, resolves intermediate symlinks). Falls back to literal
/// path comparison so we don't lie about correctness when the source is
/// missing — we'd rather call it "wrong target" and let the user decide.
fn same_path(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

/// One of the four file-mutating responses a user can pick at a conflict
/// prompt. `Skip` is a no-op kept here so callers can treat resolutions
/// uniformly. `Quit` is intentionally absent — that's the driver's
/// concern, not the filesystem layer's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Overwrite,
    Backup,
    Adopt,
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    Overwritten,
    BackedUp { backup_path: PathBuf },
    Adopted,
    Skipped,
}

/// Create the symlink `to → expected_source`, mkdir-p'ing the parent of
/// `to` if needed. Caller is responsible for verifying nothing's already
/// at `to` (use [`link_state`] first).
pub fn create(expected_source: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    unix_fs::symlink(expected_source, to)?;
    Ok(())
}

/// Apply a conflict resolution. Re-inspects state internally so we don't
/// race against the user changing things between prompt and action.
pub fn apply(expected_source: &Path, to: &Path, action: Action) -> Result<ApplyOutcome> {
    match action {
        Action::Skip => Ok(ApplyOutcome::Skipped),
        Action::Overwrite => {
            remove_target(to)?;
            create(expected_source, to)?;
            Ok(ApplyOutcome::Overwritten)
        }
        Action::Backup => {
            let backup_path = pick_backup_path(to)?;
            fs::rename(to, &backup_path)?;
            create(expected_source, to)?;
            Ok(ApplyOutcome::BackedUp { backup_path })
        }
        Action::Adopt => adopt(expected_source, to),
    }
}

fn adopt(expected_source: &Path, to: &Path) -> Result<ApplyOutcome> {
    if expected_source.exists() {
        return Err(Error::Config(format!(
            "cannot adopt: source already exists at {}",
            expected_source.display()
        )));
    }
    if let Some(parent) = expected_source.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(to, expected_source)?;
    create(expected_source, to)?;
    Ok(ApplyOutcome::Adopted)
}

/// Remove whatever's at `path`: file, directory, or symlink.
fn remove_target(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    let ft = meta.file_type();
    if ft.is_dir() && !ft.is_symlink() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

/// Pick the first non-colliding `<path>.bak[.N]` suffix.
fn pick_backup_path(target: &Path) -> Result<PathBuf> {
    let base = append_extension(target, BACKUP_SUFFIX);
    if !base.exists() {
        return Ok(base);
    }
    for n in 1..BACKUP_MAX_TRIES {
        let candidate = append_extension(target, &format!("{BACKUP_SUFFIX}.{n}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(Error::Config(format!(
        "no free `.bak` slot for {} after {BACKUP_MAX_TRIES} tries",
        target.display()
    )))
}

/// Append `.<suffix>` to a path without replacing any existing extension.
/// `Path::with_extension` would clobber `foo.toml` → `foo.bak`; we want
/// `foo.toml.bak`.
fn append_extension(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".");
    s.push(suffix);
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs as unix_fs;
    use tempfile::TempDir;

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"").unwrap();
    }

    #[test]
    fn missing_target_reports_missing() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("src");
        touch(&source);
        let to = dir.path().join("nope");
        assert_eq!(link_state(&source, &to).unwrap(), LinkState::Missing);
    }

    #[test]
    fn correct_symlink_reports_correct() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("src");
        touch(&source);
        let to = dir.path().join("link");
        unix_fs::symlink(&source, &to).unwrap();
        assert_eq!(link_state(&source, &to).unwrap(), LinkState::Correct);
    }

    #[test]
    fn relative_symlink_to_correct_source_reports_correct() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("src");
        touch(&source);
        let to = dir.path().join("link");
        // Relative link: "src" interpreted from `to`'s parent.
        unix_fs::symlink("src", &to).unwrap();
        assert_eq!(link_state(&source, &to).unwrap(), LinkState::Correct);
    }

    #[test]
    fn wrong_target_symlink_reports_wrong_target() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("expected");
        touch(&source);
        let other = dir.path().join("other");
        touch(&other);
        let to = dir.path().join("link");
        unix_fs::symlink(&other, &to).unwrap();

        let state = link_state(&source, &to).unwrap();
        match state {
            LinkState::WrongTarget { actual } => {
                assert_eq!(
                    fs::canonicalize(actual).unwrap(),
                    fs::canonicalize(other).unwrap()
                );
            }
            other => panic!("expected WrongTarget, got {other:?}"),
        }
    }

    #[test]
    fn regular_file_at_target_reports_unmanaged_conflict() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("src");
        touch(&source);
        let to = dir.path().join("link");
        touch(&to);
        assert_eq!(
            link_state(&source, &to).unwrap(),
            LinkState::UnmanagedConflict
        );
    }

    #[test]
    fn directory_at_target_reports_unmanaged_conflict() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("src");
        touch(&source);
        let to = dir.path().join("link");
        fs::create_dir_all(&to).unwrap();
        assert_eq!(
            link_state(&source, &to).unwrap(),
            LinkState::UnmanagedConflict
        );
    }

    #[test]
    fn create_makes_parent_dirs_then_symlinks() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("src");
        touch(&source);
        let to = dir.path().join("a/b/c/link");
        create(&source, &to).unwrap();
        assert_eq!(link_state(&source, &to).unwrap(), LinkState::Correct);
    }

    #[test]
    fn apply_skip_does_nothing() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("src");
        touch(&source);
        let to = dir.path().join("existing");
        touch(&to);
        let out = apply(&source, &to, Action::Skip).unwrap();
        assert_eq!(out, ApplyOutcome::Skipped);
        // Existing file untouched; no symlink created.
        assert!(to.exists());
        assert!(!fs::symlink_metadata(&to).unwrap().file_type().is_symlink());
    }

    #[test]
    fn apply_overwrite_removes_file_then_symlinks() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("src");
        touch(&source);
        let to = dir.path().join("existing");
        touch(&to);
        let out = apply(&source, &to, Action::Overwrite).unwrap();
        assert_eq!(out, ApplyOutcome::Overwritten);
        assert_eq!(link_state(&source, &to).unwrap(), LinkState::Correct);
    }

    #[test]
    fn apply_overwrite_removes_directory_recursively() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("src");
        touch(&source);
        let to = dir.path().join("existing");
        fs::create_dir_all(to.join("nested/deep")).unwrap();
        fs::write(to.join("nested/file"), "x").unwrap();
        apply(&source, &to, Action::Overwrite).unwrap();
        assert_eq!(link_state(&source, &to).unwrap(), LinkState::Correct);
    }

    #[test]
    fn apply_backup_renames_then_symlinks() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("src");
        touch(&source);
        let to = dir.path().join("config.toml");
        fs::write(&to, "old contents").unwrap();
        let out = apply(&source, &to, Action::Backup).unwrap();
        let backup_path = match out {
            ApplyOutcome::BackedUp { backup_path } => backup_path,
            other => panic!("expected BackedUp, got {other:?}"),
        };
        // Backup keeps original extension intact: foo.toml -> foo.toml.bak.
        assert_eq!(backup_path, dir.path().join("config.toml.bak"));
        assert_eq!(fs::read_to_string(&backup_path).unwrap(), "old contents");
        assert_eq!(link_state(&source, &to).unwrap(), LinkState::Correct);
    }

    #[test]
    fn apply_backup_picks_free_suffix_when_bak_taken() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("src");
        touch(&source);
        let to = dir.path().join("file");
        fs::write(&to, "current").unwrap();
        fs::write(dir.path().join("file.bak"), "older").unwrap();
        let out = apply(&source, &to, Action::Backup).unwrap();
        let backup_path = match out {
            ApplyOutcome::BackedUp { backup_path } => backup_path,
            other => panic!("expected BackedUp, got {other:?}"),
        };
        assert_eq!(backup_path, dir.path().join("file.bak.1"));
    }

    #[test]
    fn apply_adopt_moves_target_into_repo_then_symlinks() {
        let dir = TempDir::new().unwrap();
        // Repo doesn't yet contain `.config` — adopt is the bootstrap case.
        let source = dir.path().join("repo/.config");
        fs::create_dir_all(dir.path().join("repo")).unwrap();
        let to = dir.path().join("home/.config");
        fs::create_dir_all(&to).unwrap();
        fs::write(to.join("inner.txt"), "user data").unwrap();

        let out = apply(&source, &to, Action::Adopt).unwrap();
        assert_eq!(out, ApplyOutcome::Adopted);
        // Source now exists in the repo with the original content.
        assert_eq!(
            fs::read_to_string(source.join("inner.txt")).unwrap(),
            "user data"
        );
        // `to` is now a symlink pointing back to the adopted source.
        assert_eq!(link_state(&source, &to).unwrap(), LinkState::Correct);
    }

    #[test]
    fn apply_adopt_refuses_when_source_already_exists() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("repo/conflict");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, "in-repo").unwrap();
        let to = dir.path().join("home/conflict");
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        fs::write(&to, "user").unwrap();

        let err = apply(&source, &to, Action::Adopt).unwrap_err();
        assert!(matches!(err, Error::Config(msg) if msg.contains("cannot adopt")));
        // Both sides are unchanged.
        assert_eq!(fs::read_to_string(&source).unwrap(), "in-repo");
        assert_eq!(fs::read_to_string(&to).unwrap(), "user");
    }
}
