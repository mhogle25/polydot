// TOML config loader.
//
// Top-level shape:
//   [<repo-name>]              — one table per managed repo
//
// Path expressions in `clone` and link `to` fields are parsed at load time,
// so syntactic errors in the config surface immediately rather than at
// command-run time.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::paths::Expression;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub path: Option<PathBuf>,
    pub repos: BTreeMap<String, RepoConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoConfig {
    /// Remote URL to clone from.
    pub repo: String,
    /// Local checkout path (path expression).
    pub clone: Expression,
    #[serde(default)]
    pub links: Vec<Link>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    /// Path within the repo (relative). Plain string — no expression syntax.
    pub from: String,
    /// Symlink target (path expression).
    pub to: Expression,
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

        Ok(Config {
            path: None,
            repos,
        })
    }

    pub fn load(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let mut config = Self::from_toml_str(&contents)?;
        config.path = Some(path.to_path_buf());
        Ok(config)
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

fn validate_repo_url(name: &str, url: &str) -> Result<()> {
    if url.starts_with("https://") {
        return Ok(());
    }
    let hint = if url.starts_with("git@") || url.starts_with("ssh://") {
        " — polydot authenticates with HTTPS + PAT, not SSH"
    } else if url.starts_with("http://") {
        " — plain HTTP is rejected, use HTTPS"
    } else {
        ""
    };
    Err(Error::Config(format!(
        "[{name}]: repo url `{url}` must be HTTPS{hint}. \
         Rewrite as `https://<host>/<owner>/<repo>.git`."
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE_CONFIG: &str = r#"
[notes]
repo  = "https://example.com/alice/notes.git"
clone = "~/dev/config/notes"

[[notes.links]]
from = "shared"
to   = "~/.notes/${~ | slug}/index"

[[notes.links]]
from = "primary"
to   = "~/.notes/${~/dev/projects/example-app | slug}/index"

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
    fn invalid_path_expression_fails_at_load() {
        let bad = r#"
[broken]
repo = "https://example.com/x.git"
clone = "~/foo/${unterminated"
"#;
        let err = Config::from_toml_str(bad).unwrap_err();
        assert!(matches!(err, Error::Config(_)));
    }

    #[test]
    fn unknown_transform_fails_at_load() {
        let bad = r#"
[broken]
repo = "https://example.com/x.git"
clone = "~/foo"
links = [{ from = ".", to = "${~ | bogus}" }]
"#;
        let err = Config::from_toml_str(bad).unwrap_err();
        assert!(matches!(err, Error::Config(_)));
    }

    #[test]
    fn rejects_ssh_scp_style_url() {
        let bad = r#"
[notes]
repo = "git@example.com:alice/notes.git"
clone = "~/dev/config/notes"
"#;
        let err = Config::from_toml_str(bad).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, Error::Config(_)));
        assert!(msg.contains("HTTPS"));
        assert!(msg.contains("PAT") || msg.contains("SSH"));
    }

    #[test]
    fn rejects_ssh_scheme_url() {
        let bad = r#"
[r]
repo = "ssh://git@example.com/alice/r.git"
clone = "~/r"
"#;
        let err = Config::from_toml_str(bad).unwrap_err();
        assert!(matches!(err, Error::Config(_)));
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
    fn round_trip_preserves_structure() {
        let original = Config::from_toml_str(EXAMPLE_CONFIG).unwrap();
        let serialized = original.to_toml_string().unwrap();
        let reparsed = Config::from_toml_str(&serialized).unwrap();
        assert_eq!(original, reparsed);
    }
}
