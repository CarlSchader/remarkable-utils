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
  (including real notebook content in test fixtures).
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
| `nix/rust-packages.nix` | crane-based Rust builds, toolchain, vendor dir, clippy check |
| `nix/checks.nix`        | `nix flake check` targets (pre-commit hooks, clippy)   |
| `nix/shells.nix`        | Dev shell (rust toolchain, treefmt, openssh, hooks)    |
| `nix/formatter.nix`     | treefmt wrapper (`nix fmt`)                            |
| `nix/lib/treefmt.nix`   | Formatter config: rustfmt, alejandra, statix, deadnix  |
| `nix/lib/git-hooks.nix` | Pre-commit hooks: treefmt + clippy (offline vendored)  |
| `libremarkable-utils/`  | Library crate (see module map below)                   |
| `rmu/`                  | `rmu` CLI binary crate (clap; all config via flags, no config file) |
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
  password travels via environment, never argv.
- `xochitl.rs` — on-device file formats and **pure** tree logic (path/UUID
  resolution, ambiguity rejection, conflict/cycle checks, rendering).
  Keep this module I/O-free so it stays unit-testable without a device.
- `client.rs` — high-level operations composing the two. `list_items`
  fetches all metadata in a single SSH round trip (batched remote script
  with a per-call random marker); keep new operations batched too.
- `epub.rs` — minimal EPUB 3 generation for `.md`/`.txt` imports (the
  device renders only notebooks/PDF/EPUB). Pure bytes-to-bytes;
  raw HTML in Markdown is escaped, never passed through (xochitl's
  EPUB reader needs well-formed XHTML). See `docs/text-import.md`.
- `progress.rs` — observer trait for progress events (`step`, `bytes`,
  `bytes_done`, `finished`). The **library never prints**; `rmu` renders
  these as indicatif bars on stderr. New long-running `Client`
  operations must emit steps and call `finished()` on completion.
- `sync.rs` — `rmu sync`: scp-style endpoint parsing, the `FsEndpoint`
  trait (`LocalFs`, `SshFs`), snapshots, sync-state files, **pure**
  planners (heavily unit-tested), and thin executors. Three pairings:
  fs↔tablet (its own asymmetric decision table: conversions, rmdoc
  pull-only, text-import one-way), and fs↔fs + tablet↔tablet (one
  shared symmetric table, `decide_pair`). Keep planner logic pure; see
  `docs/sync-design.md` for semantics and state-file placement rules.
- `error.rs` — typed errors (`thiserror`).

## reMarkable domain knowledge

Read `docs/notebook-data.md` first — it documents the storage model,
`fileType` nuances, the `.rm` page format, and `.rmdoc` bundles.
`docs/text-import.md` covers how `.md`/`.txt` uploads work (host-side
EPUB conversion) and their sharp edges.

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
- Uploads accept `.pdf`/`.epub` payloads, `.rmdoc` bundles, and
  `.md`/`.txt` (converted to EPUB host-side); reject other types.
  Bundle restores must always use a **fresh UUID** (the original may
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
cargo build --workspace            # build everything
cargo test --workspace             # run tests
cargo clippy --all-features --all-targets -- -D warnings  # mandatory lint
nix fmt                            # format (rustfmt + Nix formatters)
nix flake check                    # full check: pre-commit hooks + clippy
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

## Testing against real devices

- Unit tests never require a device; keep it that way.
- Do **not** run destructive commands (`rm`, `mv`, `rename`, `upload`)
  against a user's tablet unless explicitly asked. The safe manual
  smoke-test order is: `ls` → `mkdir` → `upload` → `download` → `mv` →
  `rename` → `rm` on a scratch folder.
- Ambiguous logical paths must be rejected, never guessed — this is a
  data-safety invariant, not a convenience choice.

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
