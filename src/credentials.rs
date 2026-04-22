// HTTPS credentials for git remotes.
//
// Resolution order for a given host:
//   1. Env var (currently only `GITHUB_TOKEN` for github.com — same name
//      `gh` and most GitHub tooling honor, so one PAT can serve everything).
//   2. `~/.config/polydot/credentials.toml`, table `[hosts."<host>"]`.
//   3. Nothing → caller decides whether to error.
//
// File must be mode 0600 on Unix; looser permissions are a hard refusal,
// not a warning. The file is treated as a secret store.

use std::collections::BTreeMap;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};

const DEFAULT_USERNAME: &str = "x-access-token";
const FILE_NAME: &str = "credentials.toml";
const GITHUB_HOST: &str = "github.com";
const GITHUB_TOKEN_ENV: &str = "GITHUB_TOKEN";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    hosts: BTreeMap<String, HostCredentials>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HostCredentials {
    #[serde(default = "default_username")]
    pub username: String,
    pub token: String,
}

fn default_username() -> String {
    DEFAULT_USERNAME.to_string()
}

#[derive(Debug, Deserialize)]
struct CredentialsFile {
    #[serde(default)]
    hosts: BTreeMap<String, HostCredentials>,
}

impl Credentials {
    pub fn empty() -> Self {
        Self {
            hosts: BTreeMap::new(),
        }
    }

    /// Load from `~/.config/polydot/credentials.toml`. A missing file is fine
    /// and yields empty credentials — env-var fallback may still apply.
    pub fn load_default() -> Result<Self> {
        let path = default_path()?;
        Self::load(&path)
    }

    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::empty());
        }
        check_permissions(path)?;
        let text = std::fs::read_to_string(path)?;
        let file: CredentialsFile = toml::from_str(&text)
            .map_err(|e| Error::Config(format!("credentials at {}: {e}", path.display())))?;
        Ok(Self { hosts: file.hosts })
    }

    pub fn for_host(&self, host: &str) -> Option<HostCredentials> {
        resolve(host, &self.hosts, &|key| {
            std::env::var(key).ok().filter(|s| !s.is_empty())
        })
    }

    pub fn require_for_host(&self, host: &str) -> Result<HostCredentials> {
        self.for_host(host).ok_or_else(|| missing_credentials(host))
    }
}

fn resolve(
    host: &str,
    file_creds: &BTreeMap<String, HostCredentials>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<HostCredentials> {
    if host == GITHUB_HOST
        && let Some(token) = env(GITHUB_TOKEN_ENV)
    {
        return Some(HostCredentials {
            username: DEFAULT_USERNAME.to_string(),
            token,
        });
    }
    file_creds.get(host).cloned()
}

fn missing_credentials(host: &str) -> Error {
    if host == GITHUB_HOST {
        Error::Config(format!(
            "no credentials configured for `{host}`.\n\
             Generate a personal access token at https://github.com/settings/tokens, then either:\n  \
             - export {GITHUB_TOKEN_ENV}=<token>\n  \
             - or add to ~/.config/polydot/credentials.toml:\n      \
             [hosts.\"{host}\"]\n      \
             username = \"<your-github-username>\"\n      \
             token = \"<token>\""
        ))
    } else {
        Error::Config(format!(
            "no credentials configured for `{host}`.\n\
             Add to ~/.config/polydot/credentials.toml:\n  \
             [hosts.\"{host}\"]\n  \
             username = \"<username>\"\n  \
             token = \"<token>\""
        ))
    }
}

fn default_path() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .ok_or_else(|| Error::Config("could not determine user config dir".to_string()))?;
    Ok(dir.join("polydot").join(FILE_NAME))
}

