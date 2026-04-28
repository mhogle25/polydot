// Format-preserving config mutations.
//
// Backed by `toml_edit` so comments and existing whitespace round-trip
// through `polydot repo/link add/rm`. Every mutation re-parses the
// updated TOML through `Config` so polydot's schema and topology
// invariants apply uniformly to CLI-driven and hand-edited configs.
//
// Quirk: when `add_repo` writes the first table into an otherwise
// comment-only file (e.g. fresh `polydot init` output), `toml_edit`
// places the new table above the trailing comment block — comments
// stay trailing because the original file had no tables to anchor them
// as leading decor. Output is valid TOML; users who care about order
// can resort by hand.

use std::path::Path;

use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value, value};

use crate::config::{Config, validate_repo_url};
use crate::error::{Error, Result};
use crate::paths::SystemEnv;

fn read_doc(path: &Path) -> Result<DocumentMut> {
    let raw = std::fs::read_to_string(path)?;
    raw.parse::<DocumentMut>()
        .map_err(|e| Error::Config(format!("{}: {e}", path.display())))
}

fn write_doc(path: &Path, doc: &DocumentMut) -> Result<()> {
    std::fs::write(path, doc.to_string())?;
    Ok(())
}

/// Re-parse the document through `Config` and run topology validation.
/// Catches anything our edits could break — duplicate clone paths,
/// invalid URL schemes, malformed entries — before the file gets written.
fn validate_doc(doc: &DocumentMut) -> Result<()> {
    let parsed = Config::from_toml_str(&doc.to_string())?;
    parsed.validate_topology(&SystemEnv)?;
    Ok(())
}

/// Add a new managed-repo entry. Idempotent for an exact-match request
/// (same name + same `repo` + same `clone`); errors on a conflicting name.
pub fn add_repo(path: &Path, name: &str, repo_url: &str, clone: &str) -> Result<AddOutcome> {
    validate_repo_url(name, repo_url)?;
    let mut doc = read_doc(path)?;

    if let Some(existing) = doc.get(name) {
        let table = existing.as_table().ok_or_else(|| {
            Error::Config(format!(
                "[{name}] exists but is not a table — refusing to overwrite"
            ))
        })?;
        let existing_url = table.get("repo").and_then(item_as_str);
        let existing_clone = table.get("clone").and_then(item_as_str);
        if existing_url == Some(repo_url) && existing_clone == Some(clone) {
            return Ok(AddOutcome::AlreadyExists);
        }
        return Err(Error::Config(format!(
            "[{name}] already exists with different fields — \
             remove it first or edit by hand"
        )));
    }

    let mut table = Table::new();
    table.insert("repo", value(repo_url));
    table.insert("clone", value(clone));
    doc.insert(name, Item::Table(table));

    validate_doc(&doc)?;
    write_doc(path, &doc)?;
    Ok(AddOutcome::Added)
}

/// Remove a repo entry (and all its links).
pub fn remove_repo(path: &Path, name: &str) -> Result<()> {
    let mut doc = read_doc(path)?;
    if doc.remove(name).is_none() {
        return Err(Error::Config(format!("no repo `{name}` in config")));
    }
    write_doc(path, &doc)
}

/// Add a link entry to an existing repo. Idempotent for an exact match
/// (same `from` + same `to`); errors if `from` exists with a different `to`.
pub fn add_link(path: &Path, repo: &str, from: &str, to: &str) -> Result<AddOutcome> {
    let mut doc = read_doc(path)?;
    let table = doc
        .get_mut(repo)
        .and_then(|i| i.as_table_mut())
        .ok_or_else(|| Error::Config(format!("no repo `{repo}` in config")))?;

    if let Some(existing_to) = find_link_to(table, from) {
        if existing_to == to {
            return Ok(AddOutcome::AlreadyExists);
        }
        return Err(Error::Config(format!(
            "[{repo}] link `{from}` already points at `{existing_to}` — \
             remove it first or edit by hand"
        )));
    }

    append_inline_link(table, from, to);
    validate_doc(&doc)?;
    write_doc(path, &doc)?;
    Ok(AddOutcome::Added)
}

/// Remove a link entry. Errors if the repo or the link doesn't exist.
pub fn remove_link(path: &Path, repo: &str, from: &str) -> Result<()> {
    let mut doc = read_doc(path)?;
    let table = doc
        .get_mut(repo)
        .and_then(|i| i.as_table_mut())
        .ok_or_else(|| Error::Config(format!("no repo `{repo}` in config")))?;

    if !drop_link(table, from) {
        return Err(Error::Config(format!("no link `{from}` in [{repo}]")));
    }
    write_doc(path, &doc)
}

#[derive(Debug, PartialEq, Eq)]
pub enum AddOutcome {
    /// New entry written.
    Added,
    /// Identical entry already present — no change.
    AlreadyExists,
}

// --- helpers ---

