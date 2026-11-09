# Design: `rmu hub` — git-backed multi-tablet sync

Status: **proposed**. Nothing here is implemented yet. This document is
the plan of record for the work; update it as phases land and as device
experiments confirm or refute its assumptions.

## Goal

A git-like workflow for reMarkable tablets:

- One **origin** — a git repository on a machine you control — holds the
  canonical state of your library, with full history.
- Any number of **tablets clone** the origin. Each tablet is a working
  copy: you read, annotate, insert note pages, write notebooks, move and
  rename things on the tablet as usual.
- A tablet **syncs** by committing its changes, merging them with
  whatever other tablets pushed meanwhile, pushing the result, and
  receiving everyone else's changes.
- Every state the library has ever been in is recoverable from git.

Non-goals (for now): rendering handwriting to PDF; editing documents
from a computer other than adding new PDFs/EPUBs; stroke-level merging
inside a single page; replacing `rmu sync`, which remains the tool for
mirroring a tablet folder to and from an ordinary directory of files.

## Why git, and why rmu still has to merge

The tablet does not store zip files. Its data directory
(`/home/root/.local/share/remarkable/xochitl`) is a flat set of small
JSON files, one `.rm` stroke file per inked page, and the PDF/EPUB
payload (see `notebook-data.md`). `.rmdoc` zips are only an export
format. Mirrored file for file, this is data git stores well: inking
one page changes one `.rm` file and a few lines of JSON.

Git supplies the parts that would otherwise have to be built:
content-addressed deduplicated storage, history, SSH transport, hosting,
and — crucially — **atomic compare-and-swap ref updates**: a push that
is not a fast-forward is rejected, which is exactly the primitive
needed for many clients sharing one origin.

What git cannot do is merge this data. Its line-based text merge would
corrupt `.content` JSON and has nothing to offer for binary `.rm` files.
So rmu uses git for **storage, history and transport only** and computes
every merge itself, with rules that understand documents, pages and
folders. Git is never asked to merge; rmu writes merge commits directly.

Consistent with how rmu treats SSH (`ssh.rs`), git is driven as a
**subprocess** (`git` on `PATH`), not linked as a library: no native
dependencies, and users' git configuration and credentials work
unchanged.

## What a sample device looks like

Read-only inspection of a reMarkable Paper Pro on firmware 3.28
grounded the plan:

- Top-level entries per item UUID: `.metadata`, `.content`, `.pagedata`
  (not always present), `.pdf` (PDF documents only), `.local`,
  `.thumbnails/`, and a `<uuid>/` directory holding `<page-id>.rm`
  files. Folders have `.metadata` and a `.content` of `[]`.
- One non-UUID file, `.tree`: a binary cloud-sync cache that **contains
  cloud account identifiers**. It must never be mirrored.
- `.local` files (`{"contentFormatVersion": 2}`) are rewritten by the
  device in normal use; they look device-local.
- Every `.content` was `formatVersion: 2` with a `cPages` object
  (`lastOpened`, `original`, `pages`, `uuids`).
- Each page entry has `id`, `idx` (a fractional-index string such as
  `"ba"`, `"nnesb"` that orders pages), and optionally `redir` (index of
  the backing PDF page), `deleted`, `template`, `modifed`, `scrollTime`,
  `verticalScroll`. **Inserted note pages are the entries without
  `redir`.** A 703-page PDF with three inserted pages had 706 entries.
- Mutable values are CRDT-style registers `{"timestamp": "A:C",
  "value": ...}` where `A` is a small per-document author index and `C`
  a counter; `cPages.uuids` maps author UUIDs to those indices. On one
  device every register used author `1`, so **timestamps from two
  tablets are not comparable** without care.
- Simply opening and scrolling a document rewrites view state in
  `.content` (`lastOpened`, `scrollTime`, `verticalScroll`, zoom
  fields). Without special handling, every sync would report "changed
  on both tablets" for any document both of them opened.
- `.rm` files are `reMarkable .lines file, version=6`, from 0.4 KB to
  about 1.2 MB per page on the sample.
- The largest PDF was about 110 MB. GitHub rejects files over 100 MB,
  so a **self-hosted origin** (a bare repository over SSH, or a Forgejo
  instance with suitable limits) is the supported setup.
- A cloud-sync process was running. Cloud sync and `rmu hub` would both
  rewrite the same files; the two must not run against the same
  documents.

## Concepts

| Term | Meaning |
|---|---|
| origin | A git repository (usually bare, reached over SSH). Branch `main` is the shared library. |
| local cache | A bare clone of the origin on the machine running rmu, at `$XDG_CACHE_HOME/rmu/hub/<hash>.git`. Disposable; re-fetched if lost. |
| tablet | A working copy. Never runs git; rmu reads and writes it over SSH. |
| base | The origin commit a tablet was last synced to. Stored **on the tablet** so any computer can sync it. |
| device ref | `refs/rmu/devices/<device-id>` on the origin: the tablet's latest raw snapshot. A per-device backup that exists before any merge happens. |
| device id | A random identifier generated at clone time. Never the serial number. |

