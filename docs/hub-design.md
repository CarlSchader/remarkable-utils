# Design: `rmu hub` — git-backed multi-tablet sync

Status: **phase 1 implemented in the library** (git plumbing, data
model, merge engine, device layer, sync cycle with clone, backup, status
and `--dry-run`); the CLI and device validation are next (see
"Implementation plan"). This document is the plan of record; update it
as phases land and as device experiments confirm or refute its
assumptions.

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
                N times
12. stage       upload new and changed files into the staging directory
13. stop        arm the watchdog; systemctl stop xochitl
14. apply       re-read changed metadata; verify; tree checks against the
                live tablet; move staged files into place; device sync
15. record      update base, overrides, manifest; clear the journal
16. start       reset-failed + start xochitl; disarm the watchdog
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
  change within its second) and only if its blob is in the cache (a
  pre-read cut mid-file loses its whole `fast-import`); everything else
  is read again while stopped. Step 5 reads **before** step 4 decides:
  reconciliation judges the files just read, and a document it finishes
  is read again. `status` and `--dry-run` read the same way, with the
  second read also while xochitl runs (retried once if a file changes
  while read).
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
- **Updating `main`.** If every document of the snapshot is equivalent
  to origin's (`.metadata`/`.content` compared normalised, other files
  by blob), nothing is pushed: the merge may have taken the tablet's
  bytes for a document whose difference is view state, and pushing that
  would make two tablets push each other's view state on every sync.
  Otherwise, if the merge result is exactly the snapshot, origin has not
  moved since the snapshot's base, and no trailer is needed, the
  snapshot fast-forwards `main`. Otherwise a merge commit is pushed, even
  when its tree equals origin's: it makes the snapshot an ancestor of
  `main`, which every base override pointing at the snapshot needs.
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
  test.

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
  the journal.) Nothing is inferred automatically, because a
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
- Other mutating rmu commands (`upload`, `mv`, `rm`, `rename`, `sync`,
  `restart`) refuse while the hub lock is held, and `rmu sync` warns
  when its target tablet is attached to a hub (fresh-UUID copies would
  become duplicates in the library).

### Restartability

The journal `{ merge, snapshot }` is written before **every** push
attempt (step 10), so every window in which origin may know about a
merge the tablet has not fully applied is covered: a crash before the
push, a lost push acknowledgement, a crash before or during the apply.

Reconciliation (step 4) runs **while xochitl is stopped**, because it
may write files:

- If the journaled merge is **not** an ancestor of origin `main`, the
  push never landed. The journal is dropped; the tablet's bases are
  unchanged, and its edits are picked up by this run's snapshot.
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
    read the merge as the user's own change and fork it.

  Documents whose merge and snapshot versions agree take base = merge.
  A document is never given base = merge unless its files are at the
  merge version (or the apply demonstrably put them there and the user
  changed them since). A conflict copy is finished only together with
  its original. An override for a document the snapshot lacks stays an
  explicit override: falling back to the default base would read it as
  deleted by the tablet.
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
  recorded value is our own lost update and is accepted; anything else
  means another tablet uses this device id, and the run stops. With no
  recorded value, a remote head is accepted as our own lost first push
  (device ids are random per clone, and a clone never reuses another
  tablet's state); a recorded value with no remote ref stops the run.
  `clone --reattach` is the way out of both stops.
- **Known limits** (no ink is lost in any): a document the user edits
  during the window of a sync that forks it is skipped, its copy with
  it, and the next sync forks it again into a second copy (the first
  one holds the tablet's version as of the snapshot); the same happens
  when a forking sync's push lands and the run stops before its staging
  completes, and the user then edits the original; and a document edited
  after the hub state was lost has no known base, so `--reattach` treats
  it as new on both sides and forks it if it differs from origin's.

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
  --auto` runs at the end.
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
first-parent ancestor of `main` whose version of the document is
equivalent (normalised `.metadata`/`.content`, other files by blob) is
that document's base override; the default base is empty, so documents
only on origin are added to the tablet. Versions that exist only in
merged snapshots (second parents) are not searched. The tablet state is
written before the device-ref push, so an interrupted clone continues
with `sync`. An empty origin is one whose `main` holds only
`.rmu/hub.json` (`init` writes it): the tablet's documents are then all
"added" and pushed as a merge on `init`'s commit. When two first clones
race, the loser's push is rejected and it merges with the winner's
`main` like any sync, without inferring again (an exact match would
merge identically either way). `clone` warns when the tablet's model
differs from the one model the repository's history has seen.

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
| `libremarkable-utils/src/hub/cycle.rs` | The operations composing the parts below (`init`, `clone`, `sync`, `backup`, `status`, `--dry-run`); `Progress` steps; submodules for the local cache (`store`), reading the tablet (`snapshot`), the pure decisions (`plan`: equivalence, apply plan, verification, tree and trash checks, reconciliation verdicts, manifest), commit messages and trailers (`message`), the steps (`steps`) and the read-only commands (`inspect`). |
| `libremarkable-utils/src/hub/model.rs` | Pure: layout mapping, allowlist classification, view-state paths, document grouping, carry-forward, tablet state and journal formats, snapshot validation. |
| `libremarkable-utils/src/hub/merge.rs` | Pure: merge rules, deletion guard, fork naming and UUIDs, orphan/cycle checks. |
| `libremarkable-utils/src/hub/device.rs` | Device I/O behind a trait: lock, watchdog, stop/start, stat pass, streaming download, verify, staged apply, journal, listing what is still staged. A local-directory fake implements the trait, with fault injection. |
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
  that kills `receive-pack` once the update is committed.
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

Needing writes (answered during device validation, with go-ahead):

5. Does xochitl regenerate missing thumbnails and `.local` files?
6. How long is the UI down for snapshot and apply on a real library? Is
   stopping xochitl safe on the Paper Pro with encrypted storage, and
   within the start-limit window when repeated?
7. Does xochitl accept a `.content` whose page list was merged
   externally?

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
  new PDF fork it. `rmu hub add` should write a materialised page list
  with deterministic page ids (UUIDv5 over document UUID and page index)
  so documents added through the hub never hit the case. Phase 2 matches
  pages by `redir` when both sides materialised from an empty base.
- Sync cycle, step 2 (done): the pairing guard runs the survey's check
  (in the device layer's fail-closed form, which errors when the
  configuration cannot be read) and refuses on `true`.
- Device layer (done): skip the `tar` call when there is nothing to
  download (BusyBox `tar` fails on an empty file list); stat signatures
  use `%y`/`%z` for nanosecond times. When recording the manifest, leave out
  racy entries (`StatSignature::keep_in_manifest`, "Snapshot
  validation") using the device time of the step-6 start for step-5
  stats and the record time for post-write stats.

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
   AGENTS.md module map.
7. **Device validation** on a real tablet (questions 5–7) and fixes.
   Requires the owner's go-ahead.
8. **Refactor checkpoint**: shared code between `sync` and `hub`
   (listing, stat passes, tar transfer), module sizes, naming.
9. **Phase 2 page-level merge.**
10. **`log`, `restore`, `add`.**
