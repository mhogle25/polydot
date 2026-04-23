// HTTPS credentials for git remotes.
//
// Resolution order for a given host:
//   1. Env var (currently only `GITHUB_TOKEN` for github.com — same name
//      `gh` and most GitHub tooling honor, so one PAT can serve everything).
//   2. `~/.config/polydot/credentials.toml`, table `[hosts."<host>"]`.
//   3. `git credential fill` — consults whatever credential helper git is
//      configured with (osxkeychain, libsecret, manager-core, gh, etc.).
//      `GIT_TERMINAL_PROMPT=0` keeps it non-interactive; a missing `git`
//      binary, an unconfigured helper, or an empty reply all fall through
//      cleanly to step 4.
//   4. Nothing → caller decides whether to error.
//
// File must be mode 0600 on Unix; looser permissions are a hard refusal,
// not a warning. The file is treated as a secret store.

use std::collections::BTreeMap;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
        resolve(
            host,
            &self.hosts,
            &|key| std::env::var(key).ok().filter(|s| !s.is_empty()),
            &git_credential_fill,
        )
    }

    pub fn require_for_host(&self, host: &str) -> Result<HostCredentials> {
        self.for_host(host).ok_or_else(|| missing_credentials(host))
    }
}

fn resolve(
    host: &str,
    file_creds: &BTreeMap<String, HostCredentials>,
    env: &dyn Fn(&str) -> Option<String>,
    helper: &dyn Fn(&str) -> Option<HostCredentials>,
) -> Option<HostCredentials> {
    if host == GITHUB_HOST
        && let Some(token) = env(GITHUB_TOKEN_ENV)
    {
        return Some(HostCredentials {
            username: DEFAULT_USERNAME.to_string(),
            token,
        });
    }
    if let Some(creds) = file_creds.get(host) {
        return Some(creds.clone());
    }
    helper(host)
}

