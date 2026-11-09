# Design: `rmu hub` — git-backed multi-tablet sync

Status: **proposed**. Nothing here is implemented yet. This document is
the plan of record; update it as phases land and as device experiments
confirm or refute its assumptions.

## Goal

A git-like workflow for reMarkable tablets:

- One **origin** — a git repository on a machine you control — holds the
  canonical library, with full history.
- Any number of **tablets clone** the origin. Each tablet is a working
  copy: read, annotate, insert note pages, write notebooks, move and
  rename things as usual.
- A tablet **syncs**: its changes are committed, merged with whatever
  other tablets pushed meanwhile, pushed, and everyone else's changes are
  applied to it.
- Every state the library has ever been in is recoverable from git, and
  every tablet's raw state is backed up before anything is merged.

Non-goals for now: rendering handwriting to PDF; editing documents on a
computer beyond adding new PDFs/EPUBs; stroke-level merging inside one
page; replacing `rmu sync`, which stays the tool for mirroring a tablet
folder to and from an ordinary directory.

## Data-safety invariants

These hold across every design decision below; a change that weakens
one needs to say so explicitly.

1. **The tablet's state reaches the origin before anything is merged or
   written anywhere** (the device ref, step 6 of the cycle).
2. **The tablet is only ever written from a merge that origin has
   accepted.** Origin is never behind any tablet.
3. **A merge never discards ink.** Concurrent ink on the same thing
   forks; deletion loses to edits.
4. **Nothing is deleted on a large scale without explicit consent**
   (`--allow-deletions`).
5. **A snapshot that cannot be read completely and correctly is not
   committed.** Failure is closed, never partial.
6. **Every run is restartable**: a crash at any point leaves state from
   which the next run converges, without spurious or duplicate forks.
7. **xochitl comes back** even if the computer running rmu disappears.

## Why git, and why rmu still has to merge

The tablet does not store zip files. Its data directory
(`/home/root/.local/share/remarkable/xochitl`) is a flat set of small
JSON files, one `.rm` stroke file per inked page, and the PDF/EPUB
payload (see `notebook-data.md`). `.rmdoc` zips are only an export
format. Mirrored file for file, this is data git stores well: inking
one page changes one `.rm` file and a few lines of JSON.

Git supplies content-addressed storage, history, SSH transport,
hosting, and **atomic compare-and-swap ref updates**: a push that is not
a fast-forward is rejected, which is the primitive needed for many
clients sharing one origin.

What git cannot do is merge this data. Its line merge would corrupt
`.content` JSON and has nothing to offer for binary `.rm` files. So rmu
uses git for **storage, history and transport only** and computes every
merge itself. Git is never asked to merge; rmu writes merge commits
directly.

As with SSH (`ssh.rs`), git is driven as a **subprocess** (`git` on
`PATH`): no native dependencies, users' git configuration, SSH setup
and credential helpers work unchanged.

## What a sample device looks like

Read-only inspection of a reMarkable Paper Pro on firmware 3.28:

- Top-level entries per item UUID: `.metadata`, `.content`, `.pagedata`
  (not always present), `.pdf` (PDF documents), `.local`,
  `.thumbnails/`, and a `<uuid>/` directory of `<page-id>.rm` files.
  Folders have `.metadata` and a `.content` of `[]`.
- One non-UUID file, `.tree`: a binary cloud-sync cache that **contains
  cloud account identifiers**. Never mirrored.
- `.local` (`{"contentFormatVersion": 2}`) is rewritten in normal use
  and looks device-local.
- Every `.content` was `formatVersion: 2` with a `cPages` object
  (`lastOpened`, `original`, `pages`, `uuids`).
- Each page entry has `id`, `idx` (a fractional-index string such as
  `"ba"` that orders pages), and optionally `redir` (index of the backing
  PDF page), `deleted`, `template`, `modifed`, `scrollTime`,
  `verticalScroll`. **Inserted note pages are the entries without
  `redir`.** A 703-page PDF with three inserted pages had 706 entries.
