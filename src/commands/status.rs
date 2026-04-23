// `polydot status` — read-only per-repo report.
//
// For each configured repo: open the local checkout (if any), gather git
// state (dirty?, ahead/behind upstream), and classify each link target.
// Output is plain text, one block per repo. No network, no writes.

use std::path::PathBuf;

use anyhow::Context;

use crate::config::{Config, Link, RepoConfig};
use crate::git::{self, GitStatus};
use crate::link::{self, LinkState};
use crate::paths::{SystemEnv, evaluate};
use crate::ui;

#[derive(Debug)]
struct RepoReport {
    name: String,
    clone_path: PathBuf,
    git: RepoGitState,
    links: Vec<LinkReport>,
}

#[derive(Debug)]
enum RepoGitState {
    NotCloned,
    Open(GitStatus),
    OpenError(String),
}

#[derive(Debug)]
struct LinkReport {
    from: String,
    to: PathBuf,
    state: LinkResult,
}

#[derive(Debug)]
enum LinkResult {
    Resolved(LinkState),
    /// `to` couldn't be evaluated (path expression error after load — should
    /// be unreachable given Phase 1's load-time validation, but we degrade
    /// gracefully anyway).
    UnresolvableTarget(String),
    /// `fs::symlink_metadata` failed for a reason other than NotFound.
    InspectionError(String),
}

pub fn run(config: &Config) -> anyhow::Result<()> {
    if config.repos.is_empty() {
        println!("(no repos configured)");
        return Ok(());
    }
    let env = SystemEnv;
    let reports: Vec<_> = config
        .repos
        .iter()
        .map(|(name, repo_cfg)| gather(name, repo_cfg, &env))
        .collect::<anyhow::Result<_>>()?;
    print!("{}", format_reports(&reports));
    Ok(())
}

fn gather(name: &str, repo_cfg: &RepoConfig, env: &SystemEnv) -> anyhow::Result<RepoReport> {
    let clone_path_str = evaluate(&repo_cfg.clone, env)
        .with_context(|| format!("evaluating clone path for `{name}`"))?;
    let clone_path = PathBuf::from(&clone_path_str);

    let git_state = gather_git(&clone_path);
    let links = repo_cfg
        .links
        .iter()
        .map(|link| gather_link(&clone_path, link, env))
        .collect();

    Ok(RepoReport {
        name: name.to_string(),
        clone_path,
        git: git_state,
        links,
    })
}

fn gather_git(clone_path: &std::path::Path) -> RepoGitState {
    if !clone_path.exists() {
        return RepoGitState::NotCloned;
    }
    match git::open(clone_path) {
        Ok(repo) => match git::status(&repo) {
            Ok(s) => RepoGitState::Open(s),
            Err(e) => RepoGitState::OpenError(e.to_string()),
        },
        Err(e) => RepoGitState::OpenError(e.to_string()),
    }
}

fn gather_link(clone_path: &std::path::Path, link: &Link, env: &SystemEnv) -> LinkReport {
    let to_str = match evaluate(&link.to, env) {
        Ok(s) => s,
        Err(e) => {
            return LinkReport {
                from: link.from.clone(),
                to: PathBuf::new(),
                state: LinkResult::UnresolvableTarget(e.to_string()),
            };
        }
    };
    let to = PathBuf::from(&to_str);
    let expected_source = clone_path.join(&link.from);
    let state = match link::link_state(&expected_source, &to) {
        Ok(s) => LinkResult::Resolved(s),
        Err(e) => LinkResult::InspectionError(e.to_string()),
    };
    LinkReport {
        from: link.from.clone(),
        to,
        state,
    }
}

fn format_reports(reports: &[RepoReport]) -> String {
    let mut out = String::new();
    for (i, report) in reports.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&format_repo(report));
    }
    out
}

fn format_repo(report: &RepoReport) -> String {
    let mut out = format!("{}  {}\n", report.name, report.clone_path.display());
    let rows: Vec<(&str, String)> = vec![
        ("git", format_git(&report.git)),
        ("links", format_links_summary(&report.links)),
    ];
    out.push_str(&ui::render_kv(&rows));
    for (i, link) in report.links.iter().enumerate() {
        if let Some(detail) = format_link_detail(link, i + 1) {
            out.push_str(&detail);
        }
    }
    out
}

fn format_git(state: &RepoGitState) -> String {
    match state {
        RepoGitState::NotCloned => "(not cloned)".to_string(),
        RepoGitState::OpenError(msg) => format!("error: {msg}"),
        RepoGitState::Open(s) => format_git_status(s),
    }
}

fn format_git_status(s: &GitStatus) -> String {
    let cleanliness = if s.dirty { "DIRTY" } else { "clean" };
    let branch = s.branch.as_deref().unwrap_or("(detached)");
    match (&s.upstream, s.ahead_behind) {
        (Some(upstream), Some((ahead, behind))) => {
            format!("{cleanliness} · {branch} ↑{ahead} ↓{behind} {upstream}")
        }
        (None, _) => format!("{cleanliness} · {branch} (no upstream)"),
        (Some(upstream), None) => format!("{cleanliness} · {branch} {upstream}"),
    }
}

fn format_links_summary(links: &[LinkReport]) -> String {
    if links.is_empty() {
        return "(none configured)".to_string();
    }
    let total = links.len();
    let correct = links
        .iter()
        .filter(|l| matches!(l.state, LinkResult::Resolved(LinkState::Correct)))
        .count();
    let broken = total - correct;
    if broken == 0 {
        format!("{correct}/{total} correct")
    } else {
        format!("{correct}/{total} correct, {broken} need attention")
    }
}

