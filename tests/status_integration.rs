// End-to-end integration test for `polydot status`.
//
// Builds a tempdir with two managed repos in known states (one clean, one
// dirty), wires up a config pointing at them, runs the binary, and asserts
// the rendered output. Exercises the full path: clap → config load → path
// expression eval → git2 read-only ops → link state inspection → output.

use std::fs;
use std::path::Path;
use std::process::Command;

use git2::{BranchType, Repository};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_polydot");

/// Init a working repo with one commit on `main`, an `origin` pointing at a
/// fresh bare repo in the same scratch dir, and an upstream tracking branch.
fn init_repo_with_remote(scratch: &Path, name: &str) -> std::path::PathBuf {
    let work = scratch.join(name);
    let bare = scratch.join(format!("{name}.git"));
    fs::create_dir_all(&work).unwrap();
    fs::create_dir_all(&bare).unwrap();

    Repository::init_bare(&bare).unwrap();
    let repo = Repository::init(&work).unwrap();
    let mut cfg = repo.config().unwrap();
    cfg.set_str("user.name", "Test").unwrap();
    cfg.set_str("user.email", "test@example.com").unwrap();

    fs::write(work.join("README.md"), "hi\n").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("README.md")).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = repo.signature().unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
        .unwrap();

    let head_short = repo.head().unwrap().shorthand().unwrap().to_string();
    if head_short != "main" {
        let head_commit = repo
            .find_commit(repo.head().unwrap().target().unwrap())
            .unwrap();
        repo.branch("main", &head_commit, true).unwrap();
        repo.set_head("refs/heads/main").unwrap();
    }

    repo.remote("origin", &format!("file://{}", bare.display()))
        .unwrap();
    let mut remote = repo.find_remote("origin").unwrap();
    remote
        .push(&["refs/heads/main:refs/heads/main"], None)
        .unwrap();
    let mut local = repo.find_branch("main", BranchType::Local).unwrap();
    local.set_upstream(Some("origin/main")).unwrap();

    work
}

#[test]
fn status_against_two_repos_clean_and_dirty() {
    let scratch = TempDir::new().unwrap();
    let clean = init_repo_with_remote(scratch.path(), "alpha");
    let dirty = init_repo_with_remote(scratch.path(), "beta");

    // Make beta dirty.
    fs::write(dirty.join("scratch.txt"), "uncommitted").unwrap();

    // Symlink target for alpha — point it correctly.
    let alpha_link_target = scratch.path().join("alpha-link");
    std::os::unix::fs::symlink(clean.join("README.md"), &alpha_link_target).unwrap();

    // Symlink target for beta — leave it missing on disk.
    let beta_missing = scratch.path().join("beta-missing");

    let config_path = scratch.path().join("config.toml");
    let config = format!(
        r#"
[save]
default_mode = "per-repo"

[alpha]
repo  = "file://{bare_alpha}"
clone = "{clone_alpha}"
links = [{{ from = "README.md", to = "{link_alpha}" }}]

[beta]
repo  = "file://{bare_beta}"
clone = "{clone_beta}"
links = [{{ from = "README.md", to = "{link_beta}" }}]
"#,
        bare_alpha = scratch.path().join("alpha.git").display(),
        clone_alpha = clean.display(),
        link_alpha = alpha_link_target.display(),
        bare_beta = scratch.path().join("beta.git").display(),
        clone_beta = dirty.display(),
        link_beta = beta_missing.display(),
    );
    fs::write(&config_path, config).unwrap();

    let output = Command::new(BIN)
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .output()
        .expect("failed to spawn polydot");

    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        output.status.success(),
        "polydot status exited non-zero\nstderr: {stderr}\nstdout: {stdout}"
    );

    // alpha — clean, link correct.
    assert!(
        stdout.contains("alpha"),
        "stdout missing alpha block:\n{stdout}"
    );
    assert!(stdout.contains("clean"), "alpha should be clean:\n{stdout}");
    assert!(
        stdout.contains("1/1 correct"),
        "alpha should report 1/1 correct:\n{stdout}"
    );
    // beta — dirty, link missing.
    assert!(
        stdout.contains("beta"),
        "stdout missing beta block:\n{stdout}"
    );
    assert!(stdout.contains("DIRTY"), "beta should be dirty:\n{stdout}");
    assert!(
        stdout.contains("0/1 correct"),
        "beta should report 0/1 correct:\n{stdout}"
    );
    assert!(
        stdout.contains("(missing)"),
        "beta link should be flagged missing:\n{stdout}"
    );
}