- Mutable values are CRDT-style registers `{"timestamp": "A:C",
  "value": ...}`, `A` a per-document author index into `cPages.uuids`,
  `C` a counter. Every register on the sample used author `1`, so
  **timestamps from two tablets are not comparable**.
- Opening and scrolling a document rewrites view state in `.content`.
- `.rm` files are `reMarkable .lines file, version=6`, 0.4 KB to about
  1.2 MB per page on the sample.
- The largest PDF was about 110 MB. GitHub rejects files over 100 MB,
  so a **self-hosted, private origin** (a bare repository over SSH, or a
  self-hosted forge with suitable limits) is the supported setup. The
  origin holds every document name and ink stroke; it must be private.
- Public documentation states that cloud sync (`rm-sync`) starts and
  stops with xochitl, and that on the Paper Pro xochitl is involved in
  unlocking encrypted storage at boot.

## Concepts

| Term | Meaning |
|---|---|
| origin | A git repository, usually bare over SSH. Branch `main` is the shared library. |
| repo id | Random id in `.rmu/hub.json`, created by `init`. Identifies the origin independently of its URL. |
| local cache | Bare clone of origin on the machine running rmu, `$XDG_CACHE_HOME/rmu/hub/<repo-id>.git`. Disposable. Guarded by an `flock`. |
| tablet | A working copy. Never runs git; rmu reads and writes it over SSH. |
| tablet state | `/home/root/.local/share/rmu/hub.json` on the tablet (survives firmware updates; `/home` is a separate partition). See below. |
| base | Per document: the commit whose version of that document the tablet last agreed with. Usually one commit for all documents, plus overrides. |
| device ref | `refs/rmu/devices/<device-id>` on origin: the tablet's snapshot history. Fallback `refs/heads/rmu/devices/<device-id>` if the forge rejects custom refs. |
| device id / name | Random id generated at clone; a user-chosen name (`--name`, default model + short id). Never the serial or hostname. |

### Tablet state

```jsonc
{
  "format": 1,
  "repo_id": "…",              // must match origin's .rmu/hub.json
  "origin": "…",               // default URL; overridable per computer
  "device_id": "…", "device_name": "…",
  "base": "<commit>",          // default base for every document
  "base_overrides": { "<doc-uuid>": "<commit>" },
  "device_ref": "<commit>",    // last value pushed, for --force-with-lease
  "manifest": { "<path>": [size, mtime, ctime, inode, "<blob-id>"] },
  "journal": null              // present only while an apply is in flight
}
```

`base_overrides` lets one document lag behind the others: a document
the user edited during a sync, or one that a crashed apply did not
finish, keeps the version it actually has as its base. Overrides are
cleared as soon as the document is reconciled.

## Repository layout

One subtree per document, so an unchanged document is a single tree-id
comparison and `git log -- docs/<uuid>` is that document's history:

```
.rmu/hub.json                  format, repo id, mirror-rules version
docs/<uuid>/metadata           <- <uuid>.metadata
docs/<uuid>/content            <- <uuid>.content
docs/<uuid>/pagedata           <- <uuid>.pagedata
docs/<uuid>/template           <- <uuid>.template (TemplateType items)
docs/<uuid>/payload.pdf        <- <uuid>.pdf   (PDFs, and EPUB renditions)
docs/<uuid>/payload.epub       <- <uuid>.epub
docs/<uuid>/pages/<id>.rm      <- <uuid>/<id>.rm
docs/<uuid>/pages/<id>-metadata.json
docs/<uuid>/highlights/<id>.json  <- <uuid>.highlights/<id>.json
```

The mapping is a pure, tested function in both directions. The mirror is
an **allowlist**: thumbnails, `.local`, `.tree`, rmu temp files and
anything unrecognised stay on the tablet, are reported, and are never
copied or deleted by a merge.

