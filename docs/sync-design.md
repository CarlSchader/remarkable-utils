# Design: `rmu sync`

Status: **phase 1 implemented** (`libremarkable-utils/src/sync.rs`,
`rmu sync`): one-way sync with scp-style endpoints, tablet detection,
sync-state file, and `--dry-run`. Phases 2–3 (two-way, `--conflict`,
`--delete`, generic hosts, tablet↔tablet) remain planned. Update this
file if the design changes during implementation.

## Goal

Sync a local directory with a folder (or the root) of the reMarkable's
logical filesystem, in either direction — or eventually between any two
endpoints. Only supported file types participate; everything else is left
untouched. Implemented as an `rmu` subcommand (not a separate binary) so it
reuses the SSH session, auth flags, transfer machinery, conversions, and
progress reporting.

## CLI

```sh
rmu sync <SRC> <DST>                  # one-way: copy changes from SRC to DST
rmu sync ./books remarkable:/books    # PC -> tablet ("push")
rmu sync remarkable:/books ./books    # tablet -> PC ("pull")
rmu sync --two-way ./books remarkable:/books   # phase 2: bidirectional
```

Flags:

| Flag | Meaning |
|---|---|
| `--dry-run` | Print the action plan (stdout), change nothing |
| `--delete` | Propagate deletions (opt-in, rsync-style) |
| `--two-way` | Bidirectional; argument order stops mattering (phase 2) |
| `--conflict skip\|newest\|src\|dst` | Conflict policy; default `skip` (phase 2) |
| `--remote-kind remarkable\|fs` | Override endpoint auto-detection (escape hatch) |

Implementation notes (phase 1):

- A push whose device folder does not exist creates it (`mkdir -p`
  semantics); a pull creates the local root directory.
- Only folders that will receive synced content are created — a local
  directory containing nothing but unsupported files does not produce
  an empty device folder.
- A mapped file deleted on the *destination* is recopied
  (rsync-without-`--delete` semantics), on both push and pull.
- The regular `--host`/`--user` flags are ignored by sync; `--port`
  (now optional everywhere) is only passed to ssh when given, so ssh
  config `Port` works.

Direction is determined by **argument order**, exactly like `scp`/`rsync`:
the first argument is the source, the second the destination. There are no
`--push`/`--pull` flags.

### Endpoint syntax (scp conventions)

Remote endpoints use scp syntax: `[user@]host:path`.

- An argument is remote iff it contains a `:` **before the first `/`**
  (`./weird:name` is local; prefix a colon-containing local path with `./`).
- `user@host:path` splits at the first `:`. An empty path after `:` means
  the endpoint root.
- The host string is handed to the system `ssh` binary verbatim, so
  **ssh config resolution comes for free**: `remarkable:/books` uses
  whatever `Host remarkable` resolves to (`HostName`, `User`, `Port`,
  `IdentityFile`, `ProxyJump`, ...). No `user@` means ssh config (or ssh's
  defaults) picks the user — sync does not force `root@`.
- The global `--host`/`--user` flags do **not** apply to sync (endpoints
  are self-contained); the connection flags (`-i`, `-o`, `--password*`,
  `--no-multiplex`) apply to every ssh endpoint.

## Endpoint model

Sync is defined over **two endpoints of three possible kinds**, not
hardcoded "local vs. tablet":

| Kind | Listing | Identity / change signal | Conversions |
|---|---|---|---|
| `LocalDir` | filesystem walk | rel path + mtime/size | — |
| `Remarkable` (ssh) | `Client::list_items` logical tree | item UUID + `lastModified` | md/txt→EPUB toward device; notebook→`.rmdoc` from device |
| `RemoteFs` (ssh, generic host) | remote `find`-based walk | rel path + mtime/size | — |

Conversion rules activate only when **exactly one side is a
`Remarkable`**. Supported pairings:

- **local ↔ tablet** — the core feature (phase 1).
- **tablet ↔ tablet** — copy documents between devices: stream the
  `.rmdoc` tar from device A, re-target to a fresh UUID, extract on
  device B. The existing bundle machinery is already this pipeline
  (phase 3).
- **local ↔ generic host / pc ↔ pc** — plain file-tree sync via
  `RemoteFs`. Honest caveat: for pure file trees this is a feature-poor
  rsync (no delta transfer); it exists for consistency and the docs
  should say so, not pretend to compete (phase 3).

### Runtime tablet detection

Each remote endpoint is classified with one probe command after
connecting (connection multiplexing makes this near-free — the probe
shares the master connection with the transfers that follow):

```sh
test -d <xochitl-dir> && test -e /usr/bin/xochitl
```

The xochitl data dir (honoring `--xochitl-dir`) + binary is the signal
(present on rM1, rM2, and Paper Pro — same software stack). Probe exit 1
means "reachable but not a tablet"; ssh exit 255 is a connection error
and fails the sync. `--remote-kind` overrides detection. Corroborating
signals (os-release / device-tree strings) were considered and dropped
as unnecessary; revisit only if the single probe misfires in practice.

## The core problem: identity, not transfer

Transfer is solved by existing code. The hard part is knowing that local
`notes.md` *is* device document `abc-123`, because the conversions are
asymmetric:

- `notes.md` uploads as an **EPUB** named "notes"; a naive pull would
  bring back `notes.epub` next to `notes.md`, and the next push would
  duplicate it — a loop.
- Notebooks pull as `Name.rmdoc`, which uploads back under a *fresh UUID*
  by design — another loop.
- The device allows duplicate sibling names; filesystems do not.

### Sync state file

A versioned JSON state file (`.rmu-sync.json`) records, per synced entry:

```
relative path <-> device UUID,
last-synced local mtime+size,
last-synced remote lastModified
```

This enables true **three-way diffing** (last-synced state vs. src now
vs. dst now), which is what distinguishes "unchanged", "changed on one
side", "changed on both" (conflict), and — with `--delete` — "deleted
since last sync" vs. "never existed". It also breaks both loops above:
a state-mapped `notes.md` knows its device EPUB, and a device-side EPUB
payload never changes on its own, so pull is a no-op for it.

Placement (phase 1): in the local sync root. When tablet↔tablet lands
(phase 3), state moves to the initiating computer, keyed by the
endpoint pair. Users should gitignore it. Written **incrementally after
each action** so an interrupted sync resumes cleanly.

## What syncs (local ↔ tablet)

| Local file | Toward tablet | From tablet |
|---|---|---|
| `.pdf` / `.epub` | upload; **update-in-place** if mapped (overwrite `<uuid>.<ext>`, bump `lastModified` — preserves annotations and location) | download payload when remote changed |
| `.md` / `.txt` | convert → EPUB; update-in-place replaces the generated payload | never pulled *as* md/txt; the state mapping makes pull a no-op |
| `.rmdoc` | **new/unmapped file only** — treated as a restore (fresh UUID), then mapped. Mapped `.rmdoc`s are never pushed back (see below) | notebooks pull as `Name.rmdoc` when `lastModified` moved |
| anything else | ignored, left in place | n/a |

Device folders map to directories, created as needed in both directions.
Excluded from sync: trash and orphan items, duplicate sibling names, and
names that don't sanitize to a valid filename — skip with a warning,
consistent with the repo-wide "never guess on ambiguity" invariant.

### Why mapped `.rmdoc`s are pull-only

1. An `.rmdoc` is an opaque backup (zipped `.rm` stroke data). Nothing on
   a computer edits it, so a *mapped* local `.rmdoc` differing from the
   device means it is stale (fix: pull) or corrupted (pushing it back
   would be exactly wrong).
2. Stroke data cannot be merged. If the tablet was drawn on since the
   last pull, versions have diverged and someone's ink gets destroyed.
   The tablet is the only writer of notebook content, so the tablet wins.
3. `upload_rmdoc` re-targets to a fresh UUID by design (restores must not
   collide with a live original). Pushing a mapped `.rmdoc` back would
   therefore *duplicate*, not update — and the next pull would hit a
   duplicate-sibling ambiguity.

A brand-new local `.rmdoc` with no mapping is a deliberate restore
(user copied a backup in) and is pushed as one.

## Change detection and conflicts

- Local/RemoteFs changed: mtime+size differs from state. (Content hashing
  deliberately omitted from the MVP; can be added behind the same planner
  interface later.)
- Remarkable changed: `lastModified` differs from state. Caveat:
  annotating a PDF bumps `lastModified` without changing the payload, so
  a pull may re-download an identical payload — correct, occasionally
  wasteful.
- One-way mode: "conflict" means the *destination* changed since last
  sync; default is skip + warn rather than silently overwrite.
- Two-way mode (phase 2): both sides changed → `--conflict` policy;
  default `skip` reports and loses nothing.
- First sync (no state): union merge — copy what exists only on one
  side; same name on both sides with no state to arbitrate = conflict.

## Architecture

Following repo conventions (pure logic separated from I/O):

1. **`libremarkable-utils/src/sync.rs` — pure planner.** Inputs: the
   local snapshot (`LocalEntry` list), the device snapshot
   (`RemoteSnapshot`), previous state, and the direction. Output:
   ordered `Vec<SyncAction>`: `CreateRemoteFolder`, `Upload`,
   `UpdateRemote`, `Download`, `Skip { path, reason }`. Conflicts are
   expressed as `Skip` with an explanatory reason in phase 1; a
   dedicated `Conflict` variant with a resolution arrives with the
   phase-2 `--conflict` policies. `Delete*` actions arrive with
   `--delete`. Zero I/O — exhaustively unit-tested with fabricated
   snapshots (creates/updates/conflicts/loop-prevention/duplicate
   names/first-sync/recopy).
2. **Executor.** Applies actions in dependency order (folders before
   contents; deletions last, once they exist), emits `Progress` steps
   (`"[3/17] upload notes.md"`), writes state incrementally, and
   restarts xochitl **once** at the end, not per file. Phase 1 is
   direction-aware over the two concrete endpoint kinds; the
   generalized **endpoint trait** (snapshot/read/write/mkdir/delete)
   is deliberately deferred to phase 3, when `RemoteFs` gives it a
   second real implementation — abstracting over one implementation
   earns nothing.
3. **CLI.** `--dry-run` renders the plan to stdout (that *is* the
   output); summary and progress to stderr, honoring `--quiet`, per the
   repo's output discipline.

New `Client` primitive required: `update_payload(uuid, source)` —
overwrite an existing document's payload file and bump `lastModified`
(preserves annotations and tree location). Small; composes existing ssh
methods.

## Phasing

- **Phase 1 (MVP):** `rmu sync <SRC> <DST>`, scp endpoint syntax,
  endpoint detection, `LocalDir` + `Remarkable` endpoints, one-way with
  state file, `--dry-run`, skip+warn on destination drift, `.rmdoc`
  rules as above.
- **Phase 2:** `--two-way`, `--conflict` policies, `--delete`.
- **Phase 3:** `RemoteFs` endpoint (pc↔pc, honest-caveat mode),
  tablet↔tablet via bundle streaming. Also candidates: pull annotated
  PDFs as `.rmdoc`, content hashing, watch mode.

Testing: the planner carries the correctness burden in unit tests; the
executor is verified against a real device, `--dry-run` first.
