# polydot — Development Roadmap

Companion to [`docs/design.md`](docs/design.md). The design doc is *what* the tool does; this is *when and in what order* we build it.

## Working principles

- **Build read-only before write, local before remote.** Every command class stabilizes against real fixtures before the next class is started. No half-working `save` while `link` is still buggy.
- **Each phase is independently shippable.** End of every phase: `cargo fmt --check && cargo clippy -- -D warnings && cargo test` green, and the new surface is exercised against the real claude-memory setup (the v1 dogfood target).
- **Tests track the phase.** Unit tests inline (`#[cfg(test)] mod tests`) for pure logic; integration tests under `tests/` drive the real binary against tempdir fixtures — no git2 mocking.
- **Conventional commits, one logical change per commit.** Phase boundaries are good PR boundaries.
- **No phase carries a TODO into the next.** Backlog items get filed in this doc's "Deferred" section, not left as `// TODO` in code.

## Module dependency map

```
            ┌──────────┐
            │  error   │ (foundation — used by everything)
            └────┬─────┘
                 │
        ┌────────┼────────┐
        │        │        │
     ┌──▼──┐  ┌──▼──┐  ┌──▼──┐
     │ ui  │  │paths│  │config│
     └──┬──┘  └──┬──┘  └──┬──┘
        │        └────┬───┘
        │             │
        │        ┌────▼────┐
        │        │   git   │
        │        └────┬────┘
        │             │
        │        ┌────▼────┐
        └────────►  link   │
                 └────┬────┘
                      │
              ┌───────┼───────┐
              │       │       │
           status   sync    save
                              │
                          push, bootstrap
```

`paths` and `config` are independent — config *uses* paths but paths is testable on its own. `link`/`git` build on both. Commands compose modules; they don't depend on each other.

---

## Phase 0 — Scaffold

**Goal:** A buildable, lintable, CI-checked Rust project with all 6 subcommands wired through clap and stubbed.

**Scope:**
- `cargo new --bin polydot`
- Crate-root lints: `#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]`
- Cargo deps: `clap` (derive), `serde`+`toml`, `git2`, `dirs`, `thiserror`+`anyhow`, `tracing`+`tracing-subscriber`
- Module skeletons: `error.rs`, `ui/mod.rs`, `paths.rs`, `config.rs`, `git/mod.rs`, `link.rs`, `commands/{bootstrap,sync,link,status,save,push}.rs`
- `Error` enum with `thiserror`; `main()` catches `anyhow::Error` and exits cleanly
- All 6 subcommands: argument shape from design spec, body prints "not yet implemented" and returns `Ok(())`
- `tracing-subscriber` initialized, `--verbose` flag plumbed
- `LICENSE` (GPLv3), `README.md` (one-liner + design doc link), `.gitignore`
- GitHub Actions: `fmt --check`, `clippy -D warnings`, `test`

**Out of scope:** any real implementation, real config parsing, dialoguer.

**Acceptance:**
- `cargo build` clean
- `cargo clippy -- -D warnings` clean
- `cargo test` green (will be near-empty)
- `polydot --help` lists all 6 subcommands with correct arg shapes
- `polydot status` exits 0 and prints "not yet implemented"
- CI green on first push

**Estimated effort:** 1 session (~2-3 hrs).

---

## Phase 1 — Config + paths

**Goal:** TOML config parses into typed structs; path expressions evaluate to absolute paths.