**Carry-forward.** A snapshot is built from the base tree, replacing
only the paths this binary's allowlist covers; every other path in the
repository is carried forward unchanged. An older rmu, or one with a
narrower allowlist, therefore cannot delete what it does not understand.
rmu refuses to sync when `.rmu/hub.json`'s format or mirror-rules
version is newer than it supports. `.rmu/*` is never changed by sync;
only `init` and explicit upgrade commands write it.

## Commands

```sh
rmu hub init <path|url>          # create a bare origin (or initialise an empty one)
rmu hub clone <origin> --name N  # attach the tablet; first sync
rmu hub sync                     # the full cycle below
rmu hub backup                   # steps 1–6 only: snapshot to the device ref, no tablet writes
rmu hub status                   # read-only: changes since base, ahead/behind, open conflict copies
rmu hub sync --dry-run           # read-only: merge result and planned tablet writes
rmu hub remote [set-url <url>]   # show/change the origin URL stored on the tablet
rmu hub detach                   # forget hub state on the tablet; documents untouched
# later
rmu hub log [path]               # history of the library or one document
rmu hub restore <path> --at <rev># bring back an old version as a new document
rmu hub add <file.pdf> <folder>  # add a document to origin without a tablet
```

The tablet is chosen with rmu's existing global SSH flags (`--host`,
ssh config aliases), as for every other command. The origin URL stored
on the tablet can be overridden per invocation (`--origin`) because ssh
aliases differ between computers. URLs with embedded credentials are
rejected.

`status` and `--dry-run` never stop xochitl, take the lock, or push
anything; their snapshot is best effort and they say so.

## The sync cycle

```
 1. preflight   git ls-remote origin (auth, reachability) and a local-cache
                flock — before the tablet is touched
 2. guard       take the tablet lock; refuse if the tablet is paired with
                the reMarkable cloud; check tablet state ↔ origin repo id;
                reconcile any leftover journal (see "Restartability")
 3. stop        arm the watchdog; systemctl stop xochitl
 4. snapshot    stat every allowlisted file; download only files whose
                stat signature moved; validate (fail closed); build the
                snapshot tree with carry-forward
 5. start       start xochitl (UI is back; network work happens with it up)
 6. backup      commit the snapshot (parents: base, previous device ref);
                push the device ref with --force-with-lease
 7. fetch       fetch origin main; check ancestry
 8. merge       pure three-way merge; deletion guard
 9. push        push the merge to origin main (fast-forward only);
                on rejection go back to 7, at most N times
10. stop        arm the watchdog; systemctl stop xochitl
11. apply       write the journal; verify each tablet file still matches
                the snapshot; stage, then move into place; device sync
12. record      update base, overrides, manifest; clear the journal
13. start       systemctl reset-failed xochitl; start; disarm the watchdog
```

The UI is down only for the snapshot (4) and the apply (11), not for
network transfers. If the tablet is edited between 5 and 10, step 11's
verification notices: those documents are not overwritten, keep their
snapshot version as a base override, and are picked up by the next
sync.

### Snapshot validation (fail closed)

Step 4 aborts — nothing committed, nothing pushed — on any stat or read
error, an unparsable `.metadata` or `.content`, a zero-length `.rm`, a
document directory or page files without a `.metadata`, or a byte count
that differs from the stat pass. The error names the path.

