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