fn item_as_str(item: &Item) -> Option<&str> {
    item.as_str()
}

/// Search a repo's `links` (inline-array or array-of-tables) for a `from`
/// match. Returns the matching `to` if found.
fn find_link_to<'a>(table: &'a Table, from: &str) -> Option<&'a str> {
    let links = table.get("links")?;
    if let Some(arr) = links.as_array() {
        for link in arr.iter() {
            if let Value::InlineTable(inline) = link
                && inline_get_str(inline, "from") == Some(from)
            {
                return inline_get_str(inline, "to");
            }
        }
    }
    if let Some(aot) = links.as_array_of_tables() {
        for tbl in aot.iter() {
            if tbl.get("from").and_then(item_as_str) == Some(from) {
                return tbl.get("to").and_then(item_as_str);
            }
        }
    }
    None
}

fn inline_get_str<'a>(inline: &'a InlineTable, key: &str) -> Option<&'a str> {
    inline.get(key).and_then(|v| v.as_str())
}

/// Append a `{ from = "...", to = "..." }` to the repo's `links` array.
/// Creates the array if it doesn't exist; preserves the existing form
/// (inline array) when it does.
fn append_inline_link(table: &mut Table, from: &str, to: &str) {
    let mut entry = InlineTable::new();
    entry.insert("from", Value::from(from));
    entry.insert("to", Value::from(to));

    if let Some(arr) = table.get_mut("links").and_then(|i| i.as_array_mut()) {
        arr.push(entry);
        return;
    }
    if let Some(aot) = table
        .get_mut("links")
        .and_then(|i| i.as_array_of_tables_mut())
    {
        // Preserve array-of-tables form: append a new table block.
        let mut tbl = Table::new();
        tbl.set_implicit(false);
        tbl.insert("from", value(from));
        tbl.insert("to", value(to));
        aot.push(tbl);
        return;
    }

    let mut arr = Array::new();
    arr.push(entry);
    table.insert("links", value(arr));
}