Stat signatures are size, mtime, ctime and inode. Busybox `stat` has
whole-second resolution, so a file whose mtime is within the second of
the previous snapshot is re-read regardless (git's racy-entry rule).

### Ancestry and identity checks (step 7)

- The tablet's `repo_id` must equal origin's. Otherwise: wrong or
  replaced origin; stop.
- Every base commit (default and overrides) must exist and be an
  ancestor of origin `main`. Otherwise origin was rewound or replaced:
  stop with an explanation. Nothing is inferred automatically, because a
  rewound origin would otherwise read as "everything since was deleted".
  `rmu hub clone --reattach` is the deliberate recovery path; it treats
  the tablet like a fresh clone (below).

### Deletion guard (step 8)

The merge result is checked in both directions — documents it would
delete from origin, and documents step 11 would delete from the tablet.
Above a threshold (default: more than 10 documents and more than 10% of
the library, configurable), or whenever the snapshot is empty and the
base is not, the sync stops before pushing unless `--allow-deletions`
is given. `--dry-run` shows what would be deleted.

### Apply (step 11)

- The **journal** written first records the target merge commit, the
  snapshot commit, and per-document status.
- Before replacing or deleting a tablet file, rmu verifies it still
  matches the snapshot version (one batched `sha256sum` over just the
  files being replaced). Mismatching documents are skipped and get a
  base override.
- New and changed files are uploaded into a staging directory under
  `/home/root/.local/share/rmu/` (same filesystem), then moved into
  place in one batch. Within a document: pages and payload, then
  `.content`, then `.metadata`.
- A document deletion removes the whole `<uuid>`/`<uuid>.*` set,
  including thumbnails and `.local`. Otherwise apply never deletes files
  outside the mirror.
- View-state fields are restored from the tablet's own snapshot values
  (see "View state").
- `sync` runs on the device before step 12. `write_remote_file` is
  atomic but not durable on its own.

### Watchdog and lock

- **Watchdog**: before each stop, rmu arms a device-side timer that
  restarts xochitl (with `reset-failed` first) after a timeout, e.g.
  `systemd-run --on-active=… --unit=rmu-hub-watchdog`, falling back to a
  detached `sleep` if `systemd-run` is unavailable. It is re-armed as the
  run progresses and disarmed at the end. A vanished laptop, killed rmu,
  or dropped link leaves the UI down for at most the timeout.
- **Lock**: an atomic `mkdir` under `/home/root/.local/share/rmu/`
  holding host, pid, start time and `/proc/sys/kernel/random/boot_id`.
  It is stale only if the boot id changed or the watchdog fired; anything
  else needs `--break-lock`. No time-based staleness (tablet clocks
  drift).
- Other mutating rmu commands (`upload`, `mv`, `rm`, `rename`, `sync`,
  `restart`) refuse while the hub lock is held, and `rmu sync` warns
  when its target tablet is attached to a hub (fresh-UUID copies would
  become duplicates in the library).

### Restartability

- **Leftover journal** (crash during or after apply): before
  snapshotting, each journaled document is checked. If every file equals
  either its snapshot version or its merge version, the document counts
  as applied: its base becomes the merge commit. Otherwise its base
  becomes the snapshot commit (override). The journal is then cleared.
  This prevents both "changed on both" forks of half-applied documents
  and spurious forks when another tablet pushed in between.
- **Lost push acknowledgement or crash before apply**: if the new
  snapshot tree equals the device ref's tree on the same base, that
  commit is reused. If it is already an ancestor of origin `main`, the
  merge is skipped and origin `main` is applied directly.
- **Deterministic forks**: a fork's UUID is UUIDv5 over (original
  document UUID, tree id of the forked version). Recomputing a merge
  recreates the identical fork, which merges as an identical add —
  never a duplicate.
- **Identical changes** on both sides merge to one, so a run that died
  after pushing converges.

## Merge rules

The merge is a **pure function** — `(base, ours, theirs) → (merged
tree, notes, conflicts)` over parsed trees — following the repo rule
that logic which can be pure must be pure and unit-tested. "Ours" is the
tablet snapshot, "theirs" is origin `main`, and the base is per
document. JSON is compared parsed, never as bytes.

### Definitions

- A document is **present** when its `metadata` exists. **Deleted** means
  `metadata` absent. Any other missing file is a change, not a deletion.
- A document **changed** on a side when anything other than its
  view-state fields differs from base.
