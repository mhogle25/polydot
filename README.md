# polydot

Git orchestrator for managing N dotfile repos with one command each.

`polydot` is built for the per-feature-repo dotfile pattern: one git repo per concern (editor config, shell config, claude memory, etc.). Instead of running `git pull`, `git push`, and `ln -sfn` loops by hand across all of them, polydot does each operation against every managed repo in one command.

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

polydot authenticates over HTTPS with a personal access token. SSH URLs (`git@github.com:...`) are not supported — use `https://...` URLs everywhere.

For each host, credentials are resolved in this order:

1. **`GITHUB_TOKEN` env var** — GitHub only. Same variable `gh` and most GitHub tooling honor, so one PAT can serve everything.

2. **`~/.config/polydot/credentials.toml`** — must be mode `0600`:

   ```toml
   [hosts."github.com"]
   username = "<your-github-username>"
   token    = "<pat>"
   ```

3. **`git credential fill`** — reads whatever credential helper your `git` is configured with (macOS Keychain, libsecret, Windows Credential Manager, `gh`, etc.). **If `git clone <private-repo>` on this machine works without prompting, polydot will too.** Common ways to get here:
   - `gh auth login` (GitHub CLI — installs its own helper)
   - `git config --global credential.helper osxkeychain` on macOS, then clone any private repo once to prime the keychain

If none of these yield a token, polydot errors out with a message listing all three options.

## Documentation

- [Design spec](docs/design.md) — what the tool does
- [Roadmap](ROADMAP.md) — when and in what order it's built

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
