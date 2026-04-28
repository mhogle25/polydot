// TOML config loader.
//
// Top-level shape:
//   [<repo-name>]              — one table per managed repo
//
// `clone` and link `to` are plain strings with shell-style `~` and `$VAR`
// expansion applied at command-run time.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::paths::{Env, SystemEnv, expand};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub path: Option<PathBuf>,
    pub repos: BTreeMap<String, RepoConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoConfig {
    /// Remote URL to clone from.
    pub repo: String,
    /// Local checkout path. Supports `~` and `$VAR` expansion.
    pub clone: String,
    #[serde(default)]
    pub links: Vec<Link>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    /// Path within the repo (relative).
    pub from: String,
    /// Symlink target. Supports `~` and `$VAR` expansion.
    pub to: String,
}

impl Config {
    pub fn from_toml_str(s: &str) -> Result<Self> {
        let table: toml::Table = toml::from_str(s)?;

        let mut repos = BTreeMap::new();
        for (name, value) in table {
            let repo: RepoConfig = value
                .try_into()
                .map_err(|e: toml::de::Error| Error::Config(format!("[{name}]: {e}")))?;
            validate_repo_url(&name, &repo.repo)?;
            repos.insert(name, repo);
        }

        Ok(Config { path: None, repos })
    }

    pub fn load(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let mut config = Self::from_toml_str(&contents)?;
        config.path = Some(path.to_path_buf());
        config.validate_topology(&SystemEnv)?;
        Ok(config)
    }

    // Cross-repo sanity pass. Catches footguns that would silently corrupt
    // one repo with another's working tree, or point two symlinks at the
    // same location. Evaluates every `clone` and link `to` under `env` and
    // then checks pairwise for overlap.
    //
    // Failure categories:
    //   - duplicate clone paths              (two repos, same clone location)
    //   - nested clone paths                 (one repo's clone inside another)
    //   - duplicate link targets             (two links → same `to`)
    //   - nested link targets                (one `to` inside another)
    //   - link target inside a clone         (symlink would sit in a repo tree)
    //   - link target inside own repo clone  (self-referential — commits loop)
    pub fn validate_topology(&self, env: &impl Env) -> Result<()> {
        let mut clones: Vec<(String, PathBuf)> = Vec::new();
        for (name, repo) in &self.repos {
            let s = expand(&repo.clone, env)
                .map_err(|e| Error::Config(format!("[{name}] clone: {e}")))?;
            clones.push((name.clone(), PathBuf::from(s)));
        }

        for i in 0..clones.len() {
            for j in (i + 1)..clones.len() {
                let (name_a, path_a) = &clones[i];
                let (name_b, path_b) = &clones[j];
                if path_a == path_b {
                    return Err(Error::Config(format!(
                        "clone path conflict: [{name_a}] and [{name_b}] both clone to `{}`",
                        path_a.display(),
                    )));
                }
                if is_inside(path_a, path_b) || is_inside(path_b, path_a) {
                    return Err(Error::Config(format!(
                        "clone path nesting: [{name_a}] `{}` and [{name_b}] `{}` — \
                         one repo cannot live inside another",
                        path_a.display(),
                        path_b.display(),
                    )));
                }
            }
        }

        let mut links: Vec<(String, String, PathBuf)> = Vec::new();
        for (name, repo) in &self.repos {
            for link in &repo.links {
                let s = expand(&link.to, env)
                    .map_err(|e| Error::Config(format!("[{name}] link `{}` to: {e}", link.from)))?;
                links.push((name.clone(), link.from.clone(), PathBuf::from(s)));
            }
        }

        for i in 0..links.len() {
            for j in (i + 1)..links.len() {
                let (name_a, from_a, path_a) = &links[i];
                let (name_b, from_b, path_b) = &links[j];
                if path_a == path_b {
                    return Err(Error::Config(format!(
                        "link target conflict: [{name_a}] `{from_a}` and \
                         [{name_b}] `{from_b}` both target `{}`",
                        path_a.display(),
                    )));
                }
                if is_inside(path_a, path_b) || is_inside(path_b, path_a) {
                    return Err(Error::Config(format!(
                        "link target nesting: [{name_a}] `{from_a}` → `{}` and \
                         [{name_b}] `{from_b}` → `{}` — one symlink would land inside another",
                        path_a.display(),
                        path_b.display(),
                    )));
                }
            }
        }

        for (link_repo, from, to_path) in &links {
            for (clone_repo, clone_path) in &clones {
                let relation = if to_path == clone_path {
                    "equals"
                } else if is_inside(to_path, clone_path) {
                    "is inside"
                } else {
                    continue;
                };
                if link_repo == clone_repo {
                    return Err(Error::Config(format!(
                        "self-referential link: [{link_repo}] `{from}` → `{}` {relation} its own clone `{}`",
                        to_path.display(),
                        clone_path.display(),
                    )));
                }
                return Err(Error::Config(format!(
                    "cross-repo conflict: [{link_repo}] `{from}` → `{}` {relation} [{clone_repo}]'s clone `{}`",
                    to_path.display(),
                    clone_path.display(),
                )));
            }
        }

        Ok(())
    }

