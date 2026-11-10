# AGENTS.md

Guidance for AI coding agents working in this repository.

## Overview

remarkable-utils is a monorepo of tools for the reMarkable tablet. The
flagship tool is `rmu`, a CLI that manages documents and folders on the
device over SSH (list/mkdir/upload/download/move/rename/delete). Rust
utilities live as members of a single Cargo workspace; Nix flake modules
under `nix/` handle packaging, dev shells, formatting, and checks.

This repository is **open source** (dual-licensed MIT OR Apache-2.0):

- Never commit secrets, credentials, device serial numbers, or personal data
  (including real notebook content in test fixtures). Tools *may display*
  device identifiers (e.g. `rmu status` shows the serial) to the user;
  fixtures and docs must only ever contain fabricated ones.
- New crates inherit `license`, `version`, `edition`, `repository`, and
  `authors` from `[workspace.package]` in the root `Cargo.toml` and should
  include a `description`.
- **Never reference private repos or infrastructure** — anywhere:
  docs, code comments, commit messages, PR descriptions. Describe
  patterns and conventions on their own merits instead of citing where
  they came from. Referencing public projects (e.g. for attribution or
  domain references) is fine.
- Keep docs written for a general audience.

## Layout

| Path                    | Description                                            |
|-------------------------|--------------------------------------------------------|
| `flake.nix`             | Flake entry point — melds modules from `nix/` via `flake-utils.lib.meld` |
| `nix/default.nix`       | Melds `checks.nix`, `formatter.nix`, `rust-packages.nix`, `shells.nix` |
| `nix/rust-packages.nix` | crane-based Rust builds, toolchain, vendor dir, clippy check, workspace tests (hermetic git env); `rmu` wrapped with git/ssh as PATH fallbacks |
| `nix/checks.nix`        | `nix flake check` targets (pre-commit hooks, clippy, workspace tests) |
| `nix/shells.nix`        | Dev shell (rust toolchain, treefmt, openssh, git, hooks) |
| `nix/formatter.nix`     | treefmt wrapper (`nix fmt`)                            |
| `nix/lib/treefmt.nix`   | Formatter config: rustfmt, alejandra, statix, deadnix  |
| `nix/lib/git-hooks.nix` | Pre-commit hooks: treefmt + clippy (offline vendored)  |
| `libremarkable-utils/`  | Library crate (see module map below)                   |
| `rmu/`                  | `rmu` CLI binary crate (clap; all config via flags, no config file) |
| `rmu/src/hub.rs`, `rmu/src/hub/` | `rmu hub` over the library's `Hub`: arguments, `execute` (runs a command on any `&dyn Tablet`), exit statuses (`Failure`), `finish` (output and error details); pure `render.rs` (human summary, plain ids) and `json.rs` (`--json`, `"schema": 1`, unstable until device validation: the library's serialised reports plus `schema` and `command`, and the error object). Tests: `hub/tests.rs` (fabricated reports) with `hub/tests/` (`parsing`, `output`, `golden`: one exact JSON per report type and error shape, update them deliberately; `commands`: against the fake tablet), and `rmu/tests/hub_cli.rs` (the real binary over a mock `ssh` that rewrites the tablet's paths into a scratch dir) |
| `test-support/`         | Unpublished dev-dependency: helpers shared by tests (`Scratch` temp dirs with mock `bin/` + `PATH`, `write_script`, `rerun_test`, `assert_success`, `set_modified`) |
| `Cargo.toml`            | Workspace root; shared deps in `[workspace.dependencies]` |

### `libremarkable-utils` module map

- `bundle.rs` — `.rmdoc` bundle creation and parsing: repacks a tar
  streamed from the device into a zip (the official apps' export format)
  and the reverse for restore, re-targeting bundles to a fresh UUID. Pure
  bytes-to-bytes, unit-tested without a device.
- `ssh.rs` — subprocess transport around the system `ssh` binary. No SSH
  library is linked, deliberately: users get ssh config/keys/agent for
  free and the crate stays free of native deps (the dev shell is
  `mkShellNoCC`). Connections are multiplexed via ControlMaster. Password
  auth re-executes the current binary as an `SSH_ASKPASS` helper
  (`maybe_run_askpass` must be called first thing in `main`); the
  password travels via environment, never argv. Large scripts go over
  stdin (`run_script`, `sh -s`), never the remote command line (one
  argv string, 128 KiB); `stream_stdout` hands a command's stdout to the
  caller unbuffered (tar downloads).
- `git.rs` — plumbing over the system `git` binary for `rmu hub`
  (`docs/hub-design.md`): bare repositories only, explicit `--git-dir`,
  argv (never shell strings), and an explicit environment (`gc.auto=0`,
  hooks disabled, `GIT_TERMINAL_PROMPT=0` when non-interactive; the
  user's own config and credential helpers otherwise apply). Commits
  go through `fast-import` with identity always passed explicitly and
  blobs streamed; losing to a concurrent writer (non-fast-forward, stale
  lease, the remote's own compare-and-swap) is the typed `PushRejected`,
  and pushes take commit ids and full `refs/...` names, never refspecs.
  Requires git >= 2.30. Output parsers are pure and unit-tested; tests
  against temp repos must stay hermetic (`GIT_CONFIG_GLOBAL=/dev/null`,
  `GIT_CONFIG_NOSYSTEM=1`, `GIT_CONFIG_COUNT=0`, `GIT_DEFAULT_HASH=sha1`,
  temp `HOME`).
  `Oid` (strict lowercase hex) and `FileMode` are shared with
  `hub/model.rs`; don't add parallel id or mode types.
- `xochitl.rs` — on-device file formats and **pure** tree logic (path/UUID
  resolution, glob matching/expansion, ambiguity rejection,
  conflict/cycle checks, rendering). Keep this module I/O-free so it
  stays unit-testable without a device.
- `client.rs` — high-level operations composing the two. Every write
  (and the restart) first checks the `rmu hub` lock
  (`Error::HubLocked`), with the read-only probe from `hub/device`
  riding along in the listing, and carries the probe's write guard in
  its own command right before its commit point; route new writes
  through `Client::write`.
  `list_items`
  fetches metadata in batched round trips (with a listing cache it is
  incremental: stat everything, cat only changes); keep new
  operations batched too.
- `progress.rs` — observer trait for progress events (`step`, `bytes`,
  `bytes_done`, `finished`). The **library never prints**; `rmu` renders
  these as indicatif bars on stderr. New long-running `Client`
  operations must emit steps and call `finished()` on completion.
- `sync.rs` — `rmu sync`: scp-style endpoint parsing, the `FsEndpoint`
  trait (`LocalFs`, `SshFs`), snapshots, content hashing (stat-gated
  local hashes; lazy batched device payload hashes), the XDG archive
  (`$XDG_STATE_HOME/rmu/`, never inside the synced tree), **pure**
  planners (heavily unit-tested), and thin executors. Three pairings:
  fs↔tablet (its own asymmetric decision table: rmdoc
  pull-only), and fs↔fs + tablet↔tablet (one
  shared symmetric table, `decide_pair`). Keep planner logic pure; see
  `docs/sync-design.md` for semantics and archive placement rules.
- `hub/` — `rmu hub`, git-backed multi-tablet sync (see
  `docs/hub-design.md`). `hub/model.rs` is the **pure** data model:
  xochitl ↔ repository layout mapping and allowlist classification,
  `MirrorRules` (selected by the repository's mirror-rules version;
  never rmu's newest), `.rmu/hub.json` and tablet-state formats (newer
  format versions are refused, unknown fields preserved), view-state
  normalisation and overlay, per-document grouping of tree listings,
  snapshot validation (quarantine/orphan) and carry-forward, one
  submodule per concern (`model/ids.rs`, `layout.rs`, `rules.rs`,
  `state.rs`, `grouping.rs`, `snapshot.rs`, each with its tests; shared
  test helpers in `model/fixtures.rs`), re-exported from `hub::model`:
  import through that path. Keep it
  I/O-free. `hub/merge.rs` is the **pure** merge engine, phase 1:
  per-document three-way merge (blob contents through a `BlobSource`,
  history through `History`), forks with deterministic UUIDv5 ids, tree
  rules, `tree_violations` (reused by apply on the live tablet) and the
  deletion guard. Its tests (`hub/merge/tests.rs`) include a seeded
  property test that new ink is never discarded; keep it passing and
  add a case for every new rule. Hub code returns its own errors
  (`ModelError`, `MergeError`, `DeviceError`, `CycleError`), which are
  variants of `crate::Error`;
  code that calls caller-supplied readers returns `crate::Result`.
  `hub/device.rs` is tablet I/O behind the `Tablet`
  trait: `SshTablet` (every script generated by pure, tested functions
  in `device/scripts.rs`, split into `scripts/guard.rs` (guard, lock,
  watchdog, xochitl) and `scripts/transfer.rs` (stat pass, reads,
  state, staging, apply); mutating scripts start with the guard:
  watchdog marker absent and the lock still this run's, checked under
  a `flock` held until their last change, and the record carrying the
  current boot id; exceptions: release checks only ownership, starting
  xochitl is refused only by another run's lock, staging skips the
  `flock`) and
  `FakeTablet` (local directories, simulated xochitl/boot id/watchdog,
  fault injection) share the lock protocol (`device/lock.rs`) and the
  apply ordering (`device/apply.rs`). Contract scenarios in
  `device/contract_tests.rs` run against both; on Linux the real one
  runs its scripts locally through a mock `ssh` with stub
  `systemctl`/`systemd-run` and a curated `PATH` (GNU tools, and BusyBox
  `ash` + applets when `RMU_TEST_BUSYBOX` is set, as the dev shell and
  `nix flake check` do), so tests never reach the host's systemd or a
  tablet. The harnesses (`device/harness.rs`, `#[cfg(test)]`) serve the
  contract tests and the scripts' behavioural tests (`on_both`: host
  tools, then BusyBox); test what a script does by running it there,
  not by comparing its text (keep the quoting tests); the fake itself
  is public, so the sync cycle's end-to-end tests drive it directly.
  BusyBox `ash` differs from bash in ways that matter (its `kill` rejects `--`; a failed
  redirection on a special builtin such as `:` or `exec` exits the
  shell), which is why scripts are run under both. Keep new device
  operations in that shape: pure script + parser, thin executor, fake
  parity, a scenario. `device/probe.rs` is the read-only probe the
  other rmu commands check the hub lock with (lock, boot id, watchdog
  marker, attachment; marker-sectioned and never exiting, so it rides
  along in another script, and judged with the hub's own `decide`).
  `hub/cycle.rs` composes all of it into the operations behind
  `rmu hub` (`Hub`: `init`, `clone_tablet` with reattach, `sync`,
  `backup`, `status`, `dry_run`, `remote`, `set_remote_url`, `detach`;
  every report derives `Serialize` (`cycle/report.rs`), warnings are
  the typed `Warning`; `cycle.rs` itself keeps only the errors, options,
  `Hub` and helpers its submodules share): the tablet session (lock,
  watchdog, error path: `cycle/session.rs`), what a run knows about
  origin (`cycle/origin.rs`), `init` (`cycle/init.rs`), the local
  cache (`cycle/store.rs`: a bare repository per repo id under
  `$XDG_CACHE_HOME/rmu/hub`, an `flock`, raw commits and trees read
  through one `cat-file`, bounded caches; never a tree read per commit
  of history: searches go through the **pure** history index,
  `cycle/history.rs`, built from one `rev-list` and one `diff-tree`
  stream), reading the tablet into `fast-import` (`cycle/snapshot.rs`),
  the **pure** decisions (`cycle/plan.rs`: version equivalence, the
  apply plan, verification, step-14 moved paths, tree and trash checks,
  reconciliation verdicts and finish, the manifest; new decisions go
  there, with tests), commit trailers (`cycle/message.rs`), the steps
  (`cycle/steps.rs`: `sync`, `backup`, steps 1–6; its submodules
  `clone`, `reconcile` (step 4), `push` (7–11) and `apply` (12–16)),
  the read-only commands (`cycle/inspect.rs`, which shares the sync's
  merge setup, `Hub::run_merge` in `cycle/merge_run.rs`) and
  `remote`/`detach` (`cycle/admin.rs`). Base inference uses the
  merge's own "unchanged" predicate (`merge::unchanged`); keep the two
  from drifting. `tests/hub_e2e.rs` (harness in
  `tests/hub_e2e/world.rs`) runs fake tablets against a bare origin,
  ending every scenario that syncs to the end with one oracle
  (`World::check`: the tablets equal origin, no ink lost, xochitl up, no
  lock or journal left; refusals check `World::check_idle`) and
  includes crash matrices (a fault at every operation of a full cycle,
  of a run reconciling an earlier crash, of a pull-only run, of a clone
  and of a backup; the run after each recovery must be quiet), and a
  scale test (`reattach_scale_*`, `#[ignore]`d at full size: run them
  in release with `--ignored --nocapture` when touching history
  searches); add a scenario for every new cycle rule and keep the
  matrices passing.
- `status.rs` — device system state (`rmu status`): one
  marker-sectioned SSH round trip, pure best-effort parsing (every
  field `Option`al — firmware/models differ), pure rendering.
- `error.rs` — typed errors (`thiserror`).
- `marked.rs` — marker-sectioned script output (one round trip, several
  answers): the random marker, the splitter (listings, `rmu status`)
  and the single-section reader (the hub probe). Use it for new
  batched scripts rather than another parser.
- `xochitl_service.rs` — the systemctl commands for the xochitl unit,
  shared by `Client::restart_xochitl` and the hub (reset-failed first,
  always; see below).
- `xdg.rs` — XDG base-dir lookup for rmu's files on this computer (the
  hub's cache, sync's state).
- `relative_path.rs` — the pure safe relative-path splitter shared by
  `sync.rs` and `hub/model.rs`.

## reMarkable domain knowledge

Read `docs/notebook-data.md` first — it documents the storage model,
`fileType` nuances, the `.rm` page format, and `.rmdoc` bundles.

- Default USB connection: `root@10.11.99.1`; documents live under
  `/home/root/.local/share/remarkable/xochitl` (`XOCHITL_DATA_DIR`).
- Storage model: per item UUID, `<uuid>.metadata` (JSON: `visibleName`,
  `parent` UUID or `""`/`"trash"`, `type` = `DocumentType`/`CollectionType`),
  `<uuid>.content` (JSON: `fileType`), `<uuid>.<fileType>` payload, and a
  `<uuid>/` directory with per-page data. The folder tree is purely
  logical — rebuilt from `parent` pointers.
- After writing files, xochitl must be restarted to notice changes.
  **Caveat:** xochitl has a strict systemd start limit; repeated plain
  restarts can hit start-limit-hit and reboot the whole tablet. Always
  `systemctl reset-failed xochitl.service` first (see
  `Client::restart_xochitl`). Preserve this pattern.
- When read-modify-writing `.metadata`, preserve unknown fields
  (`Client::update_metadata`); different firmware versions have different
  field sets, and timestamps appear as both strings and numbers.
- Uploads accept `.pdf`/`.epub` payloads and `.rmdoc` bundles; reject
  other types. Bundle restores must always use a **fresh UUID** (the original may
  still exist on the device) and preserve unknown metadata fields.
- Native notebooks (`fileType` `"notebook"`, or `""` on older firmware —
  normalized to `"notebook"` in `item_from_metadata`) have **no payload
  file**; content is per-page `.rm` data in `<uuid>/`. Download must use
  the `.rmdoc` bundle path, never `cat <uuid>.<fileType>`.
- All remote paths/arguments must go through `ssh::shell_quote` (the
  device shell is busybox ash).

## Commands

```sh
nix develop                        # dev shell (installs pre-commit hooks)
cargo run -p rmu -- --help         # run the CLI
cargo run -p rmu -- sync --dry-run ./dir remarkable:/   # sync plan preview
cargo run -p rmu -- hub status     # hub: changes since the last sync (read-only)
cargo run -p rmu -- hub sync --dry-run --json   # hub: planned merge, as JSON
cargo build --workspace            # build everything
cargo test --workspace             # run tests
cargo clippy --all-features --all-targets -- -D warnings  # mandatory lint
nix fmt                            # format (rustfmt + Nix formatters)
nix flake check                    # full check: pre-commit hooks + clippy + workspace tests
nix build .#workspace-tests        # just the sandboxed workspace test suite
nix build .#rmu                    # build the CLI via crane
```

## Code quality (non-negotiable)

- Clippy with `-D warnings`, `--all-features --all-targets` must pass. It is
  enforced by a pre-commit hook and by `nix flake check`. Never suppress
  lints to make it pass without a clear justification.
- `nix fmt` must produce no diffs (treefmt: rustfmt, alejandra, statix,
  deadnix).
- Logic that can be pure must be pure and unit-tested (see `xochitl.rs`
  and the `parse_listing` tests in `client.rs`). Device I/O is a thin
  shell around tested logic.
- Tests that install mock executables (a fake `ssh`, `systemctl`, …)
  must write them with `test_support::write_script`, never `fs::write`
  plus `chmod`: a test thread holding a script open for writing while
  another thread spawns makes exec fail with "Text file busy".
  `test_support::Scratch` (`mock_command` + `mock_path`) does this for
  the usual scratch-dir-with-`bin/` layout; reuse it and the other
  `test-support` helpers instead of adding per-file fixture copies.

## Testing against real devices

- Unit tests never require a device; keep it that way.
- Do **not** run destructive commands (`rm`, `mv`, `rename`, `upload`)
  against a user's tablet unless explicitly asked. The safe manual
  smoke-test order is: `ls` → `mkdir` → `upload` → `download` → `mv` →
  `rename` → `rm` on a scratch folder.
- Ambiguous logical paths must be rejected, never guessed — this is a
  data-safety invariant, not a convenience choice. Glob patterns are
  the sanctioned way to address multiple items (`rm`/`mv`/`download`);
  exact-name resolution always wins over pattern expansion, and a
  pattern that matches nothing is an error.

## Conventions

### Rust

- One Cargo workspace at the repo root; each tool is a member crate
  (edition 2024, `resolver = "3"`).
- Shared dependency versions live in `[workspace.dependencies]` in the root
  `Cargo.toml`; member crates depend via `{ workspace = true }`.
- Prefer functional style: iterator chains, `fold`/`try_fold`,
  `filter_map`, `try_for_each`, and `std::iter::successors` for chain
  walks (see `ancestor_chain` in `xochitl.rs`) over imperative `for`
  loops with mutable state. Local mutation *inside* a fold accumulator
  or builder closure is fine; threading mutable state *across* loop
  iterations is what to avoid. Low-level I/O pumps (`copy_with_progress`
  in `ssh.rs`) are the accepted exception.
- Parallelism (rayon) only for **measured** CPU-bound hot spots, never
  by default — this repo's workloads are dominated by SSH round trips.
  Measured: parsing 10k metadata JSONs takes ~7 ms sequentially
  (release, M-series), invisible next to the network fetch. If a real
  CPU-bound workload appears (e.g. bulk `.rm` rendering), use
  `par_iter` with per-thread `fold` + `reduce` over a commutative
  merge, and quote the measurement in the PR. The `build_items`
  pipeline is already shaped for a drop-in `par_iter` if that day
  comes.
- Reusable, tool-agnostic logic goes in `libremarkable-utils/`; CLI/tool
  concerns (flags, prompting, printing) go in binary crates.
- Output discipline in binaries: **stdout is machine-usable results
  only** (trees/JSON, paths, UUIDs); progress and human status lines go
  to stderr and must respect `--quiet` and non-tty stderr.
- New binary crates: add to `[workspace] members`, register a package in
  `nix/rust-packages.nix` (copy the `rmu` block, and add any new library
  crates to `fileSetForCrate`).

### Nix

- Change pre-commit hooks via `nix/lib/git-hooks.nix`, never by editing
  `.pre-commit-config.yaml` (it is a gitignored symlink into `/nix/store`,
  generated by the flake).
- Keep flake logic in `nix/` modules; `flake.nix` stays a thin meld.
- crane cannot resolve `version.workspace = true`; package versions come
  from `commonArgs` in `nix/rust-packages.nix`.
