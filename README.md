# polydot

A dotfile manager for the per-feature-repo pattern: one git repo per concern (editor, shell, work tools, anything you want pinned). polydot orchestrates them — sync, symlink, commit, push — across all of them with one command each.

## Why multi-repo?

Most dotfile managers assume one big repo with everything. polydot is for the opposite case: when each concern is its own git repo because they have different lifecycles, sharing rules, or upstream relationships. Examples:

- `nvim-config` you maintain yourself, public on GitHub
- `work-config` private to your work account
- A colorscheme repo you fork from upstream and rebase occasionally
- `polydot-config` itself, version-controlled like everything else (polydot manages its own config)

One config file lists every managed repo. One `polydot save` commits and pushes across all dirty ones. One `polydot link` repairs symlinks for everything.

```toml
# ~/.config/polydot/config.toml

[nvim-config]
repo  = "https://github.com/<you>/nvim-config.git"
clone = "~/dev/config/nvim-config"
links = [{ from = ".", to = "~/.config/nvim" }]

[shell-config]
repo  = "https://github.com/<you>/shell-config.git"
clone = "~/dev/config/shell-config"
links = [
  { from = "zshrc",  to = "~/.zshrc" },
  { from = "bashrc", to = "~/.bashrc" },
]

# polydot manages its own config repo too
[polydot-config]
repo  = "https://github.com/<you>/polydot-config.git"
clone = "~/dev/config/polydot-config"
links = [{ from = "config.toml", to = "~/.config/polydot/config.toml" }]
```

## When to use something else

- **Claude Code memory sync.** If your goal is "Claude memory follows me across machines," use [claude-brain](https://github.com/toroleapinc/claude-brain). It's purpose-built — auto-sync hooks at session start/end, semantic merge for memory conflicts, secret stripping. polydot doesn't and won't compete on those features.
- **Single-repo dotfiles.** If you have one big dotfile repo with everything in it, [chezmoi](https://github.com/twpayne/chezmoi), [yadm](https://yadm.io/), or [stow](https://www.gnu.org/software/stow/) probably fit better. polydot's reason for being is the multi-repo orchestration; if you don't need that, it's overkill.

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
