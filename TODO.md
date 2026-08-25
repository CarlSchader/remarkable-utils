# TODO

Planned work, roughly in priority order.

## Sync (`rmu sync` — design: `docs/sync-design.md`)

### Done (phase 1)

- [x] One-way `rmu sync <SRC> <DST>`, direction from argument order.
- [x] scp-style endpoints (`[user@]host:path`), ssh-config resolution,
  optional `--port` everywhere (ssh config `Port` now works).
- [x] Runtime tablet detection (`probe_remarkable`), `--remote-kind`
  override.
- [x] Sync-state file (`.rmu-sync.json`): path↔UUID mapping, three-way
  change detection, incremental saves (interrupted syncs resume).
- [x] Pure planner + thin executor; 20 unit tests covering
  creates/updates/conflicts/loops/duplicate names/first-sync/recopy.
- [x] `--dry-run` (plan to stdout), skip-on-destination-drift,
  update-in-place for mapped payloads (preserves annotations),
  mapped-`.rmdoc`-pull-only / new-`.rmdoc`-restore rules,
  md/txt→EPUB push with pull loop prevention.

### Testing (all verified on a real device, 2026-08)

- [x] **Device smoke test of sync phase 1**, in order:
  1. `rmu sync --dry-run ./dir remarkable:/SyncTest` (plan looks sane),
  2. push a small tree (pdf + md + nested folder), verify with `rmu ls`,
  3. re-run push — must be a no-op,
  4. edit a local pdf, push — must update in place and **preserve
     annotations** made on the device copy,
  5. annotate a synced pdf on the device, edit it locally too, push —
     must skip with a drift warning,
  6. pull a folder containing notebooks into an empty dir; re-pull —
     must be a no-op,
  7. draw on a synced notebook, pull — `.rmdoc` must refresh,
  8. interrupt a multi-file sync (Ctrl-C) and re-run — must resume
     without duplicating documents.
- [x] Verify `remarkable:/Books` endpoints resolve through a real
  `~/.ssh/config` `Host` entry (including a non-22 `Port`).
- [x] Confirm probe behavior against a non-tablet ssh host (should fail
  with the "not a reMarkable" message, not hang or misclassify).
- [x] Confirm `rmu` still works against the device after the
  `SshOptions` destination/port refactor (regression check on the
  pre-existing commands: `ls`, `upload`, `download`).

### Done (phase 2)

- [x] `--two-way`: bidirectional sync; argument order only matters for
  `--conflict src|dst` mapping.
- [x] `--conflict skip|newest|src|dst`: applies to both-changed
  conflicts, destination drift, deletion-vs-change races, and unmapped
  same-path collisions (which *adopt* the pairing into state when
  resolved; handwriting is never overwritten).
- [x] `--delete`: propagates deletions of **mapped** files only
  (never-synced files are never deleted); deletion-vs-change is a
  conflict; stale mappings are forgotten.
- [x] Planner rewritten as a unified per-key three-way decision table;
  11 new unit tests (59 total).

### Testing needed (phase 2, requires a real device)

- [ ] Two-way smoke test: seed both sides, `--two-way --dry-run`, then
  run; verify uploads and downloads in one pass and an idempotent
  re-run.
- [ ] Conflict policies: change a synced pdf on both sides; verify
  `--conflict skip` reports, `newest` picks the right side, `src`/`dst`
  respect argument order.
- [ ] `--delete`: delete locally, push with `--delete` (device copy
  removed); delete a *changed* device doc's local copy and verify the
  conflict is reported, not silently resolved.
- [ ] Unmapped-collision adoption: same pdf on both sides with no
  state, `--conflict newest`; verify it maps rather than duplicates.
- [ ] Interrupted two-way sync resumes cleanly (state saved per action).

### Next dev steps

- [ ] **Phase 3 — more endpoint pairings**: introduce the endpoint
  trait (snapshot/read/write/mkdir/delete) once `RemoteFs` gives it a
  second implementation; generic ssh-host sync (`--remote-kind fs`
  currently errors) and tablet↔tablet via bundle streaming (state file
  keyed by endpoint pair on the initiating computer).
- [ ] Sync niceties, after the phases: content hashing as an alternative
  change signal (mtime/size lies on some filesystems), exclude patterns,
  a `--pull-bundles` mode (annotated PDFs as `.rmdoc` instead of bare
  payload), and a machine-readable `--dry-run --json` plan.

## Text import (`rmu upload` for `.md`/`.txt`)

- [ ] **Embed local images in Markdown imports.** `![alt](path.png)`
  currently becomes a dead link inside the generated EPUB (only alt text
  renders — see `docs/text-import.md`). Teach `epub.rs` to collect local
  image references relative to the source file, add them to the EPUB
  manifest (`OEBPS/images/...`), and rewrite `src` attributes. PNG/JPEG
  only; stay pure Rust. Remote URLs stay untouched (the device is
  offline while reading). Verify rendering on a real device before
  claiming support.

- [ ] **Mermaid diagram rendering.** Fenced ```mermaid blocks currently
  render as literal source in a code block — mermaid is a JS library and
  the device's EPUB reader has no JS runtime, so diagrams must be
  rendered before they enter the EPUB. Approach: opt-in flag (e.g.
  `--render-mermaid`) that shells out to `mmdc` (mermaid-cli) when
  installed, producing PNGs that flow through the image-embedding
  pipeline above (hard dependency on Node/Chromium is unacceptable;
  a pure-Rust mermaid renderer does not exist). Depends on image
  embedding. Document the pre-render-yourself workaround in
  `docs/text-import.md` in the meantime.
