// Symlink state inspection (Phase 2 — read-only).
//
// Phase 3 will add `apply()` with conflict-resolution prompts. For now we
// just classify what's at `to` against what should be there: a symlink
// pointing into the managed repo at `from`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::error::Result;

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
}