/// Remove the link with this `from` from the repo's `links` collection,
/// in whichever form it's stored. Returns `true` if a link was removed.
fn drop_link(table: &mut Table, from: &str) -> bool {
    if let Some(arr) = table.get_mut("links").and_then(|i| i.as_array_mut()) {
        let original = arr.len();
        arr.retain(|v| match v {
            Value::InlineTable(inline) => inline_get_str(inline, "from") != Some(from),
            _ => true,
        });
        return arr.len() != original;
    }
    if let Some(aot) = table
        .get_mut("links")
        .and_then(|i| i.as_array_of_tables_mut())
    {
        let mut found = None;
        for (idx, tbl) in aot.iter().enumerate() {
            if tbl.get("from").and_then(item_as_str) == Some(from) {
                found = Some(idx);
                break;
            }
        }
        if let Some(idx) = found {
            aot.remove(idx);
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_config(path: &Path, contents: &str) {
        std::fs::write(path, contents).unwrap();
    }

    fn read_config(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn add_repo_appends_to_empty_config() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        write_config(&path, "");
        let outcome = add_repo(
            &path,
            "nvim",
            "https://example.com/nvim.git",
            "~/dev/config/nvim",
        )
        .unwrap();
        assert_eq!(outcome, AddOutcome::Added);
        let contents = read_config(&path);
        assert!(contents.contains("[nvim]"));
        assert!(contents.contains("https://example.com/nvim.git"));
    }

    #[test]
    fn add_repo_idempotent_on_exact_match() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        write_config(
            &path,
            r#"[nvim]
repo = "https://example.com/nvim.git"
clone = "~/dev/config/nvim"
"#,
        );
        let outcome = add_repo(
            &path,
            "nvim",
            "https://example.com/nvim.git",
            "~/dev/config/nvim",
        )
        .unwrap();
        assert_eq!(outcome, AddOutcome::AlreadyExists);
    }

    #[test]
    fn add_repo_errors_on_conflicting_match() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        write_config(
            &path,
            r#"[nvim]
repo = "https://example.com/nvim.git"
clone = "~/dev/config/nvim"
"#,
        );
        let err = add_repo(
            &path,
            "nvim",
            "https://example.com/different.git",
            "~/dev/config/nvim",
        )
        .unwrap_err();
        assert!(format!("{err}").contains("already exists"));
    }

    #[test]
    fn add_repo_rejects_bad_url_scheme() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        write_config(&path, "");
        let err = add_repo(
            &path,
            "nvim",
            "http://example.com/nvim.git",
            "~/dev/config/nvim",
        )
        .unwrap_err();
        assert!(format!("{err}").contains("HTTPS"));
    }

    #[test]
    fn add_repo_preserves_comments() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let original = "# my polydot config\n# edit carefully\n";
        write_config(&path, original);
        add_repo(
            &path,
            "nvim",
            "https://example.com/nvim.git",
            "~/dev/config/nvim",
        )
        .unwrap();
        let contents = read_config(&path);
        assert!(contents.contains("# my polydot config"));
        assert!(contents.contains("# edit carefully"));
        assert!(contents.contains("[nvim]"));
    }

    #[test]
    fn remove_repo_drops_table_and_links() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        write_config(
            &path,
            r#"[nvim]
repo = "https://example.com/nvim.git"
clone = "~/dev/config/nvim"
links = [{ from = ".", to = "~/.config/nvim" }]

[shell]
repo = "https://example.com/shell.git"
clone = "~/dev/config/shell"
"#,
        );
        remove_repo(&path, "nvim").unwrap();
        let contents = read_config(&path);
        assert!(!contents.contains("[nvim]"));
        assert!(!contents.contains("nvim.git"));
        assert!(contents.contains("[shell]"));
    }

    #[test]
    fn remove_repo_errors_when_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        write_config(&path, "");
        let err = remove_repo(&path, "nope").unwrap_err();
        assert!(format!("{err}").contains("no repo `nope`"));
    }

    #[test]
    fn add_link_appends_inline_to_existing_repo() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        write_config(
            &path,
            r#"[nvim]
repo = "https://example.com/nvim.git"
clone = "~/dev/config/nvim"
"#,
        );
        let outcome = add_link(&path, "nvim", ".", "~/.config/nvim").unwrap();
        assert_eq!(outcome, AddOutcome::Added);
        let contents = read_config(&path);
        assert!(contents.contains("from = \".\""));
        assert!(contents.contains("to = \"~/.config/nvim\""));
    }

    #[test]
    fn add_link_idempotent_on_exact_match() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        write_config(
            &path,
            r#"[nvim]
repo = "https://example.com/nvim.git"
clone = "~/dev/config/nvim"
links = [{ from = ".", to = "~/.config/nvim" }]
"#,
        );
        let outcome = add_link(&path, "nvim", ".", "~/.config/nvim").unwrap();
        assert_eq!(outcome, AddOutcome::AlreadyExists);
    }

    #[test]
    fn add_link_errors_on_conflicting_to() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        write_config(
            &path,
            r#"[nvim]
repo = "https://example.com/nvim.git"
clone = "~/dev/config/nvim"
links = [{ from = ".", to = "~/.config/nvim" }]
"#,
        );
        let err = add_link(&path, "nvim", ".", "~/somewhere/else").unwrap_err();
        assert!(format!("{err}").contains("already points at"));
    }

    #[test]
    fn add_link_errors_when_repo_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        write_config(&path, "");
        let err = add_link(&path, "ghost", ".", "~/.config/ghost").unwrap_err();
        assert!(format!("{err}").contains("no repo `ghost`"));
    }

    #[test]
    fn add_link_to_array_of_tables_preserves_form() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        write_config(
            &path,
            r#"[mem]
repo = "https://example.com/mem.git"
clone = "~/dev/config/mem"

[[mem.links]]
from = "alpha"
to = "~/.config/alpha"
"#,
        );
        add_link(&path, "mem", "beta", "~/.config/beta").unwrap();
        let contents = read_config(&path);
        // Both blocks should still use [[mem.links]] form.
        let count = contents.matches("[[mem.links]]").count();
        assert_eq!(
            count, 2,
            "expected 2 [[mem.links]] blocks, got:\n{contents}"
        );
    }

    #[test]
    fn remove_link_drops_inline_entry() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        write_config(
            &path,
            r#"[nvim]
repo = "https://example.com/nvim.git"
clone = "~/dev/config/nvim"
links = [
  { from = ".", to = "~/.config/nvim" },
  { from = "themes", to = "~/.config/nvim-themes" },
]
"#,
        );
        remove_link(&path, "nvim", ".").unwrap();
        let contents = read_config(&path);
        assert!(!contents.contains("from = \".\""));
        assert!(contents.contains("from = \"themes\""));
    }

    #[test]
    fn remove_link_errors_when_link_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        write_config(
            &path,
            r#"[nvim]
repo = "https://example.com/nvim.git"
clone = "~/dev/config/nvim"
"#,
        );
        let err = remove_link(&path, "nvim", ".").unwrap_err();
        assert!(format!("{err}").contains("no link `.`"));
    }

    #[test]
    fn add_repo_then_link_round_trip_loads_cleanly() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        write_config(&path, "");
        add_repo(
            &path,
            "nvim",
            "https://example.com/nvim.git",
            "~/dev/config/nvim",
        )
        .unwrap();
        add_link(&path, "nvim", ".", "~/.config/nvim").unwrap();

        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.repos.len(), 1);
        let nvim = cfg.repos.get("nvim").unwrap();
        assert_eq!(nvim.repo, "https://example.com/nvim.git");
        assert_eq!(nvim.links.len(), 1);
        assert_eq!(nvim.links[0].from, ".");
    }
}
