# Design: `rmu hub` — git-backed multi-tablet sync

Status: **phase 1 implemented** (git plumbing, data model, merge
engine, device layer, sync cycle with clone, backup, status and
`--dry-run`, the `rmu hub` CLI, the hub lock for other `rmu`
commands, and `log`, `restore` and `add`) and **validated on one Paper
Pro** (see "Device validation results"; `log`, `restore` and `add`
landed after it); the bugs found there are listed with the results.
This document is the plan of record; update it as phases land and as
device experiments confirm or refute its assumptions.

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
   written anywhere** (the device ref, step 7 of the cycle).
2. **The tablet is only ever written from a merge that origin has
   accepted.** Origin is never behind any tablet.
3. **A merge never discards ink.** Concurrent ink on the same thing
   forks; deletion loses to edits.
4. **Nothing is deleted on a large scale without explicit consent**
   (`--allow-deletions`).
5. **A snapshot that cannot be read reliably is not committed.** I/O
   failures abort the run; a single malformed document is quarantined
   (neither deleted nor changed), never guessed at.
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
- Opening a document rewrites `.metadata` view state; scrolling rewrites
  view state in `.content` and moves `lastModified` (see "Device survey
  results").
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
  "base": "<commit>",          // default base for every document;
                               // null = empty base (see below)
  "base_overrides": { "<doc-uuid>": "<commit>" },  // or null = empty base
  "device_ref": "<commit>",    // written right after each device-ref push
  "manifest": { "<path>": [size, mtime, ctime, inode, "<blob-id>"] },
                               // xochitl-relative path; times in ns;
                               // racy entries left out (see below)
  "quarantine": ["<doc-uuid>"],  // malformed documents, reported every run
  "journal": null              // { merge, snapshot } from before the push
                               // attempt until the apply is reconciled
}
```

`base_overrides` lets one document lag behind the others: a document
the user edited during a sync, or one that a crashed apply did not
finish, keeps the version it actually has as its base. Overrides are
cleared as soon as the document is reconciled.

A base of `null` is the **empty base**: no common version, so the
document merges by the "new on both" rules. `clone` into a non-empty
origin uses it for documents whose version matched nothing in origin
(see "Clone"). `base`, `device_ref` and `journal` are required keys
that may be `null`: a silently missing `base` would read as the empty
base and bring back every deletion, and a missing `journal` would skip
reconciliation. Repo and device ids are random lowercase UUIDs; commit
and blob ids are 40 or 64 lowercase hex digits (uppercase is refused,
not normalised); the device name is 1 to 64 characters without control
characters, `<` or `>` (it ends up in git identities, fork names and
trailers); manifest keys must be safe
relative paths. A newer `format` is refused. Fields an rmu does not
know are preserved when it rewrites the file, but a field may be added
without a format bump only if it stays valid when an older rmu rewrites
the rest of the file; anything describing `manifest`, `journal` or the
bases needs a format bump.

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
an **allowlist**: thumbnails, `.local`, `.tree`, rmu temp files
(`.rmu-*`) and anything unrecognised (including UUIDs not in canonical
lowercase form, and page ids outside `[A-Za-z0-9_-]`) stay on the
tablet, are reported, and are never copied or deleted by a merge.
Repository paths outside `docs/<uuid>/` and `.rmu/` (a hand-committed
README, say), and paths inside a document subtree that the active
rules do not cover, are never applied to a tablet; they are carried
forward unchanged. Only regular (and executable) entries count as
mirrored files; a symlink or gitlink at a mirrored name is treated the
same way. Paths must be UTF-8: a non-UTF-8 path in a tree stops the run
with an error naming it, because converting it lossily would rename,
and so in effect delete, a carried-forward path.

**Carry-forward.** A snapshot is built from the base tree, replacing
only the paths the allowlist covers; every other path in the
repository is carried forward unchanged. An older rmu, or one with a
narrower allowlist, therefore cannot delete what it does not understand.
Deleting a document removes its whole `docs/<uuid>/` subtree,
carried-forward paths included; a subtree that is not a document (no
`metadata`, e.g. written by another tool) is carried forward. A
carried-forward path that collides with a mirrored path (equal, or one
a directory prefix of the other) is dropped, since the tree could not
hold both.

**Rules version.** rmu always mirrors with the **repository's**
mirror-rules version, never its own newest: otherwise a newer binary
would commit a file kind that older binaries never apply, and the next
snapshot of such a tablet would read it as deleted. rmu refuses to sync
when `.rmu/hub.json`'s format or mirror-rules version is newer than it
supports. `.rmu/*` is never changed by sync; only `init` and an explicit
upgrade command (run once every computer has a new enough rmu) write it.

## Commands

```sh
rmu hub init <path|url>          # create a bare origin (or initialise an empty one)
rmu hub clone <origin> --name N  # attach the tablet; first sync
rmu hub sync                     # the full cycle below
rmu hub backup                   # steps 1–7 only: snapshot to the device ref, no tablet writes
rmu hub status                   # read-only: changes since base, ahead/behind, open conflict copies
rmu hub sync --dry-run           # read-only: merge result and planned tablet writes
rmu hub remote [set-url <url>]   # show/change the origin URL stored on the tablet
rmu hub detach                   # forget hub state on the tablet; documents untouched
rmu hub log [path|uuid]          # history of the library or one document
rmu hub restore <path|uuid> --at <rev>  # bring back an old version as a new document
rmu hub add <file.pdf|file.epub> [folder]  # add a document to origin without a tablet
```

The tablet is chosen with rmu's existing global SSH flags (`--host`,
ssh config aliases), as for every other command. The origin URL stored
on the tablet can be overridden per invocation (`--origin`) because ssh
aliases differ between computers; `init`, `clone` and `remote set-url`
take their origin as an argument and ignore it. URLs with embedded
credentials are rejected. `init` creates a bare repository when given a
local path that does not exist yet (git's rule: no `scheme://`, and no
`host:` before the first `/`), and removes what it created if it fails.

`status` and `--dry-run` never stop xochitl, take the lock, or push
anything; their snapshot is best effort and they say so. `log`,
`restore` and `add` work on origin alone and need a tablet only for
its stored URL (see "History, restore and add").

## The sync cycle

```
 1. preflight   git ls-remote origin (auth, reachability) and a local-cache
                flock — before the tablet is touched; fetch origin main
                and the device ref; read the repository's mirror rules;
                check the repo id and that every base is in origin's
                history; pre-read the tablet while xochitl still runs
 2. guard       take the tablet lock and arm the watchdog; refuse if the
                tablet is paired with the reMarkable cloud; re-read the
                tablet state; fetch origin again, so reconciliation never
                judges a lost push acknowledgement against a stale cache
                (and a run that held the lock just before cannot make a
                newer base read as a rewound origin)
 3. stop        (re-)arm the watchdog; systemctl stop xochitl
 4. reconcile   if a journal is left over, reconcile it now, while stopped
                (see "Restartability")
 5. snapshot    stat every allowlisted file; download only files whose
                stat signature moved; validate; build the snapshot tree
                with carry-forward
 6. start       reset-failed + start xochitl (UI back; network work
                happens with it up, the watchdog re-armed before each
                long step)
 7. backup      commit the snapshot (parents: base, previous device ref);
                push the device ref; record device_ref on the tablet
 8. fetch       fetch origin main; check identity and ancestry
 9. merge       pure three-way merge; deletion guard
10. journal     write { merge, snapshot } to the tablet state
11. push        push the merge to origin main (fast-forward only); on
                rejection go back to 8 (rewriting the journal), at most
                N times; nothing when the tablet already matches origin
                or has no changes of its own
12. stage       upload new and changed files into the staging directory
13. stop        arm the watchdog; systemctl stop xochitl
14. apply       re-read changed metadata; verify; tree checks against the
                live tablet; move staged files into place; device sync
15. record      update base, overrides, manifest; clear the journal
16. start       reset-failed + start xochitl; disarm the watchdog and
                release the lock; then `git maintenance run --auto` on
                the local cache, still under its flock
```

The UI is down only for reconcile + snapshot (4–5) and apply (14), not
for network transfers. If the tablet is edited between 6 and 13, step
14 notices: those documents are not overwritten, keep their snapshot
version as a base override, and are picked up by the next sync.

Every `systemctl start xochitl` — steps 6 and 16, the watchdog, and
error paths — is preceded by `systemctl reset-failed xochitl.service`,
as `Client::restart_xochitl` does today.

### How the library runs it

`hub::cycle` implements the cycle (and `clone`, `backup`, `status`,
`--dry-run`) over the device layer's `Tablet` trait. Decisions made
while implementing it, beyond the steps above:

- **Fetching at step 1 always.** The snapshot is classified with the
  repository's mirror rules and carried forward from base trees, so
  both must be local before step 5; the repo id and ancestry checks then
  stop a wrong or rewound origin before the tablet is touched. Step 8
  fetches and checks again. `.rmu/hub.json` is pinned for the whole run:
  if it changes before step 8 (an upgrade landed meanwhile), the run
  stops rather than merge a snapshot classified under other rules.
- **Pre-read.** Before taking the lock, while xochitl runs, the tablet is
  read best effort (files that change while read are left out) into a
  `fast-import` of its own, committed apart (nothing points at that
  commit). The stopped read of step 5 then downloads only files whose
  stat moved since, so a tablet with gigabytes of new data (a first
  clone) is down for seconds, not for the whole download. A pre-read
  entry counts only with a nanosecond mtime (a coarse one could hide a
  change within its second) and only if its blob is in the cache;
  everything else is read again while stopped. The pre-read checkpoints
  its `fast-import` about every 64 MiB (`HubOptions::pre_read_checkpoint`)
  and saves what is durable (each file's stat and blob) next to the
  cache, so a cut — even mid-file, which ends the `fast-import` — loses
  at most that much plus the file being read: in this run, and in the
  next, whose pre-read and stopped read both count the saved files as
  known (checked like any other entry; a pre-read that fails outright
  keeps what was saved, and the run goes on without it, unless the tablet
  stopped answering: see below). The saved file is the tablet's: keyed by its
  device id, or before it has one by where this computer reaches it
  (`Tablet::destination`, the SSH destination and port). A killed
  `fast-import` leaves `.keep` files on the packs its checkpoints wrote;
  an aborted writer removes them, and so does the next opening of the
  cache when rmu itself was killed, so git can repack. A tablet the run
  would refuse anyway is not read: the pairing
  check (and, for `clone`, the attached and pending-journal checks) run
  read-only first, and a tablet whose lock another run holds gets no
  pre-read unless the lock is stale by its fired watchdog or is being
  broken (a lock gone stale by a reboot cannot be told apart before
  locking; that run reads from what earlier pre-reads saved). Step 5
  reads **before** step 4 decides:
  reconciliation judges the files just read, and a document it finishes
  is read again. `status` and `--dry-run` read the same way, with the
  second read also while xochitl runs (retried once if a file changes
  while read, never after the tablet stopped answering).
- **The history index.** Searches over origin's history go through an
  index of which subtree of each document (`docs/<uuid>`) every ancestor
  of `main` holds. It is built from one `rev-list --topo-order --parents`,
  one `cat-file --batch-check` resolving each commit's `docs` tree, and
  one `diff-tree --stdin` over (first parent's `docs` tree, the commit's)
  pairs without recursion, so it lists one entry per changed document
  however many files changed in it, and every pair must come back
  (diff-tree skips an unreadable pair silently). The parsers and the
  index are pure. Base inference walks each document's distinct versions
  newest holder first and stops at the first match; the merge's history
  reads the first-parent chain from it; a document version at a commit
  the index has needs no tree read. The ancestry check uses the index,
  or one `rev-list` of `main`, not a process per base. The cache
  reader's caches are bounded (commits by count, trees by entries, JSON
  by an estimate of its parsed size, 16 times its source bytes).
  Measured, release build: building the index of two commits that each
  change all 204,000 files of 2000 notebooks of 100 pages takes 7.5 ms
  and +0.6 MB (0.42 s and +106 MB with a recursive diff); reattaching
  1000 documents over 2400 commits on a fake tablet takes about 1.3 s
  and +21 MB (10.5 s and +239 MB with a tree read per commit and
  document), of which base inference is about 0.3 s, and the same
  reattach with a one-merge history 0.38 s. The index is built per run
  when needed (inference, an orphan's history, a document that comes
  back); keeping it next to the cache and extending it from its last
  tip is a follow-up.
- **A clone fetches the library once.** Each cache records the origin
  URLs it was opened with (`rmu-origin` in it, the eight most recent:
  one origin is often reached through several). `clone` fetches into a
  cache this computer keeps for its URL, if that cache's object format
  is the one origin advertises (another is skipped, never re-created: it
  may hold the last copy of a replaced origin's history) and origin's
  `main` still has its repo id: origin's `main` is fetched into a
  separate ref first, and the cache's own `main` moves only once the id
  matches. If it does not, that cache forgets the URL (its `main`
  untouched), and what it fetched is borrowed (through `alternates`,
  then repacked) by the temporary repository that becomes the new
  cache, so the new library crosses the network once; other caches
  recorded for the URL then only forget it, without a fetch, unless one
  has the new repo id. With no
  recorded cache, `main` is fetched once into a temporary repository,
  which is renamed into place (or fetched from locally into an existing
  cache with its repo id). A cache that holds refs is never deleted: one
  whose object format differs from origin's is refused with an error
  naming its directory (a repository cannot change format in place, so
  another or a converted repository answers there), and a cache that
  fails to open is an error, not a reason to re-create it. Measured with pack bytes
  (`GIT_TRACE_PACKFILE`): a first clone on a computer with no cache, and
  the first clone after origin was replaced at a recorded URL, each
  fetch 2.10 MB for a 2.10 MB library; a reattach to an up-to-date cache,
  through any recorded URL, fetches nothing. Opening a cache under its
  `flock` also removes the `.keep` files a killed `fast-import` left
  there (their content is exactly `fast-import`).
- **Pure work after the start.** Validation, carry-forward, and (for
  `clone`) base inference run after step 6; while xochitl is stopped
  only the stat pass and downloads (streamed into `fast-import`) happen.
- **The watchdog stays armed** from step 2 to step 16 and is re-armed
  (a fresh timer) before every step that may take long: the device-ref
  push, each fetch, each staging batch, each stop. Disarming after step
  6 would leave a crash in the network phase with a live-looking lock
  that only `--break-lock` frees; armed, it fires (a no-op `start` of a
  running xochitl plus the marker), the lock goes stale and the next run
  takes it over. If it fires while this run is still working, the next
  guarded command fails (`WatchdogFired`) and the run ends; the journal
  covers that like any crash. Staging is uploaded in batches of about
  64 MiB, re-arming before each, so the timeout bounds a batch rather
  than a whole first-clone upload (which would otherwise fire it on
  every attempt). Pushes and fetches are incremental, so one that
  outlasts the timeout is not repeated by the next run. Starts per
  cycle: two (one when nothing is applied), plus one if the watchdog
  fires, each after `reset-failed`.
- **A tablet that stops answering** (asleep, off the network) is
  `Error::Unreachable`: ssh gets a connect timeout and a keepalive (see
  `ssh.rs`), and every further try would wait for another timeout, so
  nothing retries it. The pre-read, otherwise best effort, ends the run
  on it, and so do a failed staging cleanup before step 16 and the lock
  (which otherwise retries once to adopt a lock whose acknowledgement
  was lost). The error path stops at its first unanswered step: xochitl
  comes back with the watchdog, and the lock goes stale when it fires.
  A lock taken but not yet guarded by a watchdog, or left after the
  final disarm, needs `--break-lock` (or a reboot), as after a crash.
  `rmu hub` exits with status 8. Keep the tablet awake during long
  syncs.
- **Updating `main`.** If the merge result is origin's tree and every
  document of the snapshot is equivalent to origin's (`.metadata`/
  `.content` compared normalised, other files by blob), nothing is
  pushed (`UpToDate`): the merge may have taken the tablet's bytes for a
  document whose difference is view state, and pushing that would make
  two tablets push each other's view state on every sync. If the merge
  result is origin's tree and every document of the snapshot is
  equivalent to its own base version (the tablet has no changes of its
  own), nothing is pushed either (`Pulled`): origin's changes are
  applied as they are (see "Pull-only runs" below). Otherwise, if the
  merge result is exactly the snapshot, origin has not moved since the
  snapshot's base, and no trailer is needed, the snapshot fast-forwards
  `main`. Otherwise a merge commit is pushed, even when its tree equals
  origin's: it makes the snapshot an ancestor of `main`, which every
  base override pointing at the snapshot needs.
- **Pull-only runs.** A run whose snapshot is equivalent to its virtual
  base adds no merge commit to `main` (one per pull would double
  first-parent history). The journal is still written before the apply,
  as `{ merge: origin main as fetched, snapshot }`. Every override such
  a run (or its reconciliation) would point at the snapshot points at
  the document's previous base instead, which holds the same version
  and, unlike the snapshot, is on `main`: documents skipped at step 14,
  and, when reconciliation finds the journaled snapshot is not an
  ancestor of the journaled merge, documents kept at their snapshot
  version or changed since. The snapshot is still committed and pushed
  to the device ref at step 7: the pull overwrites the tablet's files,
  and "equivalent" ignores view state and derived fields, so the device
  ref is the only place those bytes stay; and a pull-only journal names
  the snapshot, which another computer reconciling it must be able to
  fetch. A device ref already holding the same tree is reused, but that
  only defers the push: the next run's snapshot (the pulled state plus
  the tablet's own view state) differs from the last one, so every pull
  adds one small commit to the device ref. And at the tablet's next
  fast-forward or merge, its snapshot's device-ref parent brings that
  chain of pull snapshots into `main`'s ancestry, where base inference
  searches it (cheaply, through the history index). What a pull-only
  run saves is the merge commit on `main`'s first-parent history.
- **A lost push acknowledgement** (any push error but a rejection) is
  resolved by fetching: if `main` contains the merge, the push landed
  and the run carries on to apply it.
- **Device ref reuse.** A device ref already holding the snapshot's
  exact tree is reused as the snapshot commit (nothing pushed).
- **Writes.** A `.metadata`/`.content` is written only when it differs
  from the tablet's once normalised (so derived fields such as a
  device's own `sizeInBytes` do not ping-pong between tablets); other
  files when their blob or mode differs. Files the tablet has and the
  merge lacks are removed, leftovers without a `.metadata` included once
  the document arrives — but files without a `.metadata` are touched
  only when every one already equals the merge's (what rmu's own
  interrupted write leaves); anything else may be the user's document
  that lost its `.metadata` (one in no base, after `--reattach`, say):
  it is left alone, reported as an orphan, and keeps its previous base,
  so a later repair is never read as the tablet undoing the merge.
  Staged JSON carries the view state read at step
  5; files re-read at step 14 with other view state are re-staged with
  the fresh overlay while stopped. Written bytes that are not a blob rmu
  has (overlaid JSON) are left out of the manifest and read again next
  run.
- **Step 14 checks.** Verification compares each planned document with
  what step 5 read (the same mirrored files, and every file whose stat
  moved equivalent to what was read), not with the snapshot version, so
  leftovers of an interrupted write cannot block a document forever. A
  step-5 stat with a coarse mtime in the second of the step-6 start
  counts as moved. A planned change that would carry a document the user
  touched since step 5 into the trash (origin trashed its folder) is
  skipped: on the next merge that tablet's own tree would have the edited
  document in the trash, and the edit could no longer pull it out. Tree
  checks blame only violations the live tablet does not already have.
  Fork groups are taken from this merge and from the `Rmu-Fork` trailers
  on `main` since the snapshot, so a fork a later merge re-derives as two
  unrelated documents still applies as a unit. Quarantined documents are
  never written or deleted, and keep their base version's place in the
  tree checks (a folder deleted above one counts as orphaning it).
- **Staging sentinel.** Once everything an apply of merge M needs is
  staged, rmu stages `.rmu-staged-<M>/<doc>` (empty) for every document
  the plan changes, then `.rmu-staged-<M>/done`, in one transfer, next to
  it. Staging is cleared only at step 12 of a run whose journal is
  already written, and after a recorded apply; reconciliation stages
  into its own run's directory, with a sentinel of its own, without
  clearing. The device layer lists what is still staged
  (`Tablet::staged_files`), so reconciliation knows which documents an
  interrupted run planned, and which of their files it had not moved
  yet (see "Restartability").
- **Quarantined documents never make a sync push**: the snapshot
  carries their base version, which the merge never takes over
  origin's, so they are left out of the "already agrees with origin"
  test. So are documents the apply leaves alone (files without a
  `.metadata` that are not rmu's leftovers): they keep their previous
  base, so no override points at the snapshot for them, and counting
  them would push a no-op merge commit on every sync.
- **`remote` and `detach`.** `Hub::remote` reports the stored and the
  effective origin without the lock. `Hub::set_remote_url` stores a new
  URL under the lock, after checking that it holds the tablet's
  repository (repo id, every base in `main`'s history, and the device
  ref the tablet recorded, continuing it: an origin moved with
  `git clone --bare` lacks `refs/rmu/*`, and the message says so),
  before the lock and again under it (a sync from another computer in
  between may have moved the bases), so a wrong URL fails there and not
  at the next sync. While a journal is pending, the new origin must also
  have its merge, before the lock and under it (`JournalMergeMissing`:
  only there can `sync` finish it). `Hub::detach` removes the staging
  directories and
  the hub state under the lock and leaves every document alone; with
  `--prune-ref` it then deletes the device ref on origin, leased on the
  value origin just advertised (a failure there is a warning; state
  first, so a crash in between leaves an unreferenced device ref, not a
  state whose device ref is gone). While an interrupted sync's journal
  is pending it refuses (`DetachJournalPending`: documents that apply
  was writing may be half-updated on the tablet, and `sync` would finish
  them) unless forced (`DetachOptions::force`), which discards the
  journal with a warning. A failed `init` removes the bare
  repository it created only if it has no `main` (a concurrent `init`
  of the same path may have won).
- **Cache maintenance** (`git maintenance run --auto`, the user's
  thresholds deciding whether anything happens) runs once the tablet is
  disarmed and unlocked, with the cache's `flock` still held: a slow
  repack never keeps other computers off the tablet or outlasts the
  watchdog. A failure is a warning. Each run opens the cache with one
  `ls-remote` (its advertisement picks the object format and is reused
  by the first fetch); a `clone` killed mid-fetch leaves an
  `incoming-*.git` that the next `clone` on that computer removes,
  unless a running clone holds its `incoming-*.lock`.

### Snapshot validation

- **I/O failures abort the run** — nothing committed, nothing pushed:
  stat or read errors, and byte counts that differ from the stat pass.
  The error names the path. So does a snapshot whose blob list for a
  valid document differs from the files validation saw: a lost entry
  must never read as that file being deleted on the tablet.
- **A malformed document is quarantined**, not fatal: unparsable
  `.metadata` or `.content` (or a `.metadata` that is not a JSON
  object or has no string `type`), a zero-length `.rm`, or files that contradict each other: a
  folder with payload, page or highlight files; a `DocumentType`
  without a `.content` object; `fileType` `pdf`/`epub` without that
  payload. Its base version is carried forward (neither
  deleted nor changed), it gets a base override, it is never overwritten,
  and it is reported on every run and by `status` until it parses again.
- **Files without a `.metadata`** under a UUID that has no document in
  base are an orphan — typically the by-design leftovers of an
  interrupted `rmu upload` or `rmu sync`. They are ignored and reported.
  If base does have that document, it is quarantined; it must never read
  as a deletion. Only allowlisted files count here: a leftover
  thumbnail or `.local` alone does not keep a deleted document alive.

Stat signatures are size, mtime, ctime and inode. Busybox `stat`'s `%Y`
and `%Z` are whole seconds, but `%y`/`%z` print nanoseconds on the
Paper Pro's ext4, and the signature uses those. The stat pass is one
`find . ! -type d -exec stat -c '%f %s %Y %y %Z %z %i %n' {} +` over the
data dir: nanoseconds combine `%Y` with the fraction of `%y` (no
time-zone arithmetic; the seconds fields must agree), and `%f` (raw
mode) tells regular files from symlinks. Names containing a newline
cannot be listed line by line and are never mirrored names; they are
pruned and counted. Any diagnostic from `find` or `stat` fails the pass.

Racy entries are handled as git does, by leaving them out when the
manifest is recorded (step 15); a missing entry already means "dirty,
read again next run", so no extra field is needed. An entry is racy
when its mtime is coarse (sub-second part zero, as on a filesystem or
firmware with whole-second timestamps) and falls in the same second as
its reference time, or later: xochitl could change the file again
within that second without moving the signature. The reference is the
xochitl start that follows the stat: step 6 for stats from step 5, and
the record time itself (conservative) for files apply wrote, which are
stat'ed after the write. The device layer reads the step-6 reference as
the device clock just before `systemctl start` (earlier only drops more
entries), and the record time after apply's `sync`. Nanosecond mtimes are always kept, so applied
files are not downloaded again on every sync.

### Ancestry and identity checks (step 8)

- The tablet's `repo_id` must equal origin's. Otherwise: wrong or
  replaced origin; stop.
- Every base commit (default and overrides) must exist and be an
  ancestor of origin `main`. Otherwise origin was rewound or replaced:
  stop with an explanation. (A journaled merge that is not in origin is
  different: it means the push never landed, and reconciliation drops
  the journal — unless a complete staging sentinel for it is on the
  tablet: staging only follows a push that landed, so origin was
  rewound after it did, and the run stops.) Nothing is inferred
  automatically, because a
  rewound origin would otherwise read as "everything since was deleted".
  `rmu hub clone --reattach` is the deliberate recovery path; it treats
  the tablet like a fresh clone (below).

### Deletion guard (step 9)

The merge result is checked in both directions — documents it would
delete from origin, and documents step 14 would delete from the tablet.
The sync stops before pushing, unless `--allow-deletions` is given, when
the deletions exceed a threshold (default: more than 10 documents and
more than 10% of the library, or more than 50 documents regardless;
configurable), or whenever the snapshot is empty and the base is not.
`--dry-run` shows what would be deleted.

Consent is recorded in the merge commit's trailer. Deletions that origin
has already accepted (they are in theirs, not introduced by this merge)
are applied to other tablets without asking again, so one deliberate
clean-up does not require `--allow-deletions` on every tablet — but
only deletions whose consent is recorded in a trailer. The sync cycle
collects those per document, from the commits since that document's own
base (a document with the empty base never counts: the merge reads it as
new and never deletes it from the tablet), and passes them to the merge;
anything else already deleted in origin still counts. Folders
count as documents. The percentage is of origin's library for
deletions from origin and of the tablet's for deletions from the
tablet. With `--allow-deletions`, the trailer lists this merge's
deletions from origin.

The documents listed are by definition ones the tablet no longer has
(deleted from origin) or is about to lose (deleted from the tablet), so
the refusal (`DeletionsNeedConsent`), the sync report, `status` and
`--dry-run` carry each one's `visibleName` (`names`), taken from the
trees the merge already reads: origin's, then the tablet's snapshot,
then the document's base. `status` also names the documents the tablet
deleted since its base, and `--dry-run` those it would delete.

A document this tablet's history deleted can come back: another tablet
changed it after the deletion, so its merge kept it (`KeptOverDeletion`
there). The pull that brings it back to this tablet notes it
(`ReturnedAfterDeletion`): the merge adds it (absent from the snapshot
and from the document's base), and the history index shows it present
in an earlier first-parent ancestor of that base.

### Stage and apply (steps 12–14)

- New and changed files are uploaded into a staging directory under
  `/home/root/.local/share/rmu/` (same filesystem) **before** xochitl is
  stopped, so the outage covers only verification and moves. Each run
  stages into `staging/<run-id>/`, keeping xochitl-relative paths, as one
  streamed tar; staged files are `touch`ed after extraction, so applied
  files carry device-clock nanosecond mtimes (never kept out of the
  manifest as racy). Staged files are mode 0644; the apply plan carries
  each file's size and mode (regular or executable), and executables
  get their mode before the move.
- While stopped, rmu re-reads every `.metadata` and `.content` whose
  stat moved since the snapshot.
- **Verify**: a document is applied only if its tablet files still match
  the snapshot — compared as normalised JSON for `.metadata`/`.content`
  (view-state and derived fields ignored, so a document merely being
  read is not skipped forever) and by one batched `sha256sum` for other files being
  replaced. Mismatching documents are skipped and get a base override.
- **Fork groups are one unit**: if the original is skipped, its conflict
  copy is not written either.
- **Tree checks on the live tablet**: using the re-read metadata, rmu
  computes the tablet tree as it will be after the apply and runs the
  merge's tree checks against it — no parent cycles, no folder deleted
  that has gained children, no orphans. Documents that would violate them
  are skipped with a base override.
- Staged files are moved into place in one batch script, refused while
  xochitl runs. Within a document: pages and payload, then `.content`,
  then `.metadata`. Each move is a `rename` (`mv -T`), because the
  script first checks, by device number, that staging is on the data
  dir's filesystem (BusyBox `mv` silently falls back to a non-atomic
  copy across filesystems). Mirrored files the merge dropped from a
  document that stays (a deleted page) are removed in the same phase as
  their kind.
- A document deletion removes the whole `<uuid>`/`<uuid>.*` set,
  including thumbnails and `.local`, `.metadata` first (an interrupted
  deletion leaves invisible orphans, never a half document). Otherwise
  apply never deletes files outside the mirror.
- The script first checks that every staged file is a regular file of
  its planned size (a partial upload, which BusyBox `tar -x` can leave
  with exit status 0, is never applied; nothing changes otherwise) and
  runs `sync`, so no rename can outlive its file's data in a power cut.
  A guard failure part-way (watchdog fired, lock lost, reboot) is
  reported as such and may follow documents already applied;
  reconciliation, not the error, decides what each document's files
  are. It prints each finished document, then,
  after a final `sync`, the device time and a stat of every written file
  (for the manifest), all in the same invocation.
- View-state fields of existing documents are overlaid with the values
  re-read in the stopped window (see "View state").
- `sync` runs on the device before step 15. `write_remote_file` is
  atomic but not durable on its own.
- **Manifest**: files apply did not write keep their step-5 stat; written
  files are stat'ed after the write. A manifest blob id this computer
  does not have (blobs that exist only on the tablet, e.g. view-state
  overlays) is treated as dirty and re-read next time.

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
- **A fired watchdog ends the run.** When it fires it leaves a marker;
  rmu checks the marker before every device mutation and inside the
  apply's move script itself, and aborts if it is present, so a slow but
  live run can never overlap a second run that considered the lock stale.
- **Guard.** That check (the *guard*) also requires the lock to still
  name this run: the owner record holds a random run id, and every
  mutating device command first tests, in the same invocation, that the
  marker is absent, the run id is still there, and the record carries
  the current boot id (after a reboot the run's watchdog is gone and
  xochitl may be running again, so the run must end). Taking the lock
  (fresh or over a stale one) therefore clears the marker: the old run
  is still stopped, because the lock is no longer its own. Lock operations and
  guarded commands hold an exclusive `flock` (BusyBox) on a file in
  `/home/root/.local/share/rmu/` from the guard check to their last
  change, so a takeover can never fall between a run's guard and its
  mutation; the kernel drops it when the command exits, however it ends.
  Only bounded local work runs under it, and only once the whole script
  has arrived; waiters give up after 180 s ("tablet busy"). Staging (a
  network transfer into the run's own directory) checks the guard
  without it, and the watchdog never takes it. The apply script repeats
  the guard before every document. Exceptions: releasing the lock checks
  only that it is ours, and starting xochitl is refused only by another
  run's lock: bringing the UI back stays allowed after the watchdog
  fired, after a reboot, and when the state directory is gone.
- **Device-layer details.** The owner record is `key=value` lines
  (`run`, `host`, `pid`, `started`, `boot_id`; unknown keys ignored).
  The run id belongs to the process, so a retry after a lost
  acknowledgement of the acquisition adopts its own lock, unless the
  tablet rebooted or the watchdog fired meanwhile (then it is taken over
  and reported like anyone else's). Taking over a
  stale lock is a compare-and-swap on a device-side checksum of the
  record as inspected, so two runs taking over at once cannot both win
  and even a corrupt record can be broken with `--break-lock`. A lock
  directory left without a record (power cut while acquiring) needs
  `--break-lock`. When the watchdog fires it tries to write the marker
  first, then runs `reset-failed` and `start` whether or not the marker
  could be written (`true > marker`, not the special builtin `:`, whose
  failed redirection would end the shell): a lost state directory or a
  read-only `/home` must not keep xochitl down. The systemd watchdog is
  `systemd-run --quiet --collect --unit=rmu-hub-watchdog-<token>
  --on-active=<n>s /bin/sh -c '<marker; reset-failed; start>'`, with a
  fresh unit per arming: re-arming arms the new timer first and only
  then stops the old one, so a failed re-arm leaves the old watchdog in
  place. The armed watchdog is recorded with the run that armed it.
  Stopping xochitl requires a pending watchdog armed by this run.
  Disarming is refused while xochitl is stopped (the watchdog is what
  brings it back) and only ever removes this run's own watchdog, so a
  run that took a lock over cannot disarm the previous owner's. The
  `sleep` fallback runs in its own session (`setsid`); when it wakes it
  fires unless its token file holds another token (a later arming) or
  `disarmed`. A missing token file (lost state dir) still fires.
  Arming fails unless the new sleeper is alive, and only then kills the
  old one. Disarming marks the token before killing the process group,
  which is recorded with the boot id and signalled only if it is a
  number above 1 from this boot. Limitation: where sshd kills each
  session's whole cgroup on disconnect, the sleeper dies with the
  session despite `setsid`; the systemd timer, used wherever
  `systemd-run` exists (every known model), has no such problem. The pairing check runs the survey's test
  as `t=$(sed …) || fail; printf '%s\n' "$t" | grep -qvE …`, so a
  configuration that cannot be read fails rather than reading as
  unpaired. Device scripts are piped to `sh -s` and file lists to
  `tar -T -`, never put on the remote command line (one argv string,
  128 KiB on Linux); every script that changes anything is wrapped in a
  shell function, so a truncated transfer runs nothing (and takes no
  lock). `Tablet::staged_files` runs the stat pass over the staging root
  (`<run>/<xochitl path>` per staged file, nothing if there is none); it
  only reads, so it needs no lock.
- Other mutating rmu commands (`upload`, `mkdir`, `mv`, `rm`,
  `rename`, `empty-trash`, `sync` when it writes to a tablet, and
  `restart`) refuse while the hub lock is held, and `rmu sync` warns
  when its target tablet is attached to a hub (fresh-UUID copies would
  become duplicates in the library). As built: `Client` checks before
  every write and before restarting xochitl, so every command built on
  it is covered and reads never are. The look is a read-only probe
  (`hub::device::probe_script`: the lock directory and owner record,
  the boot id, the watchdog marker, whether the state file exists) that
  rides along in the document listing a write starts from, so it costs
  no round trip there; a write whose last look is older than 30 s looks
  again, and the restart always looks afresh, in the round trip of its
  `reset-failed`. A lock counts only while the hub itself would refuse
  it (`lock::decide` without a run of its own or `--break-lock`): one
  from an earlier boot, or whose watchdog fired, cannot change anything
  any more and does not hold other commands back; one without a
  readable owner record does, across reboots too (there is no boot id
  to compare), until `--break-lock`, as for the hub. The refusal names
  the owner and the lock's age, and points at `rmu hub sync
  --break-lock` only for an owner that is gone. `rmu sync` warns only
  when its plan writes to the attached tablet, and says so when the
  plan deletes there (deletions reach the library, hence every tablet).
- **Each write checks again, in its own command.** The look alone is
  check-then-act, so every write's own shell command (the staged
  upload's final `mv`, `rm`, `tar -x`, `cp`, `mkdir`, the restart)
  carries a guard right before its commit point
  (`HubProbe::write_guard`): it takes the hub's guard `flock` (when the
  state directory exists) and exits 83 unless the lock is still as the
  look saw it (absent; or, for a stale lock, its owner record unchanged,
  compared by a checksum computed on the tablet as the hub's takeover
  does, or the lock gone). That exit makes the write look again, then refuse,
  or retry once when nobody holds the lock. A restart refused after a
  command's writes stays an error: xochitl must reload what was written,
  and only a hub sync is sure to restart it when it finishes. The hub takes its lock and
  applies under that `flock`, and an old run's last guarded script holds
  it to its end, so the check and the write cannot interleave with any
  of them: while a hub run holds the lock, nothing else writes.
- **What remains**, safely. On a tablet where no hub ever ran there is
  no state directory and so no `flock`: a write can land in the
  milliseconds after the first hub run created its state directory and
  lock, which is before that run reads the tablet, so it is covered like
  any edit made before a sync. A command with several writes can stop
  between two of them when a hub run takes the lock in between (an
  upload's payload without its `.metadata` is an orphan the hub reports
  and leaves alone). Beyond that, the hub's own guards stand regardless:
  it reads the tablet with xochitl stopped and aborts on a file whose
  size changes while read, verifies every planned document before
  writing it and skips one that changed, never plans new documents
  (uploads get fresh UUIDs), and refuses to apply while xochitl runs. No
  ink is lost either way: other commands never write the pages of an
  existing document.

### Restartability

The journal `{ merge, snapshot }` is written before **every** push
attempt (step 10), so every window in which origin may know about a
merge the tablet has not fully applied is covered: a crash before the
push, a lost push acknowledgement, a crash before or during the apply.

Reconciliation (step 4) runs **while xochitl is stopped**, because it
may write files:

- If the journaled merge is **not** an ancestor of origin `main`, the
  push never landed. The journal is dropped; the tablet's bases are
  unchanged, and its edits are picked up by this run's snapshot. If a
  run's complete staging sentinel for that merge is on the tablet, it
  did land (nothing is staged before a push lands or is confirmed):
  origin was rewound, and the run stops with `OriginRewound`.
- Otherwise, the merge is part of origin. For each document whose merge
  version differs from its snapshot version, compared as normalised JSON
  (view-state and derived fields ignored):
  - every file is at the merge version → base = merge;
  - every file is still at the snapshot version (the apply never reached
    it) → base override = snapshot: the next merge (base = snapshot,
    ours = snapshot, theirs = origin) brings the merge's change again,
    staged while xochitl runs instead of during this outage;
  - every file is at its snapshot or merge version, some at each (the
    apply stopped inside this document), and a run's complete staging
    sentinel names the document → finish applying it (verify, move,
    overlay view state, the step-14 tree checks), then base = merge; a
    deletion interrupted after `.metadata` is finished as a deletion,
    leftovers included. A document no run planned only looks half
    applied (a quarantined one that lost its `.metadata`, say) and falls
    under the next rule;
  - anything else (the user changed it since) → base override =
    snapshot, unless the apply had finished the document: a run's
    complete sentinel names it, none of the files it was to write is
    still staged by that run, and every file it was to remove is gone.
    Then the change is on top of the merge and the document takes base =
    merge; without this, an edit made after an unrecorded apply would
    read the merge as the user's own change and fork it. Likewise when
    the apply stopped inside the document and the user changed it since,
    as long as every file is at the merge version, one the merge leaves
    alone (the user's change), at the snapshot version with its write
    still staged by that run (or, for a removal, still there), or one
    the apply had written that the user changed since; and something was
    written and something is pending: the pending files are finished,
    one by one (never a file the user changed, so not the merge's whole
    version), then base = merge.

  Documents whose merge and snapshot versions agree take base = merge.
  A document is never given base = merge unless its files are at the
  merge version (or the apply demonstrably put them there and the user
  changed them since). A conflict copy is finished only together with
  its original. An override for a document the snapshot lacks stays an
  explicit override: falling back to the default base would read it as
  deleted by the tablet. For a pull-only journal (its snapshot is not an
  ancestor of its merge), "base override = snapshot" means the
  document's previous base, which holds the same version and is on
  `main`.
- The result (base = merge plus overrides, journal cleared) is written
  to the tablet before the snapshot, so a crash later in the run never
  pairs a new journal with stale bases. The run then continues with the
  snapshot.

This is what keeps a half-finished run from reverting other tablets'
edits ("base = merge" for files still at the snapshot version would
read as the tablet undoing the merge) and from forking or resurrecting
documents that another tablet changed or deleted in the meantime.

Further:

- **Deterministic forks**: a fork's UUID is UUIDv5 (fixed rmu
  namespace) over the original document UUID and a canonical text of
  the forked version's fork class: its `.content` normalised without
  view-state, derived and set fields (keys sorted, pages sorted by id),
  plus the path, mode and blob id of every other file except
  `.metadata`. The merge is pure and has no git tree ids, so this text
  stands in for the tree id. Recomputing a merge recreates the identical
  fork, which merges as an identical add — never a duplicate; a fork
  whose UUID already exists in the merged tree is not created again.
- **Identical changes** on both sides merge to one.
- **Device ref bookkeeping**: `device_ref` is written to the tablet
  state immediately after each device-ref push (in `sync` and `backup`).
  If a later push fails its lease, a remote head that descends from the
  recorded value is our own lost update and is accepted; a remote head
  that does not (the ref was rewritten, deleted and recreated, or holds
  an unrelated history) stops the run. That catches a rewritten or
  replaced device ref, not two tablets sharing one device id (a copied
  state file): every snapshot takes the remote head as a parent, so the
  other tablet's pushes always descend from the recorded value, and the
  ref interleaves both tablets' snapshots. No data is at risk either
  way (every snapshot stays reachable, and `main` is merged per
  document), and device ids are random per clone, so only a copied
  state shares one. With no recorded value, a remote head is accepted
  as our own lost first push (a clone never reuses another tablet's
  state); a recorded value with no remote ref stops the run with a
  message saying origin was probably moved without its `refs/rmu/*`.
  `clone --reattach` is the way out of both stops.
- **Known limits** (no ink is lost in any): a document the user edits
  during the window of a sync that forks it is skipped, its copy with
  it, and the next sync forks it again into a second copy (the first
  one holds the tablet's version as of the snapshot); the same happens
  when a forking sync's push lands and the run stops before its staging
  completes, and the user then edits the original; and a document edited
  after the hub state was lost has no known base, so `--reattach` treats
  it as new on both sides and forks it if it differs from origin's; a
  rename or move the user exactly reverts, after a crash between an
  apply and its record, is lost (the document reads as never reached by
  the apply, so the next merge applies the change again; metadata only);
  and a document an apply stopped inside, in which the user then changed
  a file whose write was still pending, forks (finishing that file
  would overwrite the user's change, and the snapshot base reads the
  files already written as the tablet's own change).
- **Residual risks** (theoretical; no ink is lost): a pre-read entry is
  trusted on stat equality alone, which is safe only if a file's
  download starts at least one timestamp granule after its stat (a
  jiffy on kernels before multigrain timestamps; it holds in practice
  over SSH). And with the watchdog armed through the network phase, a
  `main` push still in flight when the lock goes stale could land after
  another computer took the lock over and dropped the journal as "never
  landed"; the tablet's later edits would then fork once. That needs a
  push longer than the watchdog timeout and a second computer syncing
  the same tablet meanwhile.
- **`clone --reattach` with a pending journal** refuses
  (`JournalPending`) when the journaled merge is in the new origin's
  history: only that journal's reconciliation knows which documents an
  interrupted apply stopped inside, a fresh state would fork them, and
  `sync` reconciles it first. Any other journal (origin rewound or
  replaced, or the merge never landed) can never be reconciled: the
  reattach discards it with a warning, so a rewound origin's
  `OriginRewound` ("`--reattach` is the deliberate recovery") does not
  send the user back to `sync` in a loop.

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
  view-state and derived fields differs from base.
- **View state** is an enumerated list of JSON paths, versioned with the
  mirror rules (list confirmed by the device survey):
  `.content`: `cPages.lastOpened`, per-page `scrollTime` and
  `verticalScroll`, `customZoomCenterX`, `customZoomCenterY`,
  `customZoomOrientation`, `customZoomPageHeight`,
  `customZoomPageWidth`, `customZoomScale`, `zoomMode`, `extraMetadata`
  (whole object: last pen, tool, colour), and, not seen on 3.28 but
  written by other firmware, `lastOpenedPage`, `transform`,
  `keyboardMetadata`; `.metadata`: `lastOpened`, `lastOpenedPage`,
  `new`, `metadatamodified`, `modified`, `synced`, `version`.
- **Derived fields** are ignored when deciding whether a document
  changed and when verifying, and are never a reason to fork. They are
  taken from the side whose fork-class content wins (recomputed in
  phase 2): `.content` `pageCount`, `sizeInBytes`, `cPages.original`,
  `documentMetadata`, `cPages.uuids` (see phase 2); `.metadata`
  `lastModified`, merged as the maximum of both sides. The survey found
  that scrolling alone moves `lastModified`, so it cannot count as a
  change.
- Per-page `modifed` is **not** view state: it changes only with the
  page's ink, so it belongs to the page entry and moves with the `.rm`
  file.
- **Fork class**: every mirrored file except `metadata`, view-state and
  derived fields — page list and other `.content` fields, `.rm` files,
  `pagedata`, highlights, page metadata, templates, payloads, and paths
  carried forward that the active rules do not cover. `tags` and
  `pageTags` are not fork class: they merge as sets (below). The order
  of the `cPages.pages` array carries no meaning (`idx` orders pages),
  so a reordered array with unchanged `idx` values is no change.
- A fork-class change wins **whole**: the merged document takes every
  fork-class file, derived fields included, from the one side that
  changed it. EPUB units (below) therefore move together without a
  special case.

### Per document

| Situation | Result |
|---|---|
| Changed on one side only | That side's version |
| Changed identically on both | Taken once |
| New on one side | Added |
| New on both (same UUID), identical | Taken once |
| New on both, different | Treated as "changed on both" against an empty base |
| Deleted on one side, unchanged on the other | Deleted |
| Deleted on one side, changed on the other | **Kept** (changed version), note. Moving it to the trash alone does not count: trashed on one tablet and emptied from the trash on the other is a deletion |
| Deleted on both | Deleted |
| Changed on both, differently | Per-file rules below |

"New on both" is the normal first-clone case for tablets that shared a
reMarkable cloud account, and the case after lost tablet state.

### Changed on both, differently

- **`metadata`** — three-way merge per field. Concurrent different
  values for one field (renamed differently on two tablets) take
  origin's value, with a note (none for view-state fields, which only
  the overlay on apply cares about). Unknown fields are preserved.
  `lastModified` becomes the maximum of both, compared leniently as a
  number or a numeric string; the later side's value is kept as written.
- **Fork class** — phase 1: if fork-class content changed on both sides
  differently, the document **forks**. Origin keeps the UUID; the
  tablet's version becomes a new document (deterministic UUID, above)
  named `<name> (conflict <device-name> <yyyy-mm-dd>)` in the same
  folder, with a short id suffix if that name is taken (`<name>
  (conflict <device-name> <yyyy-mm-dd> <first 8 hex digits of the copy
  UUID>)`). The original keeps origin's fork class and the per-field
  metadata merge; the copy has the tablet's fork class and metadata,
  renamed. Copies are placed after the tree rules, next to wherever the
  original ended up, and named in UUID order, so the result is
  deterministic. Phase 2 replaces most forks with a page-level merge.
- **Units**: an EPUB's generated `payload.pdf` and the `.content` layout
  fields (`fontName`, `margins`, `lineHeight`, `textScale`,
  `textAlignment`, `orientation`) are fork class and move together with
  the ink positioned on them.
- **`tags`, `pageTags`** — merge as sets, three-way: a value survives
  when both sides have it or one side added it; a removal on one side
  wins unless the other side added it. A missing field is an empty set,
  and an empty result is written only where the winning `.content`
  already had the field (rmu's skeleton has no `tags`; xochitl adds
  `tags: []` on first open).
- **Unknown `.content` format** (unrecognised `formatVersion` or
  structure on ours' or theirs' side; an old format in the base does not
  count, so documents both tablets migrated identically after a
  firmware update are an identical change) — opaque: the enumerated
  view-state and derived paths are still stripped (reading never
  counts), but the rest is compared as it is: no set merge, no page
  sorting, no materialisation rule; changed on both differently forks.
  `.rm` header versions are not gated in phase 1: `.rm` files are
  compared as whole blobs, so their format cannot make a phase-1 merge
  wrong. Phase 2 gates them before merging pages.
- **First-open materialisation**: a `.content` counts as unchanged
  when its page list is a trivial materialisation of the reference's
  empty one (`"pages": []`): one entry per PDF page, keys only `id`,
  `idx`, `redir` and the per-page view state, `redir` equal to the
  page's position in `idx` order, a page count equal to
  `cPages.original` when that is set, and no `.rm` files. The reference
  is the base's `.content`; for "new on both" it is an empty page list,
  so two tablets that each opened the same imported PDF agree.
  A reference that is itself a trivial materialisation with the same
  number of entries and no ink counts as empty too (and so reads as the
  empty list itself): a page list `rmu hub add` wrote, materialised
  again with other ids by a tablet, is no change (see "History,
  restore and add").
  When both sides materialised, origin's page ids win, even if the tablet
  also changed the metadata (the fork class then comes from origin).
- **Unparsable JSON** stops the run: the `.metadata` of every present
  merged document (the tree rules parse them all) and any `.content` the
  merge reads. Snapshots are validated, so it can only come from a
  damaged commit.

### Tree-level rules, after per-document merging

Applied in rounds, in this order, until a round changes nothing:

1. **Trash vs edit**: edits beat deletion, including soft deletion. A
   document whose fork-class content a side changed must not end up on
   a trash path in the merged tree — unless that side's **own** tree has
   the document on a trash path (it trashed the document or an ancestor
   itself). A metadata-only change (a rename) is not an edit here.
   While such a document sits on a trash path, the nearest trashed item
   on that path (the document, or a trashed folder) goes back to its
   pre-trash parent: the one the editing side gives it, or the base's if
   that side lacks the item; then the walk continues upward, since that
   parent may be in the trash too. If the trashed item cannot go back
   (the editing side trashed it too; neither the editing side nor the
   base has a parent for it; or that parent is gone from the merged
   tree, is not a folder, or lies below the item, so restoring it would
   create an orphan or a cycle), the item just below it on the path
   moves out instead: to the editing side's
   parent, the base's, or the root, whichever is first off every trash
   path. Each move gets a note. So a folder both sides trashed stays in
   the trash, and the edited document comes out of it.
2. **Orphans**: a present item whose parent is missing gets its whole
   ancestor chain resurrected: each parent from origin's or the base's
   own version when one of them has it present outside the trash (the
   tablet may have deleted a folder origin still has), else from the
   newest version in origin's history; with no such version the item is
   moved to the root, with a note. A resurrected
   folder that was in the trash comes back to its last non-trash
   location (root if it has none). An item whose parent exists but is
   not a folder also moves to the root.
3. **Cycles**: while the folder tree has a cycle, one folder on it
   moves, with a note. In order of preference: a folder that was
   restored from the trash or resurrected goes to the root; a folder
   whose parent differs from origin's (one the tablet moved) takes
   origin's parent, if that is the root or a present folder; a folder
   origin lacks goes to the root; failing all that, the smallest UUID
   goes to the root. This terminates: each step either sends a folder
   to the root, which ends every walk, or gives a folder origin's parent,
   after which the second preference never picks it again, so there are
   at most two steps per folder. A cycle without any candidate would
   consist of origin's own edges, which an acyclic origin cannot have.

Rule 1 runs again in each round because rules 2 and 3 can put an edited
document back in the trash (a resurrected folder inside a trashed one,
a cycle broken by taking origin's parent under a trashed folder). Rule 1
only moves items to the root or to a present folder not below them, so
it creates neither orphans nor cycles: two changing rounds suffice, and
every rule has a proven step bound. Exceeding a bound, or ending with a
broken tree, is an error, and nothing is pushed.

The same checks (orphans, parents that are not folders, cycles) are a
pure function over item metadata (`hub::merge::tree_violations`), which
step 14 runs against the live tablet.

Paths outside document subtrees (`.rmu/*`, hand-committed files) are
taken from origin: snapshots carry them forward from base and sync never
changes them.

Trash is ordinary metadata; emptying the trash is deletion.

### Per-document format gating

Instead of blocking mixed firmware wholesale, each document is merged
only if both sides' formats are known to this rmu (`formatVersion` and
recognised structure of ours' and theirs' `.content`; the `.rm` header
version from phase 2 on, see "Unknown `.content` format"); otherwise it
is treated as opaque. Commit trailers record each device's firmware. A firmware update
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
- **No renumbering.** A tablet writes everything as author index `1`,
  and each `.rm` file carries its own `AuthorIds` block mapping its
  indices to UUIDs (survey, question 4). Because `.rm` files merge as
  whole files, a page stays self-consistent whichever side it comes
  from, so `.rm` files are never rewritten. In `.content`, the merged
  document keeps origin's `cPages.uuids` and register timestamps are
  taken verbatim from the side whose value wins; they are never compared
  across tablets. Whether xochitl accepts registers whose author index
  meant a different UUID on the writing tablet is checked with question
  7 before phase 2 ships.

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
  --auto` runs at the end, after the tablet lock is released.
- **Bulk objects**: snapshots are written with `git fast-import` (blobs,
  trees and the commit in one stream) fed by a **streaming** tar reader.
  The older tar download (`run_checked_bytes`) buffers in memory, which
  does not scale to a multi-GB first clone; the device layer reads the
  tar as it arrives (`SshSession::stream_stdout`) and hands each entry to
  the caller as a reader with its size.
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
  in a commit that is an ancestor of origin `main` (never a device ref),
  the newest such commit is its base; the tablet then fast-forwards
  instead of forking. Otherwise its base is empty (rules
  for "new on both"). Documents only on the tablet are added; documents
  only on origin are applied to the tablet.
- `--reattach` runs the same inference for a tablet whose state was lost
  or whose origin was replaced, keeping its device id and name.

As implemented: `clone` locks, stops, reads and restarts as a sync does
(with the pre-read), then infers bases with xochitl running: the newest
ancestor of `main` (children before parents, `rev-list --topo-order`,
through the history index; device refs are not searched as such, but
snapshots merged into `main` are ancestors, and so, through each
snapshot's device-ref parent, are the earlier snapshots of that
tablet) whose version of the document the tablet's is an unchanged
version of,
by the merge's own rule (`merge::unchanged`: view state and derived
fields ignored, tags as sets, page order free, a first-open
materialisation reading as the empty page list), is that document's
base override. The tablet's `.metadata`/`.content` are served from the
read itself (they are not committed yet). The default base is empty, so
documents only on origin are added to the tablet. The tablet state is
written before the device-ref push, so an interrupted clone continues
with `sync`. An empty origin is one whose `main` holds only
`.rmu/hub.json` (`init` writes it): the tablet's documents are then all
"added" and pushed as a merge on `init`'s commit. When two first clones
race, the loser's push is rejected and it merges with the winner's
`main` like any sync, without inferring again (an exact match would
merge identically either way). `clone` warns when the tablet's model
differs from the one model the repository's history has seen.

## History, restore and add

`rmu hub log`, `restore` and `add` work on origin through the local
cache. None of them stops xochitl, takes the tablet lock or writes to
the tablet.

**Which origin.** With `--origin <url>` no tablet is needed: the cache
this computer keeps for that URL is used, or created by fetching the
library once, exactly as `clone` finds its cache. Without `--origin`
the tablet (chosen with the global SSH flags, as for every command) is
asked for its hub state, and its stored URL and its repository's cache
are used; origin's repo id must be the tablet's (`RepoMismatch`
otherwise). `--origin` together with an explicit flag naming the tablet
(`--host`, `--user`, `--port`, `-i`, `-o`, `--password`,
`--password-file`) uses both: the URL given, and the tablet for its
base (`log`) and as the author (`restore`, `add`). The tablet is only read: its state file, and for
`restore`/`add` its model and firmware (for the trailers).

**`rmu hub log`.** Without an argument it shows the library's history:
`main`'s first-parent chain, newest first (`-n` limits the count), so
one entry per sync, `restore` and `add`, plus `init`. Each entry has
the commit, its time and author, its kind (from its subject, parents
and trailers: init, a sync's merge commit, a snapshot that
fast-forwarded `main`, restore, add, or another commit made by hand),
the device, model and firmware trailers, the merge's notes (its body),
its forks, deletions with recorded consent, restores and additions, and
the documents it added, changed (any file, view state included) and
deleted against its first parent, with their names (from each version's `.metadata`: the new one, or the
last one for a deletion). The changes come from the history index, so
no tree is read per commit: each document's touches on the chain, in
order, are the versions it moved between. With a tablet, the entry its
base points at is marked and its base overrides are counted.

With an argument, a logical path or a UUID, it shows that document's
history on the same chain, newest first: every commit where its
`docs/<uuid>` subtree changed, saying what changed (added, content
changed, renamed, moved, the trash included, deleted; a commit that
only moved view state or derived fields is left out), plus every
commit whose trailers name it (a conflict copy forked from it or it
being one, a restore made from it or it being one). A path is resolved
against `main`; a path that resolves to nothing there (the document was
deleted, renamed or moved) is resolved against each older first-parent
commit in turn, and the newest commit where it matches anything decides.
Ambiguous paths are refused, never guessed: the error asks for the UUID.
A UUID must name a document that was on `main`'s history. With a
tablet, the document's own base is reported.

**`rmu hub restore <doc> --at <rev>`.** `<rev>` is a commit id as `log`
prints it (an abbreviation is fine) or `main`, `main~N`; it must be
`main` or one of its ancestors. Merged snapshots count, so a tablet's
state from just before a merge can be restored; a commit that is only
on a device ref cannot. The path resolves at `<rev>`, falling back to
older commits as for `log`. The version restored is the document's at
`<rev>`; if it is absent there (deleted by then), the newest version
before `<rev>` on its first-parent chain, so `--at` a deletion commit
restores what was deleted. Folders are refused (their documents can be
restored one by one).

The version becomes a **new document with a fresh random UUID**; the
original, if it still exists, is never touched. Every file of its
subtree is reused as it is (the same blobs, so nothing is transferred;
page ids are scoped to the document and cannot collide), except
`.metadata`, rewritten with every unknown field kept: `visibleName`
becomes `<name> (restored <yyyy-mm-dd>)` (the UTC date of the restore,
with the copy's first 8 hex digits appended if that name is taken in
the folder, as for conflict copies), `parent` stays the original folder
if that is present on `main`, a folder and not in the trash, and becomes
the root otherwise, and `lastModified` is now. Since its UUID is new,
every tablet's next sync adds it as new on origin; no tablet can fork
it.

**`rmu hub add <file> [folder]`.** A `.pdf` or `.epub` becomes a new
document (fresh random UUID) in `folder` (a logical path or UUID on
`main`; the root by default; it must be a folder outside the trash),
named `--name` or the file's stem. A name already taken in that folder
is refused, as `rmu upload` does, and so is a `.pdf` that does not start
with `%PDF-` within its first KiB. Its files: `.metadata` as `rmu
upload` writes it (`new: true`, never opened), the payload, and
`.content`, without a `.pagedata` (firmware 3.28 writes none for
documents it creates). For a PDF whose page count can be read,
`.content` carries a **materialised page list**: one entry per page,
`id` = UUIDv5(namespace: the document's UUID, name: the page index in
decimal), `idx` = `b` followed by the index in base 26 (`a` to `z`,
fixed width: `ba`, `bb`, ... up to 26 pages), `redir` = the index,
registers stamped `1:2` as xochitl stamps them; `cPages.original` and
`pageCount` are the page count and `sizeInBytes` the payload's size. A
tablet opening it finds the list in place, and every tablet has the
same page ids. EPUBs (laid out on the device) and PDFs whose page count
cannot be read reliably get rmu's unmaterialised skeleton
(`"pages": []`), and the first-open materialisation rule keeps two
tablets that open them from forking.

**The page count, without native dependencies.** A small reader
(`pdf.rs`, pure and unit-tested) follows `startxref` to the
cross-reference data: classic tables and cross-reference streams,
hybrid files (`/XRefStm`), incremental updates (`/Prev`) and objects
inside object streams; Flate streams with PNG predictors are decoded
with `flate2`, which `zip` already brings in with its pure-Rust
`zlib-rs` backend. When the cross-reference data is unusable it
rebuilds the table by scanning for `N G obj`. It then reads `/Root`,
`/Pages` and `/Count`, and walks the page tree counting its leaves. The
count is used only when the walk agrees with `/Count`; anything else
(encrypted object streams, filters other than Flate, a damaged file)
falls back to the skeleton. `add` is fed files from anywhere, so every
count runs within limits (`pdf::Limits`), and running out of any of
them is "no count", the skeleton again: a work budget of 16 units per
byte of the file plus 64 Mi (a unit per byte searched, parsed or
decoded and per object built), so no input makes it search the rest of
the file once per object; 16 MiB per decoded stream; 32 MiB of object
streams held decoded; a million cross-reference entries across every
section and the scan, checked against the declared counts before
anything is allocated (offsets past the end of the file are dropped);
256 Ki objects per parse; a million page-tree nodes; and nesting,
reference hops, sections and tree depth. Stream data is read only for
cross-reference and object streams, and a hex string ends at its first
non-hex byte. Measured (`tests/pdf_bounds.rs`, a counting allocator):
the hostile files from review (cross-reference stream bombs, chained
sections, huge arrays in object streams, quadratic scans, a valid
table of pages followed by unterminated streams) peak at 33 MiB of heap
(67 MiB for a stream filling the entry cap) and take under 50 ms in a
release build; 12 million mutated files stayed within their limits
without a panic. The
alternatives were worse: a count given by the user can be wrong, and a
wrong list is worse than none (a list whose length disagrees with the
PDF is a change each tablet would make on its own); always writing the
skeleton gives up the deterministic ids that spare every tablet the
first-open rewrite and that phase 2 needs to match pages.

**If xochitl rewrites a page list it is given.** Device validation
found that xochitl keeps a page list changed externally on a document
it had opened (question 7); whether it keeps the list `add` writes on a
document it never opened is still to be confirmed. Should it materialise again, with ids of its own, the
first-open materialisation rule is generalised so that this is no change
either: a trivially materialised page list reads as the empty list when
its reference is empty **or itself trivially materialised with the same
number of pages** (see "Changed on both, differently"). As for the
empty list, the first tablet to sync after opening it pushes its ids
(origin did not change it, so the tablet's bytes are taken), and when
two tablets materialised it, origin's ids win; nothing forks. A list of
another length is a real change.

**Writes.** `restore` and `add` write one commit on top of origin's
`main` (single parent; its tree is `main`'s plus the new subtree) and
push it fast-forward only, as a sync pushes: on rejection they fetch,
decide the folder and the name again against the new `main`, and retry
(`push_attempts` in all); a lost acknowledgement is resolved by
fetching. `add` writes the payload and `.content` into the cache once,
before the first attempt (in a commit no ref points to). Identity is
explicit: author and committer `rmu`, or with a tablet the author is its
device name, as in its syncs. Trailers record what happened:
`Rmu-Restored: <copy> <original> <commit the version came from>` or
`Rmu-Added: <uuid>`, plus the device trailers when a tablet is given.
`.rmu/hub.json` is pinned as in a sync (if it changes between attempts,
the run stops). The next sync of each tablet applies the new document.

## View state

View state is **device-local**: it never counts as a change, never forks,
never resurrects a deleted document (also for an unknown `.content`
format: the enumerated view-state paths are stripped from it too), and on apply each tablet keeps its
own values for existing documents (re-read while xochitl is stopped). Documents new to a tablet get the
values stored in origin, minus model-specific zoom fields (the
`customZoom*` fields; the survey saw `1404`×`1872` page sizes on a
Paper Pro, so they may be a shared coordinate space, but they are
dropped until a second model confirms it). For an existing document,
each view-state path takes the tablet's value, or is removed when the
tablet's file has none; page entries are matched by `id`, and pages
the tablet does not have yet keep origin's values. "Resume where I left off on the
other tablet" is a possible later feature; it would need comparable
clocks and per-model translation.

## Module layout

| Module | Responsibility |
|---|---|
| `libremarkable-utils/src/git.rs` | Plumbing over the `git` binary: `init --bare`, `ls-remote`, `fetch`, `push` (rejection as a typed error, `--force-with-lease`), `ls-tree`, `cat-file --batch`, `fast-import`, `commit-tree`, `merge-base --is-ancestor`, `update-ref`. Explicit environment, no working tree. |
| `libremarkable-utils/src/hub/mod.rs` | Module root (all submodules declared from the start). |
| `libremarkable-utils/src/hub/cycle.rs` | The operations composing the parts below (`init`, `clone`, `sync`, `backup`, `status`, `--dry-run`, `remote`, `detach`); `Progress` steps; submodules for the serialisable reports and typed warnings (`report`), the tablet session (`session`), what a run knows about origin (`origin`), `init` (`init`), the merge a sync runs (`merge_run`), the local cache (`store`), reading the tablet (`snapshot`), the pure decisions (`plan`: equivalence, apply plan, verification, step-14 moved paths, tree and trash checks, reconciliation verdicts and finish, manifest), commit messages and trailers (`message`), the history index (`history`), the steps (`steps`, with `clone`, `reconcile`, `push` and `apply`), the read-only commands (`inspect`), `remote`/`detach` (`admin`), and `log` (`log`), `restore` (`restore`) and `add` (`add`) over what they share (`library`: origin without a sync, document resolution, the push of one new document). |
| `libremarkable-utils/src/pdf.rs` | Pure: the PDF page counter `add` uses (see "History, restore and add"). |
| `libremarkable-utils/src/hub/model.rs` | Pure: layout mapping, allowlist classification, view-state paths, document grouping, carry-forward, tablet state and journal formats, snapshot validation (submodules `ids`, `layout`, `rules`, `state`, `grouping`, `snapshot`, re-exported from `hub::model`). |
| `libremarkable-utils/src/hub/merge.rs` | Pure: merge rules, deletion guard, fork naming and UUIDs, orphan/cycle checks. |
| `libremarkable-utils/src/hub/device.rs` | Device I/O behind a trait: lock, watchdog, stop/start, stat pass, streaming download, verify, staged apply, journal and state removal, listing what is still staged. A local-directory fake implements the trait, with fault injection. |
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
  converges without spurious or duplicate forks. As built
  (`libremarkable-utils/tests/hub_e2e.rs`): every scenario that syncs to
  the end finishes with the same oracles — the tablets equal origin
  (`.metadata`/`.content` normalised, other files byte for byte,
  leftovers included; a quarantined document must still be on its
  tablet), every ink stroke ever drawn and not deliberately deleted is on
  origin and on every tablet, xochitl runs, no lock or journal is left;
  scenarios that are refused check that the tablet is untouched, running
  and unlocked. The crash scenario includes three documents the crashing
  tablet holds quarantined (one origin changed, two origin deleted, one
  of which looks exactly like an interrupted deletion), repaired at the
  end so their ink is checked too. The crash matrix
  copies one prepared world per fault and injects, in turn, a
  disconnection before every operation and after every mutation, a lost
  reply to every mutation, a failure of every operation, an apply killed
  after every move, the watchdog firing after every move, a cut of the
  stopped read after every file, and a disconnection at every operation
  of a run that reconciles an earlier crash (plus a pre-read cut inside
  a file); after each, xochitl must be
  up or have an armed watchdog, and the next run (with `--break-lock`
  only where the crash left a lock nothing makes stale) must end with
  exactly the clean run's conflict copies. A lost push acknowledgement
  is produced for real by a `reference-transaction` hook on the origin
  that kills `receive-pack` once the update is committed. The run after
  each recovery must be quiet (`UpToDate`, nothing applied); a failed
  run with a working connection must bring xochitl back itself; and
  variants of the matrix add a user edit after every crash (a base too
  old forks the tablet's own change, one too new reverts another's),
  crash a shared-library `clone` and a `backup` at every operation, and
  crash a pull-only run at every operation and apply move, and crash a
  pull-only and a merge run at every operation and apply move with the
  user editing both sides' documents before the next run. A scale test
  builds 1000 documents and 2400 commits with one `fast-import` stream
  and reattaches (a debug-sized variant runs always; the full size is
  `#[ignore]`d, run in release to measure).
- **Real device**, only with the owner's explicit go-ahead.

## Device questions

Read-only (questions 1–4 **answered**; see "Device survey results"
below):

1. How to detect cloud pairing without reading credentials (e.g. check
   on the device whether the token keys in `xochitl.conf` are non-empty,
   printing only a boolean).
2. Is `systemd-run` available for the watchdog? Is
   `/proc/sys/kernel/random/boot_id` present?
3. The complete set of `.content` and `.metadata` fields on current
   firmware, to finalise the view-state list; `formatVersion` and `.rm`
   header versions present. In particular: does opening or scrolling a
   document touch `.metadata` `lastModified`, and are any `.content`
   fields derived per device (e.g. `sizeInBytes`)? Either would defeat
   view-state normalisation (forks on first clone, resurrections after
   deletes) and must be handled in the list. Answering this needs a
   before/after comparison around opening a document on the tablet, which
   the owner can do by hand while rmu only reads.
4. `cPages.uuids` and `.rm` author indices: what one device writes, so
   phase 2 can decide on renumbering. (Two-device behaviour needs a
   second tablet.)

Needing writes (questions 5–7 **answered** during device validation;
see "Device validation results" below):

5. Does xochitl regenerate missing thumbnails and `.local` files?
6. How long is the UI down for snapshot and apply on a real library? Is
   stopping xochitl safe on the Paper Pro with encrypted storage, and
   within the start-limit window when repeated?
7. Does xochitl accept a `.content` whose page list was merged
   externally? (Answered: it does, see the results.) Still open: the page
   list `rmu hub add` writes, which validation did not cover. Its
   registers name author `1` without a `cPages.uuids` table defining it,
   and its `idx` values past 26 pages (`baa`, `bab`, ...) are rmu's own
   scheme, not checked against what xochitl writes for long PDFs.
   Neither matters to the merge (`uuids` is derived and `idx` compares
   within a document), but if xochitl rejects such a list, `add`
   switches to the skeleton.

### Device survey results

Read-only survey of one reMarkable Paper Pro on firmware 3.28.0, not
paired with the cloud, library of about 110 items (70 PDFs, 2 notebooks,
37 folders, 8 inked pages). Most PDFs had been uploaded by `rmu` and
never opened, so their files still had rmu's minimal skeleton; only 7
documents carried a `.content` written by xochitl. Field counts below
are therefore a floor, not a census of everything xochitl can write.
Details of the formats are in `notebook-data.md`.

**1. Cloud pairing.** The signal is the `devicetoken` key in
`/home/root/.config/remarkable/xochitl.conf` (`[General]`). On the
unpaired tablet it is `@Invalid()` (Qt's encoding of an unset value)
and `UserToken` is empty; xochitl's log says it cannot fetch a user
token without a device token. `user-auth/` under
`~/.local/share/remarkable` and the `.tree` file exist on the unpaired
tablet too, so neither is a pairing signal. The guard (step 2) runs this
one-liner, which prints only `true` or `false`:

```sh
sed -n 's/^devicetoken=//p' /home/root/.config/remarkable/xochitl.conf \
  | grep -qvE '^(|@Invalid\(\)|@ByteArray\(\)|"")$' && echo true || echo false
```

It printed `false` on the surveyed tablet; the `true` branch has only
been checked against synthetic input, since no paired tablet was
available. `rm-sync.service` (cloud sync) is enabled and running even
when unpaired; it has `BindsTo=xochitl.service` and
`PartOf=xochitl.service`, and xochitl `Wants=` it, so it stops and
starts with xochitl and needs no handling of its own.

**2. Device capabilities.**

- `systemd-run` exists (systemd 255) and supports `--on-active`,
  `--unit`, `--collect`, `--no-block` and `--timer-property`; the
  detached-`sleep` fallback is not needed on this model but stays for
  others. `setsid` and `nohup` exist; `timeout` does not.
- `/proc/sys/kernel/random/boot_id` exists.
- xochitl's unit: `StartLimitBurst=4` in `StartLimitIntervalSec=600`,
  `Restart=on-failure`, `WatchdogSec=60`, `JobTimeoutSec=60s`, and
  `OnFailure=remarkable-fail.service emergency.target`. Exhausting the
  start limit therefore drops to the emergency target: one sync cycle
  uses two starts (steps 6 and 16), so two back-to-back cycles inside 10
  minutes reach the limit without `reset-failed`. The existing rule
  (always `reset-failed` before `start`) is essential, and question 6
  still has to confirm it suffices.
- BusyBox 1.36 provides `stat`, `sha256sum`, `tar`, `find`, `mv`,
  `mktemp`, `flock`, `setsid`, `sync`. `stat -c` supports size `%s`,
  mtime `%Y`, ctime `%Z`, inode `%i` and device `%d`; `%y`/`%z` give
  nanosecond times (the stat signature now uses them). `tar` takes
  `-T -` and `--no-recursion` but fails on an empty file list (`tar:
  empty archive`), so the device layer must skip the call when nothing
  needs reading. `find` has `-print0`, `-exec ... +`, `-newer`, but no
  `-printf`. `mv -T`/`-n`, `mktemp -d -p` work. `sync` takes no
  arguments (flushes everything).
- `/home` is a single ext4 filesystem (an encrypted device-mapper volume
  on the Paper Pro). `~/.local/share` and the xochitl data dir report the
  same device number, so a staging directory under
  `/home/root/.local/share/rmu/` can be moved into place with `rename`.
  That directory does not exist yet on a fresh tablet.

**3. Fields and churn.** Every document `.content` is `formatVersion:
2`; every `.rm` header is `reMarkable .lines file, version=6`; every
`.local` is `{"contentFormatVersion": 2}`.

- Document `.metadata` written by xochitl has exactly `createdTime`,
  `lastModified`, `lastOpened`, `lastOpenedPage`, `new`, `parent`,
  `pinned`, `source`, `type`, `visibleName` (pdf and notebook alike). The
  folders, all created by rmu, also have `metadatamodified`, `modified`,
  `synced`, `version`; xochitl leaves them alone.
- `.content` keys on xochitl-written documents (7 of 7 unless noted):
  `cPages` {`lastOpened`, `original`, `pages`, `uuids`},
  `coverPageNumber`, `customZoomCenterX`, `customZoomCenterY`,
  `customZoomOrientation`, `customZoomPageHeight`, `customZoomPageWidth`,
  `customZoomScale`, `documentMetadata` (`title`/`authors` on 3 PDFs),
  `extraMetadata` (19 last-tool keys), `fileType`, `fontName`,
  `formatVersion`, `lineHeight`, `margins` (PDFs only), `orientation`,
  `pageCount`, `pageTags`, `sizeInBytes`, `tags`, `textAlignment`,
  `textScale`, `zoomMode`. Page entries have `id`, `idx`, and optionally
  `redir`, `modifed`, `scrollTime`, `verticalScroll`, `template`. No
  `deleted` page was present on the sample; it stays in the rules from
  earlier inspection.
- **First open rewrites the whole `.content`** of an imported document:
  rmu's skeleton has `"pages": []`, and opening it creates one entry per
  PDF page with a fresh random page id, plus `original`, `uuids`,
  `pageCount`, `sizeInBytes`, `documentMetadata`, zoom fields,
  `extraMetadata`, `tags`, and `new: false` in `.metadata`. Two tablets
  that open the same new PDF would therefore fork it. The merge engine
  must handle this (see "Follow-ups from design review").
- **Derived per device**: `sizeInBytes` (payload plus ink),
  `pageCount`, `cPages.original`, `documentMetadata`. `.local` is
  device-local. `customZoomPageWidth`/`Height` were `1404`×`1872` on a
  Paper Pro (whose screen is larger), so they look like a shared
  coordinate space; still treated as model-specific until a second model
  is seen.
- **`lastModified`**: opening a document rewrote `.metadata`
  (`lastOpened`, `lastOpenedPage`) without moving `lastModified` and
  without rewriting `.content` (two documents opened days after their
  last change: `.metadata` mtime equals `lastOpened`, `.content` mtime
  equals `lastModified`). Scrolling **does** move it: on two documents
  `lastModified` equals the newest `scrollTime` to the second while the
  newest ink is weeks older. One document had both files rewritten 26
  minutes after `lastModified` without a bump. Confidence: medium. The
  timestamps are consistent across all seven documents, but no
  controlled before/after test was done. Consequence: `lastModified` is
  a derived field, not a change signal (see "Definitions").
  `cPages.lastOpened` counters reach the hundreds on a book being read.
  `modifed` equals the page's `.rm` mtime, so it tracks ink, not reading.
- **File kinds in the data dir**: top level `<uuid>.metadata`,
  `.content`, `.pagedata` (written by rmu; notebooks created on the
  tablet have none), `.pdf`, `.local`, `<uuid>/` and
  `<uuid>.thumbnails/`, plus `.tree`. Inside `<uuid>/`: only
  `<page-id>.rm`. Inside `.thumbnails/`: `<page-id>.png`, only for the 7
  opened documents. No EPUBs, `.highlights/`, `.template` or
  `-metadata.json` files on the sample; the allowlist keeps them from
  earlier knowledge. The allowlist needs no change.

**4. Author indices.** All 7 documents with a `cPages.uuids` list have
exactly one entry, `{"first": <uuid>, "second": 1}`, and it is the same
UUID in all of them: one distinct author UUID on the tablet. Every
register timestamp uses author `1` (about 6,300); the only exceptions are
three `"0:0"` defaults (`cPages.original` on both notebooks created on
the tablet, `cPages.lastOpened` on one of them). All 8 `.rm` files start
with an `AuthorIds` block (type 0x09) holding one entry, index `1` to that same UUID. Every stroke and
tombstone id uses author `1`; the built-in root/layer references use
author `0`. Implications for phase 2: a second tablet presumably also
writes as author `1` with its own UUID, so indices collide rather than
differ, and renumbering would mean rewriting `.rm` files. It is avoided:
`.rm` files merge as whole files and carry their own author table, and
`.content` registers are never compared across tablets (see "Phase 2").
Whether the author UUID is per tablet or per account, and what xochitl
does with a merged `cPages.uuids`, needs a second tablet (question 7).

### Device validation results

Writes against the same reMarkable Paper Pro (firmware 3.28.0, not
paired with the cloud, about 110 items), with the owner's go-ahead,
after a full read-only backup of the data dir. The origin was a local
bare repository; the tablet was reached over Wi-Fi through a VPN, not
USB. Every write went into one fabricated scratch folder (one generated
one-page PDF); origin-side changes were ordinary commits pushed to the
bare repository by hand. Sequence: `init`, `clone`, a no-op `sync`;
`rmu mkdir` + `rmu upload` into the scratch folder and a `sync` up; on
origin a rename, a `sync` down; the owner opened the document, a `sync`
up; on origin one inserted page, a `sync` down; the owner opened it
again, a `sync`; on origin the document and folder deleted, a `sync`
down. Every run exited 0 with the expected report: no quarantine, no
forks, no deletion-guard refusal, no watchdog firing, no lock or
journal left behind; afterwards the tablet's listing equalled the one
from before the test and `status` was clean.

**5. Thumbnails and `.local`.** xochitl creates both **lazily, when a
document is opened**, not when it starts: the document arrived without
either (as every document applied from origin does, since neither is
mirrored), and after two xochitl restarts it still had none. On first
open `.local` (`{"contentFormatVersion": 2}`) appeared at once and a
thumbnail of the page shown about 12 s later; `.content` and
`.metadata` were rewritten when the document was closed (the first-open
materialisation, see "Device survey results"). Turning to the inserted
blank page wrote no thumbnail for it. Nothing about the missing files
was logged. A rename applied from origin did not touch thumbnails, and
the deletion applied from origin removed `.local` and `.thumbnails/`
with the rest. Not tested: deleting the thumbnails of a document that
already had them (nothing in the hub does that).

**6. Downtime and the start limit.** Times from the tablet's journal
(`Stopping` to `Started` of `xochitl.service`; `rm-sync` stops about
10 ms earlier and starts with it):

| Run | Wall time | xochitl stopped |
|---|---|---|
| `clone` (109 documents, nothing to apply) | 729 s | 0.57 s |
| no-op `sync` | 3.7 s | 0.52 s |
| `sync` pushing two new items | 4.3 s | 0.52 s |
| `sync` applying a rename | 6.7 s | 0.54 s + 1.14 s, 2.0 s apart |
| `sync` after a read-only open (view state only) | 4.3 s | 0.58 s |
| `sync` applying two deletions | 6.0 s | 0.60 s + 1.47 s, 1.4 s apart |

- The snapshot window (steps 4–5) costs about half a second when
  little changed; the apply window (step 14) 1.1–1.5 s for one or two
  documents.
- `xochitl.service` is `Type=simple`, so `Started` is only the process
  start: xochitl's own start-up (waveforms, front light, orientation)
  ends about 2.3 s later. The UI is therefore gone for about 3 s per
  stop. On a sync that applies, step 13 stops xochitl 1.4–2 s after
  step 6 started it, before it finished starting, so the user sees one
  outage of about 6–7 s (from the snapshot's stop to the end of the
  apply's start-up), not two short ones. See "Follow-ups from device
  validation".
- The clone's time was almost all step 1's pre-read: the whole library
  (about 0.8 GB, 670 MB packed) downloaded while xochitl ran, at about
  1 MB/s over Wi-Fi and the VPN. The stopped snapshot after it took
  well under a second. A first clone is best done over USB.
- Stopping xochitl is safe on the Paper Pro with encrypted storage:
  `/home` stays mounted and readable while xochitl is stopped (every
  snapshot read it in that window). Eleven hub stops, all clean.
- The start limit (`StartLimitBurst=4` in 600 s) was never hit: four
  starts within 80 s (one `rmu upload` restart and three hub starts) and
  seven within about 11 minutes (the journal of the last three had
  rotated away by the time it was read; the runs succeeded and the
  xochitl process ids show no reboot), `NRestarts` stayed 0, `Result`
  stayed `success`, and `OnFailure` never ran. `reset-failed` before every
  start is sufficient.
- Every armed watchdog timer was gone after each run (no
  `rmu-hub-watchdog-*` unit left).
- **The tablet sleeps under rmu.** It suspended about 20 minutes after
  the owner last woke it even though rmu had run over SSH and restarted
  xochitl twice in that time; neither keeps it awake. It went to sleep
  in the middle of an `rmu mkdir`, which then hung (see below). The
  owner should keep the tablet awake (and on power for a first clone).
- Side effect seen in the journal, harmless: the tablet's SSH server
  opens a login session per command, `user@0.service` fails on every
  session (it did so before rmu ran, so the system already reported
  `degraded`), and a burst of rmu's commands trips the start limit of
  `user-runtime-dir@0.service` (5 in 10 s). That unit has no
  `OnFailure`, and every command still ran.

**7. A page list changed externally.** Accepted. On origin, one
blank inserted page was added after the only PDF page of the
(materialised) document: an entry without `redir`, a fresh random
`id`, `idx` `"bb"` after `"ba"`, `template` `"Blank"`, registers as
author `1` with counters above the existing ones, and `pageCount` 2;
no `.rm` file. After the apply the document opened normally and showed
the PDF page followed by a blank page. xochitl kept the entry verbatim
(no register renumbered) and, when the owner turned to it, set
`cPages.lastOpened` to the new page with counter `"1:2"`, below the
entry's own `"1:3"`/`"1:4"`: counters are not one clock per document.
The next sync pushed nothing (view state only) and quarantined nothing.
Not tested: page lists merged from two tablets (different author
UUIDs in `cPages.uuids`), deleted pages, or inserted pages with ink;
those need a second tablet (phase 2).

**Other observations.**

- A device ref already holding the snapshot's tree was reused: the
  `sync` that applied the rename pushed no new snapshot. The first
  no-op `sync` after `clone` did push one, as designed: the clone's
  snapshot (empty base) has no `.rmu/hub.json`, later snapshots carry
  it forward from base.
- When origin had not moved, `main` fast-forwarded to the snapshot
  commit itself, so the tablet's raw view state of a document the owner
  had only read (`lastOpened`, `lastModified`, a `cPages.lastOpened`
  counter) reached `main` with the new items. Harmless: it merges as
  view state.
- Opening a document and turning a page moved `lastModified` (see
  `notebook-data.md`), confirming that it must stay a derived field.
- Between two steps the tablet's battery ran low and it powered off,
  with no rmu run in progress; after the reboot it was still attached,
  unlocked and clean, and the remaining steps ran normally.

**Bugs found** (each to be fixed in its own PR):

1. **rmu hangs when the tablet sleeps.** `ssh.rs` sets neither
   `ConnectTimeout` nor `ServerAliveInterval`, so a tablet that
   suspends mid-command (or is asleep) leaves rmu waiting forever with
   no output; seen for more than five minutes on `rmu mkdir`. The
   validation passed `-o ConnectTimeout=15 -o ServerAliveInterval=10
   -o ServerAliveCountMax=3` on every command. Defaults must not
   override the user's ssh config (ssh takes the first value it sees).
   Fixed: `ConnectTimeout=15` and `ServerAliveInterval=10` are added
   where `ssh -G` reports ssh's built-in values and the user's `-o` does
   not set them (a config line setting the built-in value looks unset:
   `-o` or `--no-ssh-defaults` keeps it); a tablet that stops answering
   is `Error::Unreachable` (`rmu hub` exit status 8) and nothing retries
   it (see "A tablet that stops answering" above).
2. **`rmu mkdir` is not atomic and writes the visible file first.**
   `Client::create_folder_in` writes `.metadata`, `.content` and
   `.pagedata` in three round trips, `.metadata` first. The sleep above
   cut it after the first: a folder xochitl would show, without
   `.content`. Write all three in one guarded script, `.metadata`
   last, as apply orders its moves.
3. **Wrong firmware version in `Rmu-Firmware` and `rmu status`.**
   firmware 3.28 has no `/usr/share/remarkable/update.conf`, so
   `status::parse_status` falls back to `/etc/os-release` `VERSION_ID`,
   which is the base OS's version (`5.8.203`), not the reMarkable
   release; that one is `IMG_VERSION` (`3.28.0.172`) in the same file.
   `Rmu-Model` is the board codename (`reMarkable Ferrari`), which is
   cosmetic. Fixed: `IMG_VERSION` first, then update.conf, then
   `VERSION_ID`; `reMarkable Ferrari` is shown as `reMarkable Paper Pro`
   (reMarkable's documented code name).
4. **View state counted as a change.** After the owner only opened a
   document, `sync` reported `Documents: 1 changed on the tablet` next
   to `already in sync with origin; nothing pushed`, and `status`
   printed both `No changes on the tablet since its last sync` and `A
   sync would merge: Documents: 1 changed on the tablet`. Nothing wrong
   was written; the per-document summary counts the document as
   changed by the tablet although the merged tree equals origin's.
   Fixed: the merge still decides a document only one side changed (or
   both to the same bytes) by blob ids, without reading it; the sync's
   summary (`MergeOutcome::summary_with`) then counts such a document
   as unchanged when its merged version is equivalent to its base by
   the same test that decides pushing and applying
   (`plan::versions_equivalent`). A first-open materialisation, which
   is pushed, still counts as a change.

## Decisions (confirmed by the owner)

- The tablet that syncs second after a same-page conflict gets origin's
  version under the original name and its own as a conflict copy. Each
  fork is recorded in the merge commit's trailer (`Rmu-Fork: <original>
  <copy>`); `status` lists conflict copies that still exist under their
  generated name. Deleting or renaming the copy closes it.
- View state is per tablet.
- An edit pulls a document — or its trashed ancestor folder — out of the
  trash.
- Phase 1 is validated on the Paper Pro; other models are handled by
  per-document format gating but untested until hardware is available.
  `clone` warns when a tablet of a different model attaches to a repo
  that has only seen one model.
- The origin must be private and self-hosted.

## Follow-ups from design review

To be handled in the implementation PRs named:

- Merge engine (done in phase 1): the deletion-guard exemption applies
  only to deletions that carry recorded consent in a trailer, not to
  every deletion already in origin (one pushed by hand has none). Prove
  the cycle rule terminates when a folder restored from the trash sits
  on a cycle (send it to the root).
- Sync cycle (done): reconciliation that finishes an interrupted apply
  uses the same verify and tree checks as step 14 (it judges the files
  step 5 just read while stopped, and runs the same tree checks). `backup`
  skips reconciliation, since reconciliation can write files. The
  journal is flushed to disk (`sync`) on the tablet before each push
  (`Tablet::write_state` syncs before returning). The metadata re-read in
  step 14 includes newly created files (and moved ones of any document,
  for the tree checks). Quarantined documents are never deleted, not
  only never overwritten (they are left out of the apply plan and keep
  their previous base as an override).
- Merge engine (from the device survey; done in phase 1, see "Changed
  on both, differently"): **first-open materialisation**. A `.content` whose base has `"pages": []` and
  whose new page list is trivial — one entry per PDF page, `redir` equal
  to its position, no inserted, deleted or templated pages, no `.rm`
  files — counts as unchanged (only view state and derived fields
  moved). If both sides materialised, origin's page ids win and the
  tablet takes them on apply. Without this, two tablets opening the same
  new PDF fork it. `rmu hub add` writes a materialised page list with
  deterministic page ids (UUIDv5 over document UUID and page index) so
  documents added through the hub never hit the case (done, with the
  rule generalised to a materialised base materialised again; see
  "History, restore and add"). Phase 2 matches pages by `redir` when
  both sides materialised from an empty base.
- Sync cycle, step 2 (done): the pairing guard runs the survey's check
  (in the device layer's fail-closed form, which errors when the
  configuration cannot be read) and refuses on `true`.
- History index (deferred): keep the index next to the cache, keyed by
  its tip, and extend it with `rev-list <new> ^<old>`, instead of
  building it per run (an O(history) cost on syncs that receive a new
  document, and on `status` while one waits on origin); or query only
  the candidate documents for "came back after deletion".
- Device layer (done): skip the `tar` call when there is nothing to
  download (BusyBox `tar` fails on an empty file list); stat signatures
  use `%y`/`%z` for nanosecond times. When recording the manifest, leave out
  racy entries (`StatSignature::keep_in_manifest`, "Snapshot
  validation") using the device time of the step-6 start for step-5
  stats and the record time for post-write stats.

## Follow-ups from device validation

- **One outage per applying sync.** On a small apply, step 13 stops
  xochitl before the step-6 start has finished (see question 6), so
  the second start only adds start-limit budget and seconds of
  outage. When the staged upload is small (a size threshold), the cycle
  could keep xochitl stopped from step 3 through step 16 under the
  armed watchdog, with one start instead of two.
- The four bugs listed under "Device validation results".

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
   and crash-at-every-step tests (done: `hub::cycle`,
   `libremarkable-utils/tests/hub_e2e.rs`).
6. **CLI** (`init`, `clone`, `sync`, `backup`, `status`, `--dry-run`,
   `remote`, `detach`), output discipline, progress; README and
   AGENTS.md module map (done: `rmu/src/hub.rs`, exit statuses and
   `--json` in `rmu hub --help` and the README; `--json` stays unstable
   until device validation, item 8).
7. **Hub lock for other rmu commands**: the other mutating `rmu`
   commands (`upload`, `mv`, `rm`, `rename`, `sync`, `restart`) refuse
   while the hub lock is held, and `rmu sync` warns when its target
   tablet is attached to a hub (fresh-UUID copies would become
   duplicates in the library); see "Watchdog and lock". Touches the
   shared `Client`, so it is its own PR (done: `Client` checks before
   every write with `hub::device::probe_script`, carried by the
   listing).
8. **Device validation** on a real tablet (questions 5–7) and fixes.
   Requires the owner's go-ahead (done on one Paper Pro: "Device
   validation results"; the fixes are separate PRs).
9. **Refactor checkpoint**: shared code between `sync` and `hub`
   (listing, stat passes, tar transfer), module sizes, naming.
10. **Phase 2 page-level merge.**
11. **`log`, `restore`, `add`** (done: `hub::cycle::{library, log,
    restore, add}`, `pdf.rs`, the CLI with `--json` goldens; e2e
    scenarios restore a changed and a deleted document onto two
    tablets, add a PDF that both open without forking, check the log
    against the history, and race an `add` against a sync both ways).