- **View state** is an enumerated list of JSON paths, versioned with the
  mirror rules. Initial list, to be completed by the device survey:
  `.content`: `cPages.lastOpened`, per-page `scrollTime`,
  `verticalScroll`, `modifed`, `customZoom*`, `zoomMode`,
  `extraMetadata` (last pen, tool, colour), `lastOpenedPage`,
  `transform`, `keyboardMetadata`; `.metadata`: `lastOpened`,
  `lastOpenedPage`, `metadatamodified`, `modified`, `synced`, `version`.
- **Fork class**: every mirrored file except `metadata` and view-state
  fields — page list and other `.content` fields, `.rm` files,
  `pagedata`, highlights, page metadata, templates, payloads.

### Per document

| Situation | Result |
|---|---|
| Changed on one side only | That side's version |
| Changed identically on both | Taken once |
| New on one side | Added |
| New on both (same UUID), identical | Taken once |
| New on both, different | Treated as "changed on both" against an empty base |
| Deleted on one side, unchanged on the other | Deleted |
| Deleted on one side, changed on the other | **Kept** (changed version), note |
| Deleted on both | Deleted |
| Changed on both, differently | Per-file rules below |

"New on both" is the normal first-clone case for tablets that shared a
reMarkable cloud account, and the case after lost tablet state.

### Changed on both, differently

- **`metadata`** — three-way merge per field. Concurrent different
  values for one field (renamed differently on two tablets) take
  origin's value, with a note. Unknown fields are preserved.
  `lastModified` becomes the maximum of both. **Trash vs edit**: if one
  side moved the document to the trash (`parent: "trash"`) and the other
  changed fork-class content, the document stays out of the trash, with
  a note — edits beat deletion, including soft deletion.
- **Fork class** — phase 1: if fork-class content changed on both sides
  differently, the document **forks**. Origin keeps the UUID; the
  tablet's version becomes a new document (deterministic UUID, above)
  named `<name> (conflict <device-name> <yyyy-mm-dd>)` in the same
  folder, with a short id suffix if that name is taken. Phase 2 replaces
  most forks with a page-level merge.
- **Units**: an EPUB's generated `payload.pdf` and the `.content` layout
  fields (`fontName`, `margins`, `lineHeight`, `textScale`,
  `textAlignment`, `orientation`) are fork class and move together with
  the ink positioned on them.
- **`tags`, `pageTags`** — merge as sets.
- **Unknown `.content` format** (unrecognised `formatVersion`, `.rm`
  header, or structure) — opaque: changed on both differently forks.

### Tree-level rules, after per-document merging

- **Orphans**: a present item whose parent is missing gets its whole
  ancestor chain resurrected from the newest version in history; with no
  such version it is moved to the root, with a note. A resurrected folder
  that was in the trash comes back to its last non-trash location.
- **Cycles**: folder moves on two tablets that would form a cycle —
  the tablet's move is reverted, with a note.
- **Trash** is ordinary metadata; emptying the trash is deletion.

### Per-document format gating

Instead of blocking mixed firmware wholesale, each document is merged
only if both sides' formats are known to this rmu (`formatVersion`,
`.rm` header version, recognised structure); otherwise it is treated as
opaque. Commit trailers record each device's firmware. A firmware update
that migrates every `.content` makes the next sync touch the whole
library: sync all tablets before updating any of them.

### Phase 2: page-level merge

When fork-class content changed on both sides:

- Pages are matched by page `id`; pages added on either side are kept.
  Per-page registers merge three-way; concurrent different values take
  origin's value, except that **ink beats page deletion** (a page deleted
  on one side and inked on the other is kept). Equal `idx` values from
  two tablets are ordered by page id.
- Each `.rm` file merges as a whole file three-way. The same page inked
  on both sides forks the document, as in phase 1.
- `pageCount` and other derived fields are recomputed.
- `cPages.uuids` author indices may need renumbering when two tablets'
  author lists differ. v6 `.rm` scene items carry author indices too, so
  renumbering must cover `.rm` files or be avoided; the device survey
  decides which (question 4).

