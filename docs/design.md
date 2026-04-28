# polydot — Design Spec

## Purpose

A git orchestrator for keeping dotfiles in sync across machines, via the per-concern repo pattern (one git repo per concern). polydot does clone, symlink, save (commit + push) across all managed repos with one command each.

## Repo + binary layout

```
~/dev/projects/polydot/              # source repo (Rust)
├── Cargo.toml
├── src/
└── ...

~/.local/bin/polydot                  # installed binary

~/dev/config/polydot-config/          # synced via polydot itself
└── config.toml                       # the polydot config

~/.config/polydot/config.toml          → symlink to polydot-config/config.toml
```

## Language: Rust

Decided. Rationale: mature CLI ecosystem (`clap`, `serde` + `toml`, `git2`, `dirs`, `anyhow`) collapses arg parsing, config parsing, git ops, cross-platform path handling, and error context into one-line-each concerns. Justified for a user-facing tool with public-release intent.

## Config format

```toml
# ~/.config/polydot/config.toml

# Each [<name>] is a managed repo
[nvim-config]
repo  = "https://github.com/<you>/nvim-config.git"
clone = "~/dev/config/nvim-config"
links = [{ from = ".", to = "~/.config/nvim" }]

[shell-config]
repo  = "git@github.com:<you>/shell-config.git"
clone = "~/dev/config/shell-config"
links = [
  { from = "zshrc",  to = "~/.zshrc" },
  { from = "bashrc", to = "~/.bashrc" },
]

[polydot-config]
repo  = "https://github.com/<you>/polydot-config.git"
clone = "~/dev/config/polydot-config"
links = [{ from = "config.toml", to = "~/.config/polydot/config.toml" }]
```

`polydot-config` is self-listed — once bootstrapped, polydot manages its own config repo like any other.

## Path expansion

`clone` and link `to` are plain strings with shell-style expansion applied at command-run time:

- **`~`** at the start expands to the user's home dir.
- **`$NAME`** expands to env var `NAME`.
- **`$$`** is a literal `$` (shell convention).

No other syntax is interpreted; everything else passes through verbatim.

## Commands

```
polydot bootstrap <config-repo-url> [--to <path>]
    Clone the config repo, symlink config.toml into place, then sync + link
    everything else. The "new machine" entry point.
    URL accepts any scheme git can clone (https, ssh, file). Auth is
    inherited from the user's git config.
    --to defaults to $XDG_DATA_HOME/polydot/config.

polydot sync
    Clone missing repos. Pull existing repos. On conflict during pull:
    interactive prompt per affected repo.

polydot link
    Create/verify symlinks per config. On conflict (target exists):
    interactive prompt per conflict.

polydot status
    Per-repo summary: clean/dirty, ahead/behind origin, link state
    (correct / wrong target / missing / unmanaged-conflict).

polydot save [-m "<message>"]
    Commit dirty changes + push, across all managed repos.
    No flag: per-repo interactive prompt for each dirty repo.
    -m "<msg>": shared mode — one commit message across all dirty repos.

polydot push
    Push already-committed work across all repos. No new commits.
```

## Interactive prompts

### Link conflict (target exists)

```
conflict: ~/.config/nvim exists as a directory
  → would symlink from ~/dev/config/nvim-config

[o]verwrite — remove existing, create symlink
[b]ackup    — rename existing to <path>.bak, then symlink
[a]dopt     — move existing INTO the repo, then symlink (bootstrap case)
[s]kip      — leave this one alone for now
[d]iff      — show difference between existing and repo content (files only)
[q]uit      — stop processing remaining links
choice>
```

### Save when repo has diverged

```
=== claude-memory ===
Local has 2 commits ahead of origin/main.
Remote has 1 commit ahead of local.
Fast-forward not possible.

[r]ebase local onto remote
[m]anual — drop me into a shell at the repo
[s]kip this repo (don't push)
[a]bort save
choice>
```

### Sync when pull fails on conflict

```
=== nvim-config ===
Pull failed: merge conflict in init.lua

[m]anual — drop me into a shell at the repo to resolve
[s]kip this repo (leave at pre-pull state)
[a]bort sync
choice>
```

### Save in per-repo mode

```
=== nvim-config (3 files changed) ===
 init.lua              |  15 +++
 lua/plugins/init.lua  |   2 +
 lua/util.lua          |   7 +++---

[v]iew full diff | [s]kip | commit message>
```

After all dirty repos: `3 repos pushed, 1 skipped, 0 failed`.

## Bootstrap flow (new machine)

1. Install Rust toolchain (`rustup`).
2. Install polydot:
   ```sh
   cargo install --git https://github.com/mhogle25/polydot
   ```
   (Or download a release binary once the project ships releases.)
3. Bootstrap:
   ```sh
   polydot bootstrap https://github.com/mhogle25/polydot-config.git --to ~/dev/config/polydot-config
   ```
   This clones polydot-config to the path given by `--to` (default: `$XDG_DATA_HOME/polydot/config`), symlinks `config.toml` into `~/.config/polydot/`, then runs `polydot sync && polydot link` to pick up everything else.

One manual step (install binary), one automated bootstrap command, fully synced from there.

## Hard scope line

Out of scope for v1:
- Templating (variable substitution in file contents)
- Secrets (encrypted values)
- Non-git-backed targets (plain directories)

## Out of scope for v1, possible later

- Parallel git operations (currently serial)
- `polydot doctor` health-check command
- Per-repo branch policies (currently assumes single-branch workflow)
- Lightweight events system (post-save / post-link / post-sync hooks for user shell scripts)
