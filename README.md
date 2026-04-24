# polydot

Sync your AI tool configs and per-project memory across machines (and your dotfiles too).

`polydot` is a git orchestrator for the configs that follow you across machines: per-project memory in your AI assistants, your editor setup, your shell, anything you want pinned. Each managed thing is its own git repo; one polydot command operates across all of them, and symlinks land them where each tool expects.

## What it's for

### AI tool configs and per-project memory

The strongest use case is keeping AI assistant configs (Claude Code, Cursor, Aider, anything in `~/.config` or `~/.<vendor>/`) and per-project memory consistent across every machine you work on. Path transforms turn a single shared repo into per-project symlinks:

```toml
[claude-memory]
repo  = "https://github.com/<you>/claude-memory.git"
clone = "~/dev/config/claude-memory"

[[claude-memory.links]]
from = "polydot"
to   = "~/.claude/projects/${~/dev/projects/polydot | slug}/memory"

[[claude-memory.links]]
from = "shared"
to   = "~/.claude/projects/${~ | slug}/memory"
```

One `claude-memory` repo, fanned out into the per-project memory directories Claude Code expects. Sync, commit, push it across every machine with one command each.

### Traditional dotfiles

polydot is also a clean fit for the per-feature-repo dotfile pattern (one repo for editor config, one for shell, etc.):

```toml
[nvim-config]
repo  = "https://github.com/<you>/nvim-config.git"
clone = "~/dev/config/nvim-config"
links = [{ from = ".", to = "~/.config/nvim" }]
```

## Install

Requires Rust 1.88 or newer.

```sh
cargo install --git https://github.com/mhogle25/polydot
```

## Quick start

```sh
polydot bootstrap https://github.com/<you>/<your-polydot-config>.git
```

Day-to-day:

```sh
polydot status     # what's clean / dirty / behind / unlinked across all repos
polydot sync       # pull all
polydot save       # commit + push all dirty
polydot commit     # commit all dirty, don't push (offline / review-before-push)
polydot push       # distribute already-committed work across all repos
polydot link       # repair any missing/wrong symlinks
```

## Authentication

polydot authenticates over HTTPS with a personal access token. SSH URLs (`git@github.com:...`) are not supported. Use `https://...` URLs everywhere.

For each host, credentials are resolved in this order:

1. **`GITHUB_TOKEN` env var** (GitHub only). Same variable `gh` and most GitHub tooling honor, so one PAT can serve everything.

2. **`~/.config/polydot/credentials.toml`** (must be mode `0600`):

   ```toml
   [hosts."github.com"]
   username = "<your-github-username>"
   token    = "<pat>"
   ```

3. **`git credential fill`**. Reads whatever credential helper your `git` is configured with (macOS Keychain, libsecret, Windows Credential Manager, `gh`, etc.). **If `git clone <private-repo>` on this machine works without prompting, polydot will too.** Common ways to get here:
   - `gh auth login` (GitHub CLI, installs its own helper)
   - `git config --global credential.helper osxkeychain` on macOS, then clone any private repo once to prime the keychain

If none of these yield a token, polydot errors out with a message listing all three options.

## Documentation

- [Design spec](docs/design.md): what the tool does
- [Roadmap](ROADMAP.md): when and in what order it's built

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
