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

- [ ] Sync niceties: content hashing as an alternative
  change signal (mtime/size lies on some filesystems), exclude patterns,
  a `--pull-bundles` mode (annotated PDFs as `.rmdoc` instead of bare
  payload), and a machine-readable `--dry-run --json` plan.
- [ ] Try to remove dependence on `.rmu-sync.json` files. Try and do whatever `git` does. This may require hashing files.
- [ ] Move/rename detection for sync: content hashes in the state file
  (git-style), `MoveRemote` (metadata-only, preserves annotations) and
  `MoveLocal` actions instead of delete+reupload.
