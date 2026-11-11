# TODO

### Testing needed (phase 3, requires devices / a second host)

- [ ] Generic ssh host ↔ tablet: `rmu sync user@server:/docs remarkable:/X`
  (host auto-classified as `fs` by the probe; also verify the
  `--remote-kind` override).
- [ ] local ↔ ssh-host file sync against a real Linux and a macOS
  remote (exercises both `stat` variants and the snapshot script).
- [ ] tablet↔tablet: first sync copies both ways (`--two-way`), notebook
  arrives with ink intact, re-run is a no-op; draw on one side and
  verify replace-propagation; `--delete` and conflict policies;
  pair-state found with swapped argument order.
- [ ] Memory note: ssh-endpoint transfers buffer documents in RAM —
  sanity-check with a large (100MB+) PDF.

### Next dev steps

- [x] Sync v2 phase 1: content hashing (stat-gated local, lazy batched
  device payloads), XDG archive keyed by endpoint pair, refresh/adopt
  actions (archive loss is now benign). Device verification pending.
- [x] Sync v2 phase 2: file-level move detection in both directions
  (metadata-only device moves, local renames) and copy-by-fingerprint
  in both directions. Device verification pending.
- [x] Sync v2 phase 3: folder-level move pairing (one metadata write
  per moved folder) and the incremental device listing (stat first,
  cat only changes). Device verification pending.
- [ ] Refactor candidate (deferred): unified planner
  (`Entry`/`Snapshot`, one three-way table, folders as entries) —
  standalone refactor, not bundled with feature work.
- [ ] Sync v2 phase 4 candidates: bounded transfer pipelining, watch
  mode, `--paranoid` re-hash, exclude patterns, `--dry-run --json`,
  `--pull-bundles` (annotated PDFs as `.rmdoc`).

### rmu hub (git-backed multi-tablet sync)

- [x] Phase 1 of `docs/hub-design.md`: `init`, `clone`, `sync`,
  `backup`, `status`, `remote`, `detach`, `log`, `restore`, `add`,
  the hub lock for other commands. Validated on one Paper Pro
  ("Device validation results" in the design doc).

### rmu hub: revisit after a few weeks of real use

Decide what to do next from what daily use shows; none of this is
urgent.

- [ ] **Second tablet.** Validate with a second real device, ideally a
  different model (reMarkable 2) on different firmware: clone it into
  the existing hub, make conflicting edits on both, sync both ways.
  Answers what one tablet could not: whether the `cPages.uuids` author
  UUID is per tablet or per account, how xochitl treats page lists
  merged from two tablets, whether the `customZoom*` fields are model
  specific, and whether the mixed-model warning is enough.
- [ ] **Phase 2 page-level merge** (implementation plan item 10), only
  if conflict copies turn out to be common; its design depends on the
  second-tablet results.
- [ ] **Freeze `--json`** (`schema: 1`) if scripts depend on it: goldens
  for every note, warning and error variant; settle names first (the
  guard is `guard` in `status` but `deletions` in `sync`; `ours`/
  `theirs` merge jargon; `summary.forked` counts both copies).
- [ ] **Friction from real use**: messages and sync summaries that
  confused; UI downtime; the single-stop thresholds (a first guess from
  one tablet and one link); the tablet sleeping during long syncs
  (rmu does not keep it awake: a keep-awake mechanism?); first clone
  over USB vs Wi-Fi.
- [ ] **Persistent, incremental history index** if `log`, `status` or
  `clone --reattach` get slow as history grows (today it is rebuilt
  from a full `rev-list` + tree diffs each time it is needed).
- [ ] **Release**: version bump, changelog, README polish, a `nix run`
  example.
- [ ] **stat/sha256 script consolidation**: move the old commands onto
  the hub's device-validated scripts; `rmu sync`'s generic SSH hosts
  still need a non-GNU (`stat -f`) fallback.
- [ ] **Test gaps** left by reviews: the racy-entry rule with coarse
  mtimes (the fake tablet only produces nanosecond mtimes); occasional
  scratch-dir cleanup races from background git in e2e tests; the
  single-stop size estimate counting quarantined documents and reading
  more trees than it needs.
