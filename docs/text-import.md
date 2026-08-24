# Text and Markdown on the reMarkable

The tablet does **not** support `.txt` or `.md` files. xochitl renders
exactly three document types — native notebooks, `pdf`, and `epub` (see
`docs/notebook-data.md`) — and the official importers accept only PDF and
EPUB. A text file pushed to the data dir with made-up metadata will simply
fail to open.

`rmu` makes text files work anyway by converting them to EPUB **on the
host at upload time**: `rmu upload notes.md` produces an EPUB in memory
and uploads it through the normal payload path. On the device the document
is a regular EPUB (`fileType: "epub"`); the tablet never sees the original
source file.

## Why EPUB (and not PDF)

- EPUB reflows: the device's font-size/margin/justification controls work,
  which is what you want for prose notes. A PDF would bake in one layout.
- EPUB is a zip archive of XHTML — we already ship the `zip` crate for
  `.rmdoc` bundles, and Markdown→HTML is handled by `pulldown-cmark`.
  Everything stays pure Rust; no pandoc or other external converter.

## Conversion nuances

### Markdown (`.md`, `.markdown`)

- Dialect: CommonMark plus the extensions people actually use in notes:
  tables, strikethrough, task lists, and footnotes.
- **Raw HTML is escaped, not passed through.** xochitl's EPUB reader wants
  well-formed XHTML; arbitrary inline HTML (unclosed tags, script, etc.)
  could produce a document the device refuses to render. Escaping shows
  the HTML as literal text — lossy but safe. Revisit only with a
  sanitizer that guarantees well-formedness.
- Images are referenced, not bundled: `![alt](path.png)` becomes a dead
  link inside the EPUB since we don't package local images (yet). The alt
  text still renders.
- No styling is injected; the device's default EPUB typography applies
  (headings, emphasis, lists, and tables all render with xochitl's
  built-in styles).

### Plain text (`.txt`)

- Blank lines separate paragraphs (`<p>`); single newlines inside a
  paragraph become line breaks (`<br/>`). `\r\n` is normalized first.
- Deliberately **not** wrapped in `<pre>`: pre-formatted blocks don't
  reflow, which is miserable on a small e-ink page. Long lines wrap like
  prose instead. If you need exact whitespace, convert to PDF yourself.
- Content is XML-escaped, so `<`, `&`, etc. are always safe.

### Both

- The EPUB title and the device's visible name are the same string:
  `--name` if given, else the file stem.
- Files must be UTF-8; anything else is rejected rather than mangled.
- The generated EPUB is minimal but valid EPUB 3: `mimetype` (stored
  uncompressed, first entry — the spec requires this and strict readers
  check it), `META-INF/container.xml`, a nav document, one XHTML content
  file, and the OPF manifest with a fresh `urn:uuid` identifier.

## One-way conversion

There is no way back to Markdown. `rmu download` of an imported text file
returns the **EPUB** (or an `.rmdoc` bundle including your annotations
with `--bundle`), not the original `.md`/`.txt`. Treat the source file on
your computer as the canonical copy; re-upload after editing (delete the
old copy first, or upload under a new name — sibling name conflicts are
rejected).

## Sharp edges to remember

- xochitl's EPUB renderer is stricter than desktop readers. If a
  conversion feature is added, verify on a real device — "opens in Calibre"
  proves nothing.
- The device generates its own PDF rendition of an EPUB in the background
  (`<uuid>.pdf` appears next to `<uuid>.epub` after first open); deleting
  through `rmu rm` handles this via the `<uuid>.*` artifact glob.
- Page counts for reflowed EPUBs depend on the device's current font
  settings; `pageCount` in `.content` is bookkeeping, not truth.
