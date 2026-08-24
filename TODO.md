# TODO

Planned work, roughly in priority order.

## Sync

- [ ] **`rmu sync` — folder sync over SSH.** Full design agreed and
  written up in `docs/sync-design.md`. Phase 1: one-way
  `rmu sync <SRC> <DST>` with scp-style endpoints
  (`[user@]host:path`, ssh-config resolution, runtime tablet
  detection), sync-state file, `--dry-run`. Phase 2: `--two-way`,
  `--conflict`, `--delete`. Phase 3: generic-host endpoints and
  tablet↔tablet.

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