// Ask git's configured credential helper for a host's credentials. Returns
// None if git isn't on PATH, the helper isn't configured, the helper has
// no stored credentials, or the reply is unparsable. Never errors — this
// step is a best-effort fallback and callers treat absence as "try the
// next step" (which for `resolve` is "return None and let the caller
// produce the user-facing missing-credentials error").
fn git_credential_fill(host: &str) -> Option<HostCredentials> {
    let mut child = Command::new("git")
        .args(["credential", "fill"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    {
        let mut stdin = child.stdin.take()?;
        writeln!(stdin, "protocol=https").ok()?;
        writeln!(stdin, "host={host}").ok()?;
        writeln!(stdin).ok()?;
    }

    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }

    let reply = std::str::from_utf8(&output.stdout).ok()?;
    parse_credential_reply(reply)
}

// Parse a `git credential fill` reply block: `key=value` lines terminated
// by a blank line or EOF. `password` is required; `username` defaults to
// `x-access-token` when absent (what GitHub expects for token-only auth).
fn parse_credential_reply(reply: &str) -> Option<HostCredentials> {
    let mut username = None;
    let mut password = None;
    for line in reply.lines() {
        if line.is_empty() {
            break;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "username" => username = Some(value.to_string()),
            "password" => password = Some(value.to_string()),
            _ => {}
        }
    }
    Some(HostCredentials {
        username: username.unwrap_or_else(|| DEFAULT_USERNAME.to_string()),
        token: password?,
    })
}

fn missing_credentials(host: &str) -> Error {
    if host == GITHUB_HOST {
        Error::Config(format!(
            "no credentials configured for `{host}`.\n\
             Generate a personal access token at https://github.com/settings/tokens, then pick one:\n  \
             - export {GITHUB_TOKEN_ENV}=<token>\n  \
             - add to ~/.config/polydot/credentials.toml:\n      \
             [hosts.\"{host}\"]\n      \
             username = \"<your-github-username>\"\n      \
             token = \"<token>\"\n  \
             - or store it via a git credential helper (e.g. macOS:\n      \
             `git config --global credential.helper osxkeychain`, then clone once to prime)"
        ))
    } else {
        Error::Config(format!(
            "no credentials configured for `{host}`.\n\
             Pick one:\n  \
             - add to ~/.config/polydot/credentials.toml:\n      \
             [hosts.\"{host}\"]\n      \
             username = \"<username>\"\n      \
             token = \"<token>\"\n  \
             - or store it via a git credential helper for `{host}`"
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

    fn no_helper(_: &str) -> Option<HostCredentials> {
        None
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

        let got = resolve("github.com", &hosts, &env, &no_helper).unwrap();
        assert_eq!(got.token, "env-token");
        assert_eq!(got.username, DEFAULT_USERNAME);
    }

    #[test]
    fn file_used_when_env_unset() {
        let mut hosts = BTreeMap::new();
        hosts.insert("github.com".to_string(), host_creds("alice", "file-token"));
        let env = stub_env(HashMap::new());

        let got = resolve("github.com", &hosts, &env, &no_helper).unwrap();
        assert_eq!(got.token, "file-token");
        assert_eq!(got.username, "alice");
    }

    #[test]
    fn missing_both_returns_none() {
        let hosts = BTreeMap::new();
        let env = stub_env(HashMap::new());
        assert!(resolve("github.com", &hosts, &env, &no_helper).is_none());
    }

    #[test]
    fn env_var_only_applies_to_github() {
        let hosts = BTreeMap::new();
        let env = stub_env([(GITHUB_TOKEN_ENV, "ghp_x")].into_iter().collect());
        assert!(resolve("gitlab.com", &hosts, &env, &no_helper).is_none());
    }

    #[test]
    fn other_hosts_resolved_via_file_only() {
        let mut hosts = BTreeMap::new();
        hosts.insert("gitlab.com".to_string(), host_creds("u", "glpat"));
        let env = stub_env(HashMap::new());

        let got = resolve("gitlab.com", &hosts, &env, &no_helper).unwrap();
        assert_eq!(got.token, "glpat");
        assert_eq!(got.username, "u");
    }

    #[test]
    fn helper_consulted_when_env_and_file_empty() {
        let hosts = BTreeMap::new();
        let env = stub_env(HashMap::new());
        let helper = |host: &str| {
            assert_eq!(host, "github.com");
            Some(host_creds("from-helper", "helper-token"))
        };

        let got = resolve("github.com", &hosts, &env, &helper).unwrap();
        assert_eq!(got.token, "helper-token");
        assert_eq!(got.username, "from-helper");
    }

    #[test]
    fn file_beats_helper_when_both_present() {
        let mut hosts = BTreeMap::new();
        hosts.insert("gitlab.com".to_string(), host_creds("u", "file-token"));
        let env = stub_env(HashMap::new());
        let helper = |_: &str| Some(host_creds("ignored", "ignored"));

        let got = resolve("gitlab.com", &hosts, &env, &helper).unwrap();
        assert_eq!(got.token, "file-token");
    }

    #[test]
    fn env_beats_helper_for_github() {
        let hosts = BTreeMap::new();
        let env = stub_env([(GITHUB_TOKEN_ENV, "env-token")].into_iter().collect());
        let helper = |_: &str| Some(host_creds("ignored", "ignored"));

        let got = resolve("github.com", &hosts, &env, &helper).unwrap();
        assert_eq!(got.token, "env-token");
    }

    #[test]
    fn helper_returning_none_falls_through() {
        let hosts = BTreeMap::new();
        let env = stub_env(HashMap::new());
        assert!(resolve("gitlab.com", &hosts, &env, &no_helper).is_none());
    }

    #[test]
    fn parse_reply_extracts_username_and_password() {
        let reply = "protocol=https\nhost=github.com\nusername=alice\npassword=ghp_secret\n";
        let got = parse_credential_reply(reply).unwrap();
        assert_eq!(got.username, "alice");
        assert_eq!(got.token, "ghp_secret");
    }

    #[test]
    fn parse_reply_missing_password_returns_none() {
        let reply = "protocol=https\nhost=github.com\nusername=alice\n";
        assert!(parse_credential_reply(reply).is_none());
    }

    #[test]
    fn parse_reply_missing_username_defaults_to_x_access_token() {
        let reply = "protocol=https\nhost=github.com\npassword=ghp_secret\n";
        let got = parse_credential_reply(reply).unwrap();
        assert_eq!(got.username, DEFAULT_USERNAME);
        assert_eq!(got.token, "ghp_secret");
    }

    #[test]
    fn parse_reply_stops_at_blank_line() {
        let reply = "username=alice\npassword=first\n\nusername=bob\npassword=second\n";
        let got = parse_credential_reply(reply).unwrap();
        assert_eq!(got.username, "alice");
        assert_eq!(got.token, "first");
    }

    #[test]
    fn parse_reply_ignores_malformed_lines() {
        let reply = "protocol=https\nno-equals-here\nusername=alice\npassword=ghp\n";
        let got = parse_credential_reply(reply).unwrap();
        assert_eq!(got.username, "alice");
        assert_eq!(got.token, "ghp");
    }

    #[test]
    fn parse_reply_empty_input_returns_none() {
        assert!(parse_credential_reply("").is_none());
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

    #[test]
    fn missing_credentials_message_mentions_git_credential_helper() {
        let err = missing_credentials("github.com");
        assert!(err.to_string().contains("credential helper"));
    }
}