## Git details

- **Commits**: snapshot commits have parents (base, previous device ref),
  so the device ref always fast-forwards and older snapshots stay
  reachable. Merge commits have first parent = previous origin `main`,
  second parent = snapshot; `git log --first-parent main` shows one entry
  per sync. Notes, conflicts, device name and firmware go in trailers.
  When origin has not moved, the snapshot fast-forwards `main`.
- **Identity**: author = device name, committer = rmu, set through
  `GIT_AUTHOR_*` / `GIT_COMMITTER_*`; never auto-detected (that would
  record a hostname). Commits are unsigned.
- **Environment while the tablet is stopped**: `gc.auto=0`,
  `maintenance.auto=false`, `core.hooksPath` disabled for rmu's own
  pushes, `GIT_TERMINAL_PROMPT=0` without a tty. `git maintenance run
  --auto` runs at the end.
- **Bulk objects**: snapshots are written with `git fast-import` (blobs,
  trees and the commit in one stream) fed by a **streaming** tar reader.
  The existing tar download (`run_checked_bytes`) buffers in memory, which
  does not scale to a multi-GB first clone, so a streaming path is new
  work.
- **Remote errors** (size limits, server hooks) become typed errors that
  name the offending path.
- **Growth**: payloads never change after import, so each is stored once.
  A large PDF's `.content` (order of 100 KB for 700 pages) changes with
  every non-view edit; pack deltas absorb it after gc. History must never
  be rewritten to shrink the repository: it would invalidate every
  tablet's base, which the ancestry check then refuses.
- **Device refs** are retired with `rmu hub detach --prune-ref` (or by
  hand). Concurrent first clones into an empty origin race on the first
  push; the loser falls through to the non-empty path.

## Clone

`rmu hub clone <origin> --name N`:

- **Empty origin**: the tablet's snapshot becomes the first commit.
- **Non-empty origin**: per-document base inference. For each tablet
  document, if its normalised version matches a version of that document
  anywhere in origin's history, that commit is its base; the tablet then
  fast-forwards instead of forking. Otherwise its base is empty (rules
  for "new on both"). Documents only on the tablet are added; documents
  only on origin are applied to the tablet.
- `--reattach` runs the same inference for a tablet whose state was lost
  or whose origin was replaced.

## View state

View state is **device-local**: it never counts as a change, never forks,
never resurrects a deleted document, and on apply each tablet keeps its
own values for existing documents. Documents new to a tablet get the
values stored in origin, minus model-specific zoom fields (device
pixels, which differ between models). "Resume where I left off on the
other tablet" is a possible later feature; it would need comparable
clocks and per-model translation.

## Module layout

| Module | Responsibility |
|---|---|
| `libremarkable-utils/src/git.rs` | Plumbing over the `git` binary: `init --bare`, `ls-remote`, `fetch`, `push` (rejection as a typed error, `--force-with-lease`), `ls-tree`, `cat-file --batch`, `fast-import`, `commit-tree`, `merge-base --is-ancestor`, `update-ref`. Explicit environment, no working tree. |
| `libremarkable-utils/src/hub/mod.rs` | Module root (all submodules declared from the start); the sync cycle composing the parts below; `Progress` steps. |
| `libremarkable-utils/src/hub/model.rs` | Pure: layout mapping, allowlist classification, view-state paths, document grouping, carry-forward, tablet state and journal formats, snapshot validation. |
| `libremarkable-utils/src/hub/merge.rs` | Pure: merge rules, deletion guard, fork naming and UUIDs, orphan/cycle checks. |
| `libremarkable-utils/src/hub/device.rs` | Device I/O behind a trait: lock, watchdog, stop/start, stat pass, streaming download, verify, staged apply, journal. A local-directory fake implements the trait, with fault injection. |
| `rmu/src/hub.rs` | CLI for `rmu hub …`; stdout = results, stderr = progress, as elsewhere. |