## Repository layout

```
.rmu/hub.json                 repo format version, mirror rules version
xochitl/<uuid>.metadata
xochitl/<uuid>.content
xochitl/<uuid>.pagedata
xochitl/<uuid>.pdf | .epub
xochitl/<uuid>/<page-id>.rm
xochitl/<uuid>/<page-id>-metadata.json     (older firmware)
xochitl/<uuid>.highlights/<page-id>.json
```

The mirror is an **allowlist**. Thumbnails, `.local`, `.tree`, and any
file rmu does not recognise are excluded; unrecognised files are
reported, never silently copied or deleted. The `xochitl/` prefix leaves
room for later additions such as custom templates.

On the tablet, rmu keeps its state outside the xochitl directory, at
`/home/root/.local/share/rmu/hub.json`: origin URL, device id, base
commit, and a stat manifest (path → size, mtime, blob id) of the last
snapshot so the next one only reads changed files.

## Commands

```sh
rmu hub clone <origin> [tablet]   # attach a tablet to an origin
rmu hub sync  [tablet]            # snapshot → merge → push → apply
rmu hub status [tablet]           # changes since base; origin ahead/behind
rmu hub sync --dry-run            # print the merge result and planned device writes
# later
rmu hub log [path]                # history of the library or one document
rmu hub restore <path> --at <rev> # bring back an old version as a new document
rmu hub add <file.pdf> <folder>   # add a document to the origin without a tablet
```

`[tablet]` uses the existing SSH flags (`--host`, ssh config aliases);
the default is the usual USB address. `clone` against an empty origin
creates the first commit from the tablet. `clone` against a non-empty
origin treats the tablet's existing content as a change against an
empty base and merges it — document UUIDs are random, so independent
libraries union cleanly.

## The sync cycle

```
 1. guard      refuse if cloud sync is active, the firmware/format is
               unknown, or another rmu hub sync holds the device lock
 2. stop       systemctl stop xochitl  (no page is half-written while read)
 3. snapshot   stat every allowlisted file; download only files whose
               size/mtime moved since the manifest; build a git tree
 4. commit     commit the snapshot on top of base (skip if unchanged)
 5. backup     push it to refs/rmu/devices/<device-id>
 6. fetch      fetch origin main into the local cache
 7. merge      three-way merge: base, tablet commit, origin main
 8. push       push the merge to origin main; if rejected (someone else
               pushed), go back to 6
 9. apply      write the difference between snapshot and merge to the
               tablet; .metadata is written last for new documents
10. record     update base and the manifest on the tablet
11. start      systemctl reset-failed xochitl; systemctl start xochitl
```

Safety properties this ordering is chosen for:

- **The tablet's state reaches the origin (step 5) before anything is
  changed anywhere.** A bug in merging or applying cannot lose data
  that existed only on the tablet.
- **Every step is restartable.** If the run dies after step 9 but before
  step 10, the next run sees a tablet whose changes equal origin's
  changes; "both sides made the same change" merges to a no-op, so the
  run converges instead of reporting conflicts.
- **xochitl is always started again**, including on error paths, using
  the existing `reset-failed` pattern (`Client::restart_xochitl`)
  because repeated failed starts can reboot the tablet.
- **The tablet is never written from a merge that was not pushed.** The
  origin is always at least as new as any tablet.

## Merge rules

The merge is a **pure function** — `(base, ours, theirs) → (merged
tree, notes, conflicts)` over parsed tree listings — following the
repo convention that logic that can be pure must be pure and
unit-tested. "Ours" is the tablet, "theirs" is origin `main`.

Files are grouped by **document UUID** (folders are documents too).
For each document:

| Situation | Result |
|---|---|
| Changed on one side only | That side's version, wholesale |
| Identical change on both sides | Taken once (this is what makes interrupted runs converge) |
| New on either side | Added — UUIDs do not collide |
| Deleted on one side, unchanged on the other | Deleted |
| Deleted on one side, changed on the other | **Kept** (the changed version); note |
| Deleted on both | Deleted |
| Changed on both | Per-file rules below |

When a document changed on both sides:

- **`.metadata`** — three-way merge per JSON field. Timestamps take the
  later value. Concurrent different values for the same field (renamed
  differently on two tablets) take the origin's value, with a note.
  Unknown fields are preserved, as everywhere else in rmu.
- **`.content` view state** (`lastOpened`, `scrollTime`,
  `verticalScroll`, zoom and similar fields) — never a conflict. Take
  the side with the later `.metadata` `lastModified`, falling back to
  origin.
- **`.content` page list and other fields, `.rm` files, payload** —
  phase 1: if any of these changed on both sides, the document
  **forks**. Origin keeps the UUID; the tablet's version is written as a
  new document named `<name> (conflict <device-name>)` in the same
  folder. Phase 2 (below) replaces most forks with a page-level merge.
- **Unrecognised `.content` format** — treated as opaque: both-changed
  forks.

