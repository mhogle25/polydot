# polydot

Git orchestrator for managing N dotfile repos with one command each.

`polydot` is built for the per-feature-repo dotfile pattern: one git repo per concern (editor config, shell config, claude memory, etc.). Instead of running `git pull`, `git push`, and `ln -sfn` loops by hand across all of them, polydot does each operation against every managed repo in one command.

## Install

```sh
cargo install --git https://github.com/mhogle25/polydot
```

## Quick start

New machine:

```sh
polydot bootstrap https://github.com/<you>/<your-polydot-config>.git
```

Authentication is HTTPS + PAT (set `GITHUB_TOKEN` in the environment, or add the token to `~/.config/polydot/credentials.toml`). SSH URLs are not supported.

Day-to-day:

```sh
polydot status     # what's clean / dirty / behind / unlinked across all repos
polydot sync       # pull all
polydot save       # commit + push all dirty
polydot commit     # commit all dirty, don't push (offline / review-before-push)
polydot push       # distribute already-committed work across all repos
polydot link       # repair any missing/wrong symlinks
```

## Documentation

- [Design spec](docs/design.md) — what the tool does
- [Roadmap](ROADMAP.md) — when and in what order it's built

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