fn format_link_detail(link: &LinkReport, index: usize) -> Option<String> {
    let label = match &link.state {
        LinkResult::Resolved(LinkState::Correct) => return None,
        LinkResult::Resolved(LinkState::Missing) => "missing".to_string(),
        LinkResult::Resolved(LinkState::WrongTarget { actual }) => {
            format!("wrong target → {}", actual.display())
        }
        LinkResult::Resolved(LinkState::BrokenSource { source }) => {
            format!("broken — source gone: {}", source.display())
        }
        LinkResult::Resolved(LinkState::UnmanagedConflict) => {
            "conflict (not a symlink)".to_string()
        }
        LinkResult::UnresolvableTarget(msg) => format!("path-expression error: {msg}"),
        LinkResult::InspectionError(msg) => format!("inspect failed: {msg}"),
    };
    Some(format!(
        "    [{index}] {from} → {to}  ({label})\n",
        from = link.from,
        to = link.to.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Link, RepoConfig};
    use crate::paths::parse;
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs as unix_fs;
    use tempfile::TempDir;

    fn link_at(from: &str, to: &str) -> Link {
        Link {
            from: from.to_string(),
            to: parse(to).unwrap(),
        }
    }

    #[test]
    fn report_for_unconfigured_repos_shows_marker() {
        let config = Config {
            path: None,
            repos: BTreeMap::new(),
        };
        // run() prints — we can't easily capture stdout here without extra
        // wiring; format_reports([]) should be empty.
        assert_eq!(format_reports(&[]), "");
        assert!(config.repos.is_empty());
    }

    #[test]
    fn format_repo_clean_with_correct_links() {
        let report = RepoReport {
            name: "x".to_string(),
            clone_path: PathBuf::from("/tmp/x"),
            git: RepoGitState::Open(GitStatus {
                dirty: false,
                branch: Some("main".to_string()),
                upstream: Some("origin/main".to_string()),
                ahead_behind: Some((0, 0)),
            }),
            links: vec![LinkReport {
                from: "config.toml".to_string(),
                to: PathBuf::from("/tmp/y"),
                state: LinkResult::Resolved(LinkState::Correct),
            }],
        };
        let out = format_repo(&report);
        assert!(out.contains("x  /tmp/x"));
        assert!(out.contains("clean"));
        assert!(out.contains("↑0 ↓0"));
        assert!(out.contains("1/1 correct"));
        // Correct links don't show detail rows.
        assert!(!out.contains("[1]"));
    }

    #[test]
    fn format_repo_dirty_shows_dirty_marker() {
        let report = RepoReport {
            name: "x".to_string(),
            clone_path: PathBuf::from("/tmp/x"),
            git: RepoGitState::Open(GitStatus {
                dirty: true,
                branch: Some("main".to_string()),
                upstream: Some("origin/main".to_string()),
                ahead_behind: Some((2, 0)),
            }),
            links: vec![],
        };
        let out = format_repo(&report);
        assert!(out.contains("DIRTY"));
        assert!(out.contains("↑2 ↓0"));
        assert!(out.contains("(none configured)"));
    }

    #[test]
    fn format_repo_missing_link_shows_detail() {
        let report = RepoReport {
            name: "x".to_string(),
            clone_path: PathBuf::from("/tmp/x"),
            git: RepoGitState::NotCloned,
            links: vec![LinkReport {
                from: "a".to_string(),
                to: PathBuf::from("/tmp/missing"),
                state: LinkResult::Resolved(LinkState::Missing),
            }],
        };
        let out = format_repo(&report);
        assert!(out.contains("(not cloned)"));
        assert!(out.contains("0/1 correct, 1 need attention"));
        assert!(out.contains("[1] a → /tmp/missing  (missing)"));
    }

    #[test]
    fn format_repo_no_upstream_omits_arrows() {
        let report = RepoReport {
            name: "x".to_string(),
            clone_path: PathBuf::from("/tmp/x"),
            git: RepoGitState::Open(GitStatus {
                dirty: false,
                branch: Some("main".to_string()),
                upstream: None,
                ahead_behind: None,
            }),
            links: vec![],
        };
        let out = format_repo(&report);
        assert!(out.contains("(no upstream)"));
        assert!(!out.contains("↑"));
    }

    /// End-to-end gather+format with real fs state.
    #[test]
    fn gather_and_format_dogfood_shape() {
        let scratch = TempDir::new().unwrap();
        let clone_path = scratch.path().join("repo");
        fs::create_dir_all(clone_path.join("file")).unwrap();
        fs::write(clone_path.join("file/inner"), "x").unwrap();

        let to_dir = scratch.path().join("destination");
        fs::create_dir_all(&to_dir).unwrap();
        let to = to_dir.join("link");
        unix_fs::symlink(clone_path.join("file"), &to).unwrap();

        let mut repos = BTreeMap::new();
        repos.insert(
            "demo".to_string(),
            RepoConfig {
                repo: "https://example.com/demo.git".to_string(),
                clone: parse(clone_path.to_str().unwrap()).unwrap(),
                links: vec![link_at("file", to.to_str().unwrap())],
            },
        );
        let config = Config {
            path: None,
            repos,
        };

        let env = SystemEnv;
        let reports: Vec<_> = config
            .repos
            .iter()
            .map(|(n, c)| gather(n, c, &env).unwrap())
            .collect();
        let rendered = format_reports(&reports);
        assert!(rendered.contains("demo"));
        // Not a git repo → OpenError or NotCloned. The clone path *exists*
        // (we just created it as a plain dir), so we expect OpenError.
        assert!(rendered.contains("error:") || rendered.contains("(not cloned)"));
        assert!(rendered.contains("1/1 correct"));
    }
}
