# Notebook data on the reMarkable

Notes on how xochitl stores documents — in particular native handwritten
notebooks, which behave differently from imported PDFs/EPUBs. This is the
domain knowledge behind `libremarkable-utils`; keep it updated when firmware
behavior changes.

## Storage model recap

Every item (document or folder) is a UUID plus a flat set of files in
`/home/root/.local/share/remarkable/xochitl`:

| File                  | Contents |
|-----------------------|----------|
| `<uuid>.metadata`     | JSON: `visibleName`, `parent`, `type` (`DocumentType` / `CollectionType`), timestamps |
| `<uuid>.content`      | JSON: `fileType`, page list, layout settings |
| `<uuid>.pagedata`     | One template name per line, one line per page (may be empty/missing) |
| `<uuid>.<fileType>`   | Document payload — **only for `pdf` and `epub`** |
| `<uuid>/`             | Per-page stroke data: one `.rm` file per page (+ `*-metadata.json` on older firmware) |
| `<uuid>.thumbnails/`  | Page thumbnails (PNG/JPG) |
| `<uuid>.highlights/`  | Smart-highlight JSON (annotated PDFs/EPUBs, newer firmware) |

The folder tree is purely logical: `parent` is the UUID of the containing
`CollectionType`, `""` for root, or the pseudo-values `"trash"` (deleted via
UI) — never a real item.

## `fileType` values and their nuances

| `fileType`   | Meaning | Payload file? |
|--------------|---------|---------------|
| `"pdf"`      | Imported PDF | `<uuid>.pdf` |
| `"epub"`     | Imported EPUB | `<uuid>.epub` **and** usually a device-generated `<uuid>.pdf` rendition |
| `"notebook"` | Native handwritten notebook (firmware 3.x) | **none** |
| `""` / absent| Native notebook on older firmware | **none** |

Key nuances:

- **Notebooks have no single payload file.** All content lives in the
  `<uuid>/` directory as per-page `.rm` files. `cat <uuid>.notebook` fails —
  there is nothing to `cat`. Any code that assumes `<uuid>.<fileType>`
  exists must special-case notebooks (and treat empty `fileType` as
  "notebook" when a `.content` file is present).
- **Annotated PDFs/EPUBs also have a `<uuid>/` directory.** Downloading just
  the payload loses the annotations; a faithful copy must take the whole
  file set.
- **Page bookkeeping lives in `.content`,** and its shape varies by
  firmware: old format has a flat `"pages": [<page-uuid>, ...]` array; new
  format (`formatVersion: 2`) uses `"cPages": {"pages": [{"id": ...}, ...]}`.
  `pageCount` may be stale; the `.rm` files in `<uuid>/` are the truth.

## The `.rm` page format

Each page is a binary "lines" file. Version matters:

- v3/v5: legacy format (pre-3.0 firmware); several third-party parsers exist.
- v6 (`reMarkable .lines file, version=6`): current firmware; a scene-tree
  format that also encodes text items. The Python `rmscene` library is the
  reference third-party parser; Rust coverage of v6 is limited.

Rendering `.rm` to PDF/SVG is deliberately out of scope for now (see
AGENTS.md); we move raw files instead.

## `.rmdoc` bundles

The official reMarkable apps (and the device web UI, firmware ≥ ~3.9)
export/import `.rmdoc` files: a **zip archive of the raw xochitl file set**
for one document (`<uuid>.metadata`, `<uuid>.content`, `<uuid>.pagedata`,
`<uuid>/*.rm`, thumbnails, and the payload when there is one), with paths
relative to the data dir.

`rmu download` uses this as its bundle format: notebooks always download as
`<name>.rmdoc`, and `--bundle` forces it for PDFs/EPUBs to capture
annotations. Interop with the official importer is expected but not
guaranteed across firmware versions — treat bundles primarily as faithful
backups.

`rmu upload` accepts `.rmdoc` bundles and restores them under a **fresh
document UUID**, rewriting `parent`/`visibleName`/`lastModified` in the
metadata while preserving all other fields verbatim. Page UUIDs inside the
bundle are kept as-is — they are scoped to the document and do not collide.
Re-targeting matters because the original document may still exist on the
device; importing under the exported UUID would silently merge/overwrite it.

The device ships busybox only (no `zip`), so both directions go through tar
(`libremarkable-utils`'s `bundle` module): downloads repack a device-side
`tar -c` stream into a zip locally; uploads unpack the zip locally and
stream one tar into `tar -x` in the data dir.

## Other quirks worth remembering

- **Timestamps are inconsistent:** `createdTime`/`lastModified` appear as
  JSON strings on some firmware and numbers on others. Parse leniently.
- **Unknown metadata fields must be preserved** on read-modify-write; field
  sets differ across firmware versions.
- **Deleting an item** means removing `<uuid>` *and* every `<uuid>.*`
  artifact (payload, metadata, content, pagedata, thumbnails, highlights).
- **xochitl must be restarted** to notice filesystem changes, and it has a
  strict systemd start limit — always `systemctl reset-failed
  xochitl.service` before `restart`, or repeated restarts can reboot the
  tablet.

## References

- [rmscene](https://github.com/ricklupton/rmscene) — `.rm` v6 parser (Python)
- [remarkable_import](https://github.com/cosmolei/remarkable_import) — the
  SSH management approach `rmu` is based on
- [awesome-reMarkable](https://github.com/reHackable/awesome-reMarkable) —
  ecosystem index
