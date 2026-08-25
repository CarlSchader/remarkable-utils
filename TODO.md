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