Tree-level checks after merging, all pure:

- A document whose folder was deleted on the other side resurrects the
  folder (the deletion loses, as for documents).
- Two tablets moving folders into each other would create a cycle; the
  tablet's move is reverted, with a note.
- Trash is ordinary metadata (`parent: "trash"`); emptying the trash is
  deletion.

**Ink is never discarded by a merge.** The only automatic losses are
concurrent view-state and concurrent renames, and both are noted.

### Phase 2: page-level merge

When the same document changed on both sides:

- Pages are matched by page `id`. Pages added on either side are kept.
  Per-page registers (`deleted`, `template`, `redir`, ...) merge
  three-way; concurrent different values take the origin's value. Page
  order uses `idx`; an equal `idx` from two tablets is ordered by page
  id, deterministically.
- Each `.rm` file merges as a whole file three-way. The **same page
  inked on both sides** forks the document, as in phase 1.
- `pageCount` and similar derived fields are recomputed from the merged
  list.

This turns "I annotated chapter 1 on one tablet and chapter 3 on the
other" from a fork into a clean merge. Stroke-level merging inside a
page would need a v6 `.rm` parser in Rust and is out of scope.

## Module layout

| Module | Responsibility |
|---|---|
| `libremarkable-utils/src/git.rs` | Plumbing wrapper over the `git` binary: `init --bare`, `fetch`, `push` (with rejection detected as a typed error), `ls-tree -r`, `cat-file --batch`, `hash-object -w --stdin`, `mktree`, `commit-tree`, `update-ref`. No working tree. |
| `libremarkable-utils/src/hub/model.rs` | Pure: path classification (allowlist), document grouping, tree listings, manifests, tablet state format. |
| `libremarkable-utils/src/hub/merge.rs` | Pure: the merge rules above, heavily unit-tested. |
| `libremarkable-utils/src/hub/device.rs` | Device I/O: stop/start xochitl, lock, stat pass, batched tar download of changed files, applying a write plan. Behind a trait so tests can use a local directory as a fake tablet. |
| `libremarkable-utils/src/hub/mod.rs` | The sync cycle, composing the above; emits `Progress` steps. |
| `rmu/src/main.rs` (or `rmu/src/hub.rs`) | CLI: `rmu hub clone/sync/status`, output discipline as elsewhere (stdout = results, stderr = progress). |

Nix: `git` joins the dev shell, the check sandbox (tests create
temporary repositories), and the `rmu` package's runtime path.

## Testing strategy

- **Merge**: exhaustive unit tests with fabricated documents — every row
  of the tables above, interrupted-run convergence, cycles, resurrected
  folders, forks, view-state churn. No real notebook content in
  fixtures.
- **git.rs**: tests against temporary repositories, including a
  rejected push from a concurrent writer.
- **End to end without hardware**: two or three fake tablets (local
  directories behind the device trait) and a bare origin in a temp dir;
  scripted scenarios that clone, edit, sync in interleaved orders and
  assert convergence and no lost files.
- **Real device**, only with the owner's explicit go-ahead: read-only
  `status` first, then `clone` into a fresh origin, then a sync cycle
  with a scratch document. Results recorded in this document.

## Device questions to settle before phase 1 ships

1. Does xochitl regenerate missing thumbnails and `.local` files
   cleanly? (Assumed yes; they are excluded from the mirror.)
2. How to detect that cloud sync is active, reliably and without reading
   credentials.
3. Is `systemctl stop xochitl` followed by `start` safe to repeat within
   the start-limit window, and how long does the UI stay down for a
   typical library?
4. Do two different tablets use the same author UUID in `cPages.uuids`
   (account-scoped) or different ones (device-scoped)? This decides how
   much the register timestamps can be trusted in phase 2.
5. Does xochitl accept a `.content` whose page list was merged
   externally (new page ids, recomputed `pageCount`)?
6. Do different firmware versions write formats a merge must keep apart?
   Until known, a sync refuses to merge when the device's firmware
   differs from the version recorded by the last device that pushed,
   unless overridden.

## Implementation plan

Each item is one reviewed PR. Items on the same line can proceed in
parallel.

1. **This plan.**
2. **`git.rs` plumbing** + Nix wiring for `git` ‖ **hub model** (pure
   path classification, grouping, manifests, state format).
3. **Merge engine, phase 1** (pure; depends on the model).
   ‖ **Device layer** (stop/start, lock, stat snapshot, tar download,
   apply; local-directory fake tablet; depends on the model).
4. **Sync cycle + CLI** (`clone`, `sync`, `status`, `--dry-run`, device
   ref backup, push-retry loop) and the multi-fake-tablet end-to-end
   tests. Depends on 2 and 3.
5. **Device validation** on a real tablet, answering the questions
   above; fixes as needed. Requires the owner's go-ahead.
6. **Refactor checkpoint**: shared code between `sync` and `hub`
   (listing, stat passes, tar transfer), module sizes, naming.
7. **Phase 2 page-level merge.**
8. **`log`, `restore`, `add`.**
