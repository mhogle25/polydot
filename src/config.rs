// TOML config loader.
//
// Top-level shape:
//   [save]                     — global save defaults (optional)
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
    pub save: SaveConfig,
    pub repos: BTreeMap<String, RepoConfig>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveConfig {
    #[serde(default)]
    pub default_mode: SaveMode,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SaveMode {
    #[default]
    PerRepo,
    Shared,
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
        let mut table: toml::Table = toml::from_str(s)?;

        let save = match table.remove("save") {
            Some(value) => value
                .try_into()
                .map_err(|e: toml::de::Error| Error::Config(format!("[save]: {e}")))?,
            None => SaveConfig::default(),
        };

        let mut repos = BTreeMap::new();
        for (name, value) in table {
            let repo: RepoConfig = value
                .try_into()
                .map_err(|e: toml::de::Error| Error::Config(format!("[{name}]: {e}")))?;
            repos.insert(name, repo);
        }

        Ok(Config {
            path: None,
            save,
            repos,
        })
    }

    pub fn load(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let mut config = Self::from_toml_str(&contents)?;
        config.path = Some(path.to_path_buf());
        Ok(config)
    }

    pub fn to_toml_string(&self) -> Result<String> {
        let mut table = toml::Table::new();
        table.insert(
            "save".to_string(),
            toml::Value::try_from(&self.save)
                .map_err(|e| Error::Config(format!("serialize [save]: {e}")))?,
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    const DOGFOOD: &str = r#"
[save]
default_mode = "per-repo"

[claude-memory]
repo  = "git@github.com:mhogle25/claude-memory.git"
clone = "~/dev/config/claude-memory"

[[claude-memory.links]]
from = "shared"
to   = "~/.claude/projects/${~ | slug}/memory"

[[claude-memory.links]]
from = "lish"
to   = "~/.claude/projects/${~/dev/projects/lish-zig | slug}/memory"

[polydot-config]
repo  = "git@github.com:mhogle25/polydot-config.git"
clone = "~/dev/config/polydot-config"
links = [{ from = "config.toml", to = "~/.config/polydot/config.toml" }]
"#;

    #[test]
    fn parses_dogfood_config() {
        let config = Config::from_toml_str(DOGFOOD).unwrap();
        assert_eq!(config.save.default_mode, SaveMode::PerRepo);
        assert_eq!(config.repos.len(), 2);

        let cm = config.repos.get("claude-memory").unwrap();
        assert_eq!(cm.repo, "git@github.com:mhogle25/claude-memory.git");
        assert_eq!(cm.links.len(), 2);
        assert_eq!(cm.links[0].from, "shared");

        let pc = config.repos.get("polydot-config").unwrap();
        assert_eq!(pc.links.len(), 1);
        assert_eq!(pc.links[0].from, "config.toml");
    }

    #[test]
    fn save_section_is_optional() {
        let config = Config::from_toml_str(
            r#"
[some-repo]
repo = "git@example.com:foo.git"
clone = "~/foo"
"#,
        )
        .unwrap();
        assert_eq!(config.save.default_mode, SaveMode::PerRepo);
    }

    #[test]
    fn empty_config_is_valid() {
        let config = Config::from_toml_str("").unwrap();
        assert!(config.repos.is_empty());
        assert_eq!(config.save.default_mode, SaveMode::PerRepo);
    }

    #[test]
    fn invalid_path_expression_fails_at_load() {
        let bad = r#"
[broken]
repo = "git@example.com:x.git"
clone = "~/foo/${unterminated"
"#;
        let err = Config::from_toml_str(bad).unwrap_err();
        assert!(matches!(err, Error::Config(_)));
    }

    #[test]
    fn unknown_transform_fails_at_load() {
        let bad = r#"
[broken]
repo = "git@example.com:x.git"
clone = "~/foo"
links = [{ from = ".", to = "${~ | bogus}" }]
"#;
        let err = Config::from_toml_str(bad).unwrap_err();
        assert!(matches!(err, Error::Config(_)));
    }

    #[test]
    fn save_mode_shared_round_trips() {
        let input = r#"
[save]
default_mode = "shared"
"#;
        let config = Config::from_toml_str(input).unwrap();
        assert_eq!(config.save.default_mode, SaveMode::Shared);
    }

    #[test]
    fn round_trip_preserves_structure() {
        let original = Config::from_toml_str(DOGFOOD).unwrap();
        let serialized = original.to_toml_string().unwrap();
        let reparsed = Config::from_toml_str(&serialized).unwrap();
        assert_eq!(original, reparsed);
    }
}
