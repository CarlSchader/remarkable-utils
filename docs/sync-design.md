# Design: `rmu sync` (v2 — content-addressed)

Status: **v1 phases 1–3 implemented and device-verified** (one-way,
two-way, `--delete`, conflict policies, fs↔fs, tablet↔tablet). **v2
phases 1–3 implemented, device verification pending**: content
hashing, the XDG archive, refresh/adoption, file- and folder-level
move detection, copy-by-fingerprint, incremental device listing.
Update this file as phases land.

The v2 redesign borrows deliberately from the
[Unison file synchronizer](https://github.com/bcpierce00/unison)
(archives, fastcheck, fingerprint-verified pairing) while keeping
rmu's defining constraint: **no remote agent** — the tablet side is
busybox ssh and batched shell scripts only.

## Goal

Sync a local directory with a folder (or the root) of the reMarkable's
logical filesystem, in either direction — or between any two endpoints.
Only supported file types participate (`.pdf`, `.epub`, `.rmdoc`);
everything else is left untouched. Implemented as an `rmu` subcommand so
it reuses the SSH session, auth flags, transfer machinery, and progress
reporting.

## CLI

```sh
rmu sync <SRC> <DST>                  # one-way: copy changes from SRC to DST
rmu sync ./books remarkable:/books    # PC -> tablet ("push")
rmu sync remarkable:/books ./books    # tablet -> PC ("pull")
rmu sync --two-way ./books remarkable:/books   # bidirectional
```

| Flag | Meaning |
|---|---|
| `--dry-run` | Print the action plan (stdout), change nothing |
| `--delete` | Propagate deletions (opt-in, rsync-style) |
| `--two-way` | Bidirectional; argument order stops mattering |
| `--conflict skip\|newest\|src\|dst` | Conflict policy; default `skip` |
| `--remote-kind remarkable\|fs` | Override endpoint auto-detection |

Direction is determined by **argument order**, exactly like `scp`/
`rsync`. Endpoints use scp syntax (`[user@]host:path`, remote iff a `:`
appears before the first `/`); the host string goes to the system `ssh`
binary verbatim, so ssh config resolution comes for free. Remote
endpoints are classified at runtime with one probe (`test -d
<xochitl-dir> && test -e /usr/bin/xochitl`).

## Endpoint model

| Kind | Listing | Identity / change signal |
|---|---|---|
| `LocalDir` | filesystem walk | content hash; stat stamp gates re-hashing |
| `Remarkable` (ssh) | `Client::list_items` logical tree | item UUID + `lastModified` + lazy payload hash |
| `RemoteFs` (ssh, generic host) | remote `find`-based walk | rel path + mtime/size (hashes on demand via `sha256sum`) |

Supported pairings: local↔tablet (the core feature), tablet↔tablet
(`.rmdoc` bundle streaming, full fidelity), and fs↔fs (feature-poor
rsync, exists for consistency).

## Identity: content-addressed, not path-keyed

The v1 design keyed everything by relative path in a state file inside
the sync root. That made moves look like delete+create, made state loss
catastrophic, and put the state file in `git clean`'s blast radius. v2
treats sync as a three-way diff over **three trees** — the archive
(last agreed state), side A now, side B now — with content identity as
the primary signal:

- **fs entries**: SHA-256 of the content. The stat stamp (mtime+size)
  gates re-hashing, Unison-fastcheck style: a file whose stamp matches
  the archive is never re-read. Hashes are recorded into the archive on
  every transfer, so steady-state syncs hash nothing.
- **device entries**: UUID (a stronger identity than paths, and the
  device provides it for free) + `lastModified` + payload hash. Payload
  hashes are computed **lazily and batched**: one `sha256sum` round trip
  per sync, covering only the documents where a decision actually needs
  one (see below). The tablet's CPU is slow; eager whole-library hashing
  would take minutes, lazy hashing takes seconds.
- **paths are properties, not identities** — the groundwork for move
  detection (phase 2).

Payload hashes are only meaningful for **payload-mirrored** kinds
(pdf/epub, where the device payload is byte-identical to the local
file). Notebooks/bundles are never hashed; their rules are unchanged.

### What the lazy device hashing covers (`device_hash_candidates`)

1. **Mapped docs whose `lastModified` moved** while the archive has a
   payload hash: annotating a PDF bumps `lastModified` without touching
   the payload. If the fresh hash matches the recorded one, the plan
   emits `refresh` (state-only) instead of a re-download. This fixes
   v1's documented "correct, occasionally wasteful" behavior.
2. **Unmapped path collisions** (same pull path on both sides, no
   archive entry — typically after archive loss): if the local hash and
   the payload hash agree and the kinds match, the plan emits `adopt`
   (state-only) — the pairing is re-established silently, under any
   conflict policy, in any mode. **Archive loss is benign**: only
   genuinely divergent files surface as conflicts.

Symmetrically, a touched-but-identical local file (`touch`, re-save)
produces `refresh`, not a re-upload.

## The archive

A versioned JSON file per endpoint pair recording, per synced entry:

```
relative path <-> device UUID,
last-synced local mtime+size and SHA-256,
last-synced device lastModified and payload SHA-256
```

Placement: `$XDG_STATE_HOME/rmu/sync-<pairhash>.json` (or
`~/.local/state/rmu/`), keyed **order-independently** by the two
endpoint identities (canonicalized local path; `destination:path` for
remote endpoints), so push and pull over the same pair share one
archive. Never inside the synced tree: no git pollution, no accidental
deletion, overlapping roots each get their own archive. tablet↔tablet
pair-state files live in the same directory (`sync-pair-<hash>.json`).

Written **incrementally after each action** (temp file + atomic rename)
so an interrupted sync resumes cleanly. A legacy in-root
`.rmu-sync.json` is ignored (a note suggests deleting it); hash
adoption re-pairs its contents on the first v2 run.

Consequences accepted: moving a synced directory to a new path starts a
fresh archive (adoption re-pairs it on first contact); two machines
syncing the same tree keep independent archives (as Unison does).

## Update detection and planning

Pure planner, thin executor (repo convention). Inputs: local snapshot
(with hashes attached), device snapshot (with lazy payload hashes
attached), archive, mode/policy. Output: an ordered `Vec<SyncAction>`.

Per key, the three-way presence table (archive × local × remote)
decides; when stamps moved, content verdicts override stamp verdicts:

- both stamps clean → nothing
- stamp moved, content identical (hash known) → `refresh` (state-only)
- content changed on one side → transfer toward the other (mode
  permitting); update-in-place preserves annotations and tree location
- content changed on both → `--conflict` policy; default `skip` loses
  nothing
- unmapped collision, hash-identical → `adopt`; hash-different →
  policy (adoption toward the device only for matching payload types;
  handwriting is never overwritten)
- a vanished mapped path and a new unmapped file with the same hash
  and kind → `move` (see below)
- a new file whose content already exists on the destination → `copy`
  on the destination (nothing transferred)
- mapped, one side gone → recopy or (with `--delete`) delete; a
  deletion racing a change is a conflict
- both gone → `forget`

Ordering: state-only actions (rebind/adopt/refresh) → folder creates →
transfers → deletions (docs, then folders emptied *by this plan*,
children first; pre-existing empty folders are never touched) →
forgets → notes.

Unchanged v1 rules that remain data-safety invariants:

- Mapped `.rmdoc`s are pull-only (ink cannot be merged; the tablet is
  the only writer of notebook content). A brand-new local `.rmdoc` is a
  deliberate restore (fresh UUID).
- Trash, orphans, duplicate sibling names, and unusable filenames are
  excluded with warnings — never guessed.
- `--delete` only propagates deletions of **mapped** files.
- Uploads write `.metadata` last, so interruption leaves invisible
  orphans, not half-documents; dangling mappings **rebind** to
  same-name same-kind successors.

## Moves and copies (phase 2)

Move detection runs as a **pre-pass** before the three-way table: it
pairs moves by content identity and re-keys the working copies of the
archive and local snapshot, so the table sees moves as already-agreed
facts instead of delete-and-create pairs.

**Folder-level (`MoveRemoteFolder`)**: when *every* mapped file under
a device folder vanished locally and reappeared under one new prefix
at the same relative sub-paths with the same hashes and kinds, the
whole folder is relocated with **one** metadata write — the subtree
follows, the folder keeps its UUID (and any device-side folder
metadata). Nested moves pair the outermost folder only; partial moves,
taken target names, and ambiguous targets fall back to file-level
handling with a note. Device→local folder moves need no special
pairing: each contained doc is UUID-identified and resolves to a cheap
per-file local rename.

**Local→device (`MoveRemote`)**: a vanished mapped path and a new
unmapped file with the same hash and kind, strict 1:1 — filename
tie-break first, then a single unambiguous pair; anything else gets a
skip-note and falls back to the ordinary rules (where
copy-by-fingerprint usually still avoids the upload). Requires the
device copy unchanged since last sync: a racing device edit would be
masked by the move's `lastModified` bump, so it falls back instead.
The move itself is **one metadata write** — zero bytes transferred,
annotations preserved. Works for `--delete` and non-`--delete` alike
(the old path is consumed, so no delete/re-upload is planned).

**Device→local (`MoveLocal`)**: the document UUID *is* the identity,
so a mapped doc whose device path moved simply drags the local file
along (`rename`). A device move bumps `lastModified`; the bump is
absorbed only when the lazily fetched payload hash proves the content
unchanged — otherwise the recorded `lastModified` stays put and the
table schedules the content transfer at the new path (safe under
interruption: a resumed run still sees the pending change). An
occupied target path produces a note, never an overwrite.

**Copy-by-fingerprint** (Unison's trick, adapted):

- push: a new local file whose hash matches a mapped, unchanged device
  payload becomes an **on-device `cp`** + registration — nothing
  uploaded. The copy runs before `.metadata` is written and before any
  deletions execute, so interruption leaves an invisible orphan and
  ambiguous-move fallbacks can copy from a document that is about to
  be deleted.
- pull: a new device document whose (lazily fetched) payload hash
  matches any local file becomes a **local copy** — nothing
  downloaded. Device docs are only hashed when their size matches some
  local file (equal hashes require equal sizes, so the size check is a
  free prefilter).

Action order: state-only (rebind/adopt/refresh) → folder creates →
moves (folders first) → transfers and copies → deletions → forgets →
notes.

## Incremental device listing (phase 3)

`Client::list_items` with a listing cache
(`$XDG_STATE_HOME/rmu/listing-<hash>.json`, keyed by ssh destination +
xochitl dir) becomes change-scaled, Unison-archive style:

1. Round trip 1: `stat -c '%Y %s %n'` over every
   `.metadata`/`.content`/payload file — tiny output, one exec.
2. Round trip 2 (only when needed): `cat` exactly the files whose
   mtime+size moved since the cached copy; everything else is reused
   from the cache. Payload sizes come from the stat pass for free
   (the full fetch needed a `wc -c` loop).

A no-op listing of a large library stops re-reading thousands of
JSONs. The cache is best-effort: any load problem falls back to the
full fetch, any save problem only costs speed next run. Staleness
caveat (same trade as the git index / Unison fastcheck): a rewrite
within the same second that keeps the file size is invisible until
the next change. `rmu sync` and the regular commands both use the
cache; every run re-stats, so rmu's own writes are always seen.

## Executor

Applies actions in order, emits `Progress` steps, saves the archive
after every action, restarts xochitl **once** at the end (via
`reset-failed` first — see `AGENTS.md`). Transfers record fresh hashes
into the archive: for pdf/epub the uploaded/downloaded bytes are the
payload, so one hash serves both sides.

## Roadmap

- **v2 phase 1 (implemented):** sha2; XDG archive with atomic writes;
  stamp-gated local hashing; lazy batched device payload hashing;
  `refresh` and `adopt` actions; legacy state-file warning.
- **v2 phase 2 (implemented):** file-level move detection in both
  directions (`MoveRemote` = one metadata write; `MoveLocal` = local
  rename, UUID identity) and copy-by-fingerprint in both directions
  (`CopyRemote` = on-device `cp`; `CopyLocal` = local copy), with the
  size prefilter extending lazy device hashing. See "Moves and copies"
  above.
- **v2 phase 3 (implemented):** folder-level move pairing
  (`MoveRemoteFolder`, one metadata write per moved folder) and the
  incremental device listing (above).
- **v2 phase 4 (candidates):** bounded transfer pipelining over the
  multiplexed connection (measure first); watch mode (local notify +
  cheap device polling); `--paranoid` full re-hash; exclude patterns;
  `--dry-run --json`.
- **Refactor candidate, deliberately deferred:** collapsing the three
  planners into one `Entry`/`Snapshot` model with a single three-way
  table. The md/txt-conversion asymmetry that motivated it is gone and
  the remaining duplication is modest; a big rewrite of
  device-verified planning logic should happen on its own, not
  bundled with feature work. Revisit when the duplication actually
  blocks a feature.
- **Explicitly out of scope:** rsync-style block-delta transfer — it
  requires simultaneous computation on both ends, i.e. a remote agent,
  which rmu deliberately does not have. Payloads are replaced wholesale
  in practice, so the value would be low anyway.

Testing: the planner carries the correctness burden in unit tests; the
executor is verified against a real device, `--dry-run` first.