## Testing strategy

- **Merge**: exhaustive unit tests with fabricated documents — every row
  above, deletion guard, carry-forward, orphans, cycles, forks and fork
  determinism, view-state churn, "new on both".
- **git.rs**: tests against temporary repositories, including a
  concurrent writer causing push rejection. Tests are hermetic:
  `GIT_CONFIG_GLOBAL=/dev/null`, `GIT_CONFIG_NOSYSTEM=1`,
  `HOME=$TMPDIR`, explicit identity.
- **End to end without hardware**: two or three fake tablets and a bare
  origin in a temp dir. Scripted scenarios clone, edit and sync in
  interleaved orders and assert convergence and no lost files. Fault
  injection crashes the run at every step (including a lost push
  acknowledgement and a half-finished apply) and asserts the next run
  converges without spurious or duplicate forks.
- **Real device**, only with the owner's explicit go-ahead.

## Device questions

Read-only (answered by the survey PR, before the merge engine):

1. How to detect cloud pairing without reading credentials (e.g. check
   on the device whether the token keys in `xochitl.conf` are non-empty,
   printing only a boolean).
2. Is `systemd-run` available for the watchdog? Is
   `/proc/sys/kernel/random/boot_id` present?
3. The complete set of `.content` and `.metadata` fields on current
   firmware, to finalise the view-state list; `formatVersion` and `.rm`
   header versions present.
4. `cPages.uuids` and `.rm` author indices: what one device writes, so
   phase 2 can decide on renumbering. (Two-device behaviour needs a
   second tablet.)

Needing writes (answered during device validation, with go-ahead):

5. Does xochitl regenerate missing thumbnails and `.local` files?
6. How long is the UI down for snapshot and apply on a real library? Is
   stopping xochitl safe on the Paper Pro with encrypted storage, and
   within the start-limit window when repeated?
7. Does xochitl accept a `.content` whose page list was merged
   externally?

## Decisions taken (owner may revise)

- The tablet that syncs second after a same-page conflict gets origin's
  version under the original name and its own as a conflict copy;
  `status` lists open conflict copies.
- View state is per tablet.
- An edit pulls a document out of the trash.
- Phase 1 is validated on the Paper Pro; other models are handled by
  per-document format gating but untested until hardware is available.
- The origin must be private and self-hosted.

## Implementation plan

Each item is one reviewed PR; items on the same line run in parallel.

1. **This plan.**
2. **Read-only device survey**: answer questions 1–4 against a real
   tablet without writing to it; record findings in `notebook-data.md`
   and this document. ‖ **Nix and test plumbing**: `git` in the dev
   shell; a `cargoTest` check for the whole workspace in
   `nix flake check` with `git` in `nativeCheckInputs`; the `rmu`
   package wraps `git` with `--suffix PATH` so the user's own git wins.
3. **`git.rs`** ‖ **hub model** (`hub/mod.rs` with every submodule
   declared, `model.rs`, tablet state formats; any new workspace
   dependencies such as `uuid` v5 are added here).
4. **Merge engine, phase 1** (depends on 2 and 3) ‖ **device layer**
   with the fault-injecting fake tablet (depends on 3).
5. **Sync cycle** in the library, with the multi-fake-tablet end-to-end
   and crash-at-every-step tests.
6. **CLI** (`init`, `clone`, `sync`, `backup`, `status`, `--dry-run`,
   `remote`, `detach`), output discipline, progress; README and
   AGENTS.md module map.
7. **Device validation** on a real tablet (questions 5–7) and fixes.
   Requires the owner's go-ahead.
8. **Refactor checkpoint**: shared code between `sync` and `hub`
   (listing, stat passes, tar transfer), module sizes, naming.
9. **Phase 2 page-level merge.**
10. **`log`, `restore`, `add`.**