**Scope:**
- `config.rs`: `Config`, `RepoConfig`, `Link`, `SaveConfig` structs with `serde` derives. Toml round-trip tested against the dogfood `polydot-config/config.toml`.
- `paths.rs`: expression evaluator for `$VAR`, `~`, `${expr | transform}`, `${expr | t1 | t2}`, `$$` escape.
- Three transforms: `slug`, `basename`, `dirname`.
- Add `dialoguer` dep (used in later phases — pull in now so it's locked).

**Out of scope:** any I/O, any git operations, any commands actually doing work.

**Module dependencies:** `error`. (No git, no ui interaction.)

**Acceptance:**
- Unit tests: every expression form (plain `$VAR`, `~`, single transform, chained transforms, `$$` escape, missing var = error, unknown transform = error)
- Round-trip test: load real `polydot-config/config.toml` → struct → re-serialize → matches
- Commands still print "not yet implemented" but `--config <path>` flag now actually loads & validates

**Estimated effort:** 2-3 sessions. Path parser is the meaty bit; everything else is serde boilerplate.

---

## Phase 2 — `status` (read-only)

**Goal:** `polydot status` prints accurate per-repo state for the real claude-memory setup.

**Scope:**
- `git/mod.rs`: read-only wrappers — `is_dirty(repo)`, `ahead_behind(repo, branch)`.
- `link.rs`: read-only — `link_state(from, to)` returns `Correct | WrongTarget | Missing | UnmanagedConflict`.
- `commands/status.rs`: iterate repos, format table (repo name, clean/dirty, ahead/behind, link summary).
- `ui/`: simple table-printing helpers; `println!`/`eprintln!` only, no prompts yet.

**Out of scope:** any write operations, any prompts, network operations (no fetch — ahead/behind is local-only against existing remote-tracking branches).

**Module dependencies:** `error`, `paths`, `config`, `git`, `link`, `ui`, `commands/status`.

**Acceptance:**
- Run `polydot status` against the real `~/dev/config/polydot-config/config.toml` (once it exists). Output reflects reality of the current claude-memory checkout (clean/dirty matches `git status`, link state matches actual `ls -la` of the four claude-memory symlinks).
- Integration test: tempdir with two fake "managed" repos in known states (one clean, one dirty), assert table output.

**Estimated effort:** 2 sessions.

---

## Phase 3 — `link` (first write command)

**Goal:** `polydot link` creates/repairs symlinks, with interactive prompts on conflict.

**Scope:**
- Extend `link.rs` with `apply(from, to, action)` and conflict resolution.
- Prompts via `dialoguer`: overwrite / backup / adopt / skip / diff / quit (per design spec).
- `commands/link.rs`: iterate, prompt on conflict, summarize (`N created, M skipped, K conflicts deferred`).

**Out of scope:** git operations beyond what `status` already needs.

**Module dependencies:** Phase 2 modules + dialoguer prompts.

**Acceptance:**
- Integration test: tempdir with all six conflict types (target missing, target is correct symlink, target is wrong symlink, target is regular file, target is non-empty directory, parent dir doesn't exist). Each action exercised at least once via scripted input.
- Manual test: blow away one of the four claude-memory symlinks on this machine, run `polydot link`, verify it's restored. Then test the `backup` action by leaving a stub file in place.

**Estimated effort:** 3-4 sessions. Conflict resolution is the bulk of the complexity.

---

## Phase 4 — `sync` + `push` (network reads + simple writes)

**Goal:** Clone missing repos, pull existing ones, push committed work.

**Scope:**
- `git/`: extend with `clone(url, dest)`, `pull(repo)`, `push(repo)`. Pull is fast-forward only — anything else triggers conflict prompt.
- `commands/sync.rs`: clone missing → pull existing → on pull conflict, prompt (manual / skip / abort).
- `commands/push.rs`: trivial — push every repo's current branch, collect per-repo results.

**Out of scope:** committing changes (that's `save` in Phase 5). No merge/rebase logic in `sync`; conflicts dump user to a shell.

**Module dependencies:** Phase 3 modules + git2 network ops.

**Acceptance:**
- Integration test: local "remote" bare repos in tempdir; assert clone, pull, push all work. Conflict path tested with a pre-staged divergence.
- Manual test: `polydot sync` on this machine should be a no-op (everything already cloned and up-to-date).

**Estimated effort:** 2-3 sessions.

---

## Phase 5 — `save` (the dangerous one)

**Goal:** Commit dirty changes and push, in per-repo or shared mode, with divergence prompts.

**Scope:**
- `commands/save.rs`: detect dirty repos → mode dispatch → commit → push → handle divergence.
- Per-repo mode: per-repo prompt with diff stat preview, accept message or skip.
- Shared mode: collect dirty repos, single message prompt, apply to all.
- `-m "<msg>"` and `-i` flags override default mode.
- Divergence prompt on push failure: rebase / manual / skip / abort.

**Out of scope:** merge resolution beyond rebase. Cherry-picking, squash, etc.

**Module dependencies:** all prior modules.

**Acceptance:**
- Integration test: tempdir with two dirty repos, both modes exercised, divergence path exercised (pre-stage a remote commit, attempt save).
- Manual test: small edit to a `shared/` memory file, `polydot save -m "test save"` — should commit and push successfully, leaving claude-memory at the new HEAD.
- **Safety check:** test the abort path leaves the working tree exactly as found.

**Estimated effort:** 3-4 sessions. Most state to manage; most ways to corrupt user data.

---

## Phase 6 — `bootstrap` (composition)

**Goal:** One command takes a fresh machine to fully synced.

**Scope:**
- `commands/bootstrap.rs`: clone config repo → symlink `config.toml` into `~/.config/polydot/` → invoke `sync` → invoke `link`.
- Special case: bootstrap operates without an existing config file (it's installing one).

**Out of scope:** installing the polydot binary itself (`cargo install` does that).

**Module dependencies:** all prior commands.

**Acceptance:**
- End-to-end test in a fresh container or VM: install `cargo install --git ...`, run `polydot bootstrap https://github.com/mhogle25/polydot-config.git`, verify all four claude-memory symlinks land correctly and all configured repos are cloned.
- Re-running `bootstrap` on an already-bootstrapped machine should be safely idempotent (no-op or "already configured").

**Estimated effort:** 1-2 sessions.

---

## Phase 7 — `commit` (save decomposition) ✅ shipped

**Goal:** A dedicated `commit` command that stages and commits dirty repos without pushing. `save` becomes `commit` + `push`.

**Why:** Symmetry with `push` (the other half of save). Enables offline workflows (batch-commit now, push later) and review-before-push patterns. The save interactive flow is already most of the implementation — this is extraction, not new logic.

**Scope:**
- Extract save's per-repo commit path (diff preview, message prompt, mode dispatch) into a shared function.
- `commands/commit.rs`: iterate dirty repos → commit via shared function → summarize (`N committed, M skipped`). No push, no divergence prompt.
- `save` becomes a thin wrapper: call commit's shared function, then push's distribute path.
- `-m`, `-i`, `[save] default-mode` behave identically in both commands (same code path).

**Out of scope:** A matching `stage`/`add` command — staging without commit has no useful stopping point.

**Module dependencies:** refactor inside `commands/save.rs`; `commands/commit.rs` consumes the extraction.

**Acceptance:**
- `polydot commit -m "msg"` commits all dirty repos, leaves them ahead (not pushed).
- `polydot commit` with default-mode set prompts per-repo or shared as configured.
- `polydot save` behavior unchanged (all existing save tests still green).
- `polydot push` after `commit` distributes the commits.
- Unit + integration coverage for the new command mirrors save's.

**Estimated effort:** 1-2 sessions — mostly refactor + thin new binding.

---

## v1.0 Release

**Scope:**
- README polish: install command, quick-start, link to design doc, badges
- Repo flipped public on GitHub
- Tag `v1.0.0`, publish a release
- (Optional) publish to crates.io

**Acceptance:** A stranger can find the README, follow the install instructions, and bootstrap a real dotfile setup without contacting the author.

**Estimated effort:** 1 session.

---

## Total estimated effort

**Sessions:** ~15-20 evening sessions (~2-3 hrs each) → ~30-60 hours of focused work.

**Calendar:** highly dependent on cadence around the day job:
- Steady ~2 sessions/week → **8-10 weeks**
- Sprint mode (weekends + a vacation week) → **3-4 weeks**
- Slow drift (1 session/week + skipped weeks) → **3-4 months**

**Risk areas that could blow estimates:**
- Phase 1 path expression parser — first time hand-rolling a tiny DSL in Rust, could chase parser-design rabbit holes
- Phase 5 `save` — most state, divergence handling has many edge cases
- git2 surprises — `libgit2` ≠ `git` CLI in subtle ways (especially around merges and SSH config); we may hit issues that need workarounds

**Risk areas that could come in under estimate:**
- Phase 0, 1 (config side), 4 (push), 6 (composition) — mostly mechanical once the surrounding pieces exist
- Heavy use of Claude to draft scaffolding/tests; bottleneck is design decisions, not typing

---

## Deferred / "Out of scope for v1, possible later"

(Mirrors design doc, repeated here for roadmap completeness — items that might tempt mid-phase scope creep.)

- Parallel git operations
- `polydot add <repo> <target>` — interactive registration
- `polydot adopt --register` — config rewriting
- `polydot doctor` — health check
- Per-repo branch policies
- Plugin system (external `polydot-<transform>` binaries on PATH)

When a phase encounters a "we could also do X" thought, X gets filed here, not implemented.
