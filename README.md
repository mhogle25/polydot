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

For a fresh machine that already has a `polydot-config` git repo:

```sh
polydot bootstrap https://github.com/<you>/<your-polydot-config>.git
```

To try polydot locally without a remote config repo first:

```sh
polydot init                              # create a fresh ~/.config/polydot/config.toml
polydot repo add nvim \                   # add a managed repo
  --repo https://github.com/<you>/nvim-config.git \
  --clone ~/dev/config/nvim-config
polydot link add nvim . ~/.config/nvim    # add a link entry
polydot sync                              # clone the repo
polydot link                              # create the symlink
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

Editing config from the shell:

```sh
polydot repo add <name> --repo <url> --clone <path>
polydot repo rm  <name>
polydot repo list

polydot link add <repo> <from> <to> [--adopt]
polydot link rm  <repo> <from>
polydot link list [<repo>]
```

`link add --adopt` moves the file currently at `<to>` into the repo at `<from>` and creates the symlink in one step — useful when you want to start managing a file that already exists on disk.

## Authentication

polydot inherits authentication from your `git` configuration. Whatever URL you can `git clone` from your shell will work in polydot — HTTPS with credential helpers (macOS Keychain, libsecret, `gh`, etc.), SSH keys, or any other transport git supports.

For non-interactive contexts (cron, CI, automation hooks), load your SSH key into ssh-agent before running polydot. When stdin isn't a TTY polydot sets `ssh -o BatchMode=yes`, so missing-passphrase prompts fail fast instead of hanging.

Plain `http://` URLs are rejected — credentials would travel in cleartext. Use `https://` instead.

## Documentation

- [Design spec](docs/design.md): what the tool does
- [Roadmap](ROADMAP.md): when and in what order it's built

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