#[cfg(unix)]
fn check_permissions(path: &Path) -> Result<()> {
    let meta = std::fs::metadata(path)?;
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(Error::Config(format!(
            "credentials file {} has mode {:o}; must be 0600 (group/world bits forbidden). Run: chmod 600 {}",
            path.display(),
            mode,
            path.display(),
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn stub_env(map: HashMap<&'static str, &'static str>) -> impl Fn(&str) -> Option<String> {
        move |key| map.get(key).map(|s| s.to_string())
    }

    fn host_creds(username: &str, token: &str) -> HostCredentials {
        HostCredentials {
            username: username.to_string(),
            token: token.to_string(),
        }
    }

    #[test]
    fn env_var_beats_file_for_github() {
        let mut hosts = BTreeMap::new();
        hosts.insert(
            "github.com".to_string(),
            host_creds("from-file", "file-token"),
        );
        let env = stub_env([(GITHUB_TOKEN_ENV, "env-token")].into_iter().collect());

        let got = resolve("github.com", &hosts, &env).unwrap();
        assert_eq!(got.token, "env-token");
        assert_eq!(got.username, DEFAULT_USERNAME);
    }

    #[test]
    fn file_used_when_env_unset() {
        let mut hosts = BTreeMap::new();
        hosts.insert("github.com".to_string(), host_creds("alice", "file-token"));
        let env = stub_env(HashMap::new());

        let got = resolve("github.com", &hosts, &env).unwrap();
        assert_eq!(got.token, "file-token");
        assert_eq!(got.username, "alice");
    }

    #[test]
    fn missing_both_returns_none() {
        let hosts = BTreeMap::new();
        let env = stub_env(HashMap::new());
        assert!(resolve("github.com", &hosts, &env).is_none());
    }

    #[test]
    fn env_var_only_applies_to_github() {
        let hosts = BTreeMap::new();
        let env = stub_env([(GITHUB_TOKEN_ENV, "ghp_x")].into_iter().collect());
        assert!(resolve("gitlab.com", &hosts, &env).is_none());
    }

    #[test]
    fn other_hosts_resolved_via_file_only() {
        let mut hosts = BTreeMap::new();
        hosts.insert("gitlab.com".to_string(), host_creds("u", "glpat"));
        let env = stub_env(HashMap::new());

        let got = resolve("gitlab.com", &hosts, &env).unwrap();
        assert_eq!(got.token, "glpat");
        assert_eq!(got.username, "u");
    }

    #[test]
    fn parses_credentials_file_with_default_username() {
        let toml_text = r#"
[hosts."github.com"]
username = "alice"
token = "ghp_test"

[hosts."gitlab.com"]
token = "glpat_test"
"#;
        let file: CredentialsFile = toml::from_str(toml_text).unwrap();
        assert_eq!(file.hosts.len(), 2);

        let github = file.hosts.get("github.com").unwrap();
        assert_eq!(github.username, "alice");
        assert_eq!(github.token, "ghp_test");

        let gitlab = file.hosts.get("gitlab.com").unwrap();
        assert_eq!(gitlab.username, DEFAULT_USERNAME);
        assert_eq!(gitlab.token, "glpat_test");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_world_readable_file() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.toml");
        fs::write(&path, "[hosts]\n").unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&path, perms).unwrap();

        let err = Credentials::load(&path).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, Error::Config(_)));
        assert!(msg.contains("0600"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_group_readable_file() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.toml");
        fs::write(&path, "[hosts]\n").unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o640);
        fs::set_permissions(&path, perms).unwrap();

        let err = Credentials::load(&path).unwrap_err();
        assert!(matches!(err, Error::Config(_)));
    }

    #[cfg(unix)]
    #[test]
    fn accepts_mode_0600_file() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.toml");
        fs::write(
            &path,
            r#"
[hosts."github.com"]
username = "u"
token = "t"
"#,
        )
        .unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&path, perms).unwrap();

        let creds = Credentials::load(&path).unwrap();
        let github = creds.hosts.get("github.com").unwrap();
        assert_eq!(github.token, "t");
    }

    #[test]
    fn missing_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.toml");
        let creds = Credentials::load(&path).unwrap();
        assert!(creds.hosts.is_empty());
    }

    #[test]
    fn missing_credentials_message_for_github_points_at_token_settings() {
        let err = missing_credentials("github.com");
        let msg = err.to_string();
        assert!(msg.contains("github.com/settings/tokens"));
        assert!(msg.contains(GITHUB_TOKEN_ENV));
    }

    #[test]
    fn missing_credentials_message_for_other_host_omits_github_specifics() {
        let err = missing_credentials("gitlab.com");
        let msg = err.to_string();
        assert!(!msg.contains("github.com/settings/tokens"));
        assert!(msg.contains("gitlab.com"));
    }
}
