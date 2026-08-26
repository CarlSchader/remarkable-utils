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
- [ ] Sync v2 phase 2: unified planner (`Entry`/`Snapshot`, one
  three-way table, folders as entries), file-level move detection
  (metadata-only device moves), copy-by-fingerprint.
- [ ] Sync v2 phase 3: folder-level move pairing, incremental device
  listing (stat first, cat only changes).
- [ ] Sync v2 phase 4 candidates: bounded transfer pipelining, watch
  mode, `--paranoid` re-hash, exclude patterns, `--dry-run --json`,
  `--pull-bundles` (annotated PDFs as `.rmdoc`).