    pub fn require_repo(&self, name: &str) -> Result<&RepoConfig> {
        self.repos
            .get(name)
            .ok_or_else(|| Error::Config(format!("no repo `{name}` in config")))
    }

    pub fn to_toml_string(&self) -> Result<String> {
        let mut table = toml::Table::new();
        for (name, repo) in &self.repos {
            table.insert(
                name.clone(),
                toml::Value::try_from(repo)
                    .map_err(|e| Error::Config(format!("serialize [{name}]: {e}")))?,
            );
        }
        toml::to_string(&table).map_err(|e| Error::Config(format!("serialize: {e}")))
    }
}

// True iff `child` is strictly under `parent` (not equal, proper prefix at a
// path-component boundary). `PathBuf::starts_with` already enforces the
// component boundary, so `/ab` does not count as inside `/a`.
fn is_inside(child: &Path, parent: &Path) -> bool {
    child != parent && child.starts_with(parent)
}

// Accepted URL schemes — anything the user's `git` binary can clone:
//   https://     — credential helpers / GITHUB_TOKEN / etc. inherited from git
//   ssh://       — SSH keys + agent inherited from git
//   git@host:... — scp-style SSH, same as above
//   file://      — local bare repos (tests, mirrors, airgapped workflows)
//
// Plain `http://` is rejected as a footgun — git CLI accepts it but credentials
// would travel in cleartext.
pub(crate) fn validate_repo_url(name: &str, url: &str) -> Result<()> {
    if url.starts_with("https://")
        || url.starts_with("ssh://")
        || url.starts_with("file://")
        || url.starts_with("git@")
    {
        return Ok(());
    }
    let hint = if url.starts_with("http://") {
        " — plain HTTP is rejected (use HTTPS)"
    } else {
        ""
    };
    Err(Error::Config(format!(
        "[{name}]: repo url `{url}` must be HTTPS, SSH, or file://{hint}."
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct MockEnv {
        home: Option<PathBuf>,
        vars: HashMap<String, String>,
    }

    impl Env for MockEnv {
        fn var(&self, name: &str) -> Option<String> {
            self.vars.get(name).cloned()
        }
        fn home(&self) -> Option<PathBuf> {
            self.home.clone()
        }
    }

    fn mock_env() -> MockEnv {
        MockEnv {
            home: Some(PathBuf::from("/home/test")),
            vars: HashMap::new(),
        }
    }

    const EXAMPLE_CONFIG: &str = r#"
[notes]
repo  = "https://example.com/alice/notes.git"
clone = "~/dev/config/notes"

[[notes.links]]
from = "shared"
to   = "~/.notes/shared/index"

[[notes.links]]
from = "primary"
to   = "~/.notes/primary/index"

[settings]
repo  = "https://example.com/alice/settings.git"
clone = "~/dev/config/settings"
links = [{ from = "config.toml", to = "~/.config/example/config.toml" }]
"#;

    #[test]
    fn parses_representative_config() {
        let config = Config::from_toml_str(EXAMPLE_CONFIG).unwrap();
        assert_eq!(config.repos.len(), 2);

        let notes = config.repos.get("notes").unwrap();
        assert_eq!(notes.repo, "https://example.com/alice/notes.git");
        assert_eq!(notes.links.len(), 2);
        assert_eq!(notes.links[0].from, "shared");

        let settings = config.repos.get("settings").unwrap();
        assert_eq!(settings.links.len(), 1);
        assert_eq!(settings.links[0].from, "config.toml");
    }

    #[test]
    fn empty_config_is_valid() {
        let config = Config::from_toml_str("").unwrap();
        assert!(config.repos.is_empty());
    }

    #[test]
    fn accepts_ssh_scp_style_url() {
        let good = r#"
[notes]
repo = "git@example.com:alice/notes.git"
clone = "~/dev/config/notes"
"#;
        let config = Config::from_toml_str(good).unwrap();
        assert_eq!(config.repos.len(), 1);
    }

    #[test]
    fn accepts_ssh_scheme_url() {
        let good = r#"
[r]
repo = "ssh://git@example.com/alice/r.git"
clone = "~/r"
"#;
        let config = Config::from_toml_str(good).unwrap();
        assert_eq!(config.repos.len(), 1);
    }

    #[test]
    fn rejects_plain_http_url() {
        let bad = r#"
[r]
repo = "http://example.com/r.git"
clone = "~/r"
"#;
        let err = Config::from_toml_str(bad).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, Error::Config(_)));
        assert!(msg.contains("HTTPS"));
    }

    #[test]
    fn accepts_https_url() {
        let good = r#"
[r]
repo = "https://example.com/alice/r.git"
clone = "~/r"
"#;
        let config = Config::from_toml_str(good).unwrap();
        assert_eq!(config.repos.len(), 1);
    }

    #[test]
    fn accepts_file_url() {
        let good = r#"
[r]
repo = "file:///tmp/bare.git"
clone = "~/r"
"#;
        let config = Config::from_toml_str(good).unwrap();
        assert_eq!(config.repos.len(), 1);
    }

    #[test]
    fn save_section_no_longer_reserved() {
        // `[save]` used to be a special top-level section. After 1.3.0 it's
        // treated like any other repo table — so it'll fail validation
        // because it's missing the required `repo` + `clone` fields.
        let src = r#"
[save]
default-mode = "per-repo"
"#;
        let err = Config::from_toml_str(src).unwrap_err();
        assert!(matches!(err, Error::Config(_)));
    }

    #[test]
    fn round_trip_preserves_structure() {
        let original = Config::from_toml_str(EXAMPLE_CONFIG).unwrap();
        let serialized = original.to_toml_string().unwrap();
        let reparsed = Config::from_toml_str(&serialized).unwrap();
        assert_eq!(original, reparsed);
    }

    // --- validate_topology -------------------------------------------------

    #[test]
    fn topology_accepts_disjoint_repos() {
        let src = r#"
[a]
repo = "https://example.com/a.git"
clone = "~/dev/a"
links = [{ from = ".", to = "~/.config/a" }]

[b]
repo = "https://example.com/b.git"
clone = "~/dev/b"
links = [{ from = ".", to = "~/.config/b" }]
"#;
        let config = Config::from_toml_str(src).unwrap();
        config.validate_topology(&mock_env()).unwrap();
    }

    #[test]
    fn topology_rejects_duplicate_clone_paths() {
        let src = r#"
[a]
repo = "https://example.com/a.git"
clone = "~/dev/shared"

[b]
repo = "https://example.com/b.git"
clone = "~/dev/shared"
"#;
        let config = Config::from_toml_str(src).unwrap();
        let err = config.validate_topology(&mock_env()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("clone path conflict"), "msg: {msg}");
        assert!(msg.contains("[a]") && msg.contains("[b]"), "msg: {msg}");
    }

    #[test]
    fn topology_rejects_nested_clone_paths() {
        let src = r#"
[outer]
repo = "https://example.com/outer.git"
clone = "~/dev"

[inner]
repo = "https://example.com/inner.git"
clone = "~/dev/inner"
"#;
        let config = Config::from_toml_str(src).unwrap();
        let err = config.validate_topology(&mock_env()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("clone path nesting"), "msg: {msg}");
    }

    #[test]
    fn topology_allows_sibling_clone_paths_with_shared_prefix() {
        // `/home/test/dev/ab` must not register as nested in `/home/test/dev/a`.
        let src = r#"
[a]
repo = "https://example.com/a.git"
clone = "~/dev/a"

[ab]
repo = "https://example.com/ab.git"
clone = "~/dev/ab"
"#;
        let config = Config::from_toml_str(src).unwrap();
        config.validate_topology(&mock_env()).unwrap();
    }

    #[test]
    fn topology_rejects_duplicate_link_targets() {
        let src = r#"
[a]
repo = "https://example.com/a.git"
clone = "~/dev/a"
links = [{ from = "x", to = "~/.config/shared" }]

[b]
repo = "https://example.com/b.git"
clone = "~/dev/b"
links = [{ from = "y", to = "~/.config/shared" }]
"#;
        let config = Config::from_toml_str(src).unwrap();
        let err = config.validate_topology(&mock_env()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("link target conflict"), "msg: {msg}");
    }

    #[test]
    fn topology_rejects_nested_link_targets() {
        let src = r#"
[a]
repo = "https://example.com/a.git"
clone = "~/dev/a"
links = [{ from = "x", to = "~/.config/nest" }]

[b]
repo = "https://example.com/b.git"
clone = "~/dev/b"
links = [{ from = "y", to = "~/.config/nest/inner" }]
"#;
        let config = Config::from_toml_str(src).unwrap();
        let err = config.validate_topology(&mock_env()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("link target nesting"), "msg: {msg}");
    }

    #[test]
    fn topology_rejects_link_target_inside_other_repo_clone() {
        let src = r#"
[a]
repo = "https://example.com/a.git"
clone = "~/dev/a"

[b]
repo = "https://example.com/b.git"
clone = "~/dev/b"
links = [{ from = ".", to = "~/dev/a/linked" }]
"#;
        let config = Config::from_toml_str(src).unwrap();
        let err = config.validate_topology(&mock_env()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("cross-repo conflict"), "msg: {msg}");
        assert!(msg.contains("[b]") && msg.contains("[a]"), "msg: {msg}");
    }

    #[test]
    fn topology_rejects_self_referential_link() {
        let src = r#"
[a]
repo = "https://example.com/a.git"
clone = "~/dev/a"
links = [{ from = ".", to = "~/dev/a" }]
"#;
        let config = Config::from_toml_str(src).unwrap();
        let err = config.validate_topology(&mock_env()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("self-referential"), "msg: {msg}");
    }

    #[test]
    fn topology_rejects_link_target_inside_own_clone() {
        let src = r#"
[a]
repo = "https://example.com/a.git"
clone = "~/dev/a"
links = [{ from = "x", to = "~/dev/a/loop" }]
"#;
        let config = Config::from_toml_str(src).unwrap();
        let err = config.validate_topology(&mock_env()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("self-referential"), "msg: {msg}");
    }

    #[test]
    fn topology_accepts_whole_repo_link_outside_clone() {
        // nvim-config pattern: from="." to="~/.config/nvim", clone elsewhere.
        let src = r#"
[nvim]
repo = "https://example.com/nvim.git"
clone = "~/dev/config/nvim-config"
links = [{ from = ".", to = "~/.config/nvim" }]
"#;
        let config = Config::from_toml_str(src).unwrap();
        config.validate_topology(&mock_env()).unwrap();
    }

    #[test]
    fn topology_runs_on_load() {
        // Config::load must invoke validate_topology — regression guard.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[a]
repo = "https://example.com/a.git"
clone = "/tmp/polydot-test-shared"

[b]
repo = "https://example.com/b.git"
clone = "/tmp/polydot-test-shared"
"#,
        )
        .unwrap();
        let err = Config::load(&path).unwrap_err();
        assert!(err.to_string().contains("clone path conflict"));
    }
}
