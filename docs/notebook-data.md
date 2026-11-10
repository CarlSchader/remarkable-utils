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
| `<uuid>/`             | Per-page stroke data: one `<page-id>.rm` file per inked page (+ `*-metadata.json` on older firmware) |
| `<uuid>.thumbnails/`  | Page thumbnails, `<page-id>.png` (only for documents that have been opened) |
| `<uuid>.highlights/`  | Smart-highlight JSON (annotated PDFs/EPUBs, newer firmware) |
| `<uuid>.local`        | Device-local JSON, `{"contentFormatVersion": 2}` on firmware 3.28; one per document, none for folders |

Plus one file that belongs to no item: `.tree`, a binary cloud-sync cache
(it holds cloud account identifiers; it also exists on tablets that are not
currently paired). Nothing else lives at the top level of the data dir.

On firmware 3.28, notebooks created on the tablet have **no `.pagedata`**:
the page template lives in the `.content` page list instead (see below).
The `.pagedata` files seen there were written by upload tools.

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

## `.metadata` fields (firmware 3.28)

Documents written by xochitl carry exactly these keys:

```json
{
    "createdTime": "1767225600000",
    "lastModified": "1767312000000",
    "lastOpened": "1767398400000",
    "lastOpenedPage": 4,
    "new": false,
    "parent": "",
    "pinned": false,
    "source": "",
    "type": "DocumentType",
    "visibleName": "Example notebook"
}
```

Timestamps are millisecond strings; `lastOpened` is `"0"` for a document
that was never opened. `new` is the "new" badge and flips to `false` on
first open. Older firmware and other tools also write `deleted`,
`metadatamodified`, `modified`, `synced` and `version` (cloud-sync
bookkeeping); 3.28 does not write them for documents and leaves them
alone where they exist.

## `.content` structure (`formatVersion: 2`)

Folders have a `.content` of `[]`. Documents (PDF and notebook alike) have
an object:

```json
{
    "cPages": {
        "lastOpened": { "timestamp": "1:12", "value": "8c1f0b9e-0000-4000-8000-000000000002" },
        "original":   { "timestamp": "1:1",  "value": 2 },
        "pages": [
            { "id": "8c1f0b9e-0000-4000-8000-000000000001",
              "idx": { "timestamp": "1:2", "value": "ba" },
              "redir": { "timestamp": "1:2", "value": 0 } },
            { "id": "8c1f0b9e-0000-4000-8000-000000000003",
              "idx": { "timestamp": "1:5", "value": "bam" },
              "modifed": "1767312000000",
              "template": { "timestamp": "1:6", "value": "Blank" } },
            { "id": "8c1f0b9e-0000-4000-8000-000000000002",
              "idx": { "timestamp": "1:2", "value": "bb" },
              "redir": { "timestamp": "1:2", "value": 1 },
              "scrollTime": { "timestamp": "1:11", "value": "2026-01-02T00:00:00Z" },
              "verticalScroll": { "timestamp": "1:11", "value": 0 } }
        ],
        "uuids": [ { "first": "0f0e0d0c-0000-4000-8000-00000000a001", "second": 1 } ]
    },
    "coverPageNumber": 0,
    "customZoomCenterX": 0, "customZoomCenterY": 936,
    "customZoomOrientation": "portrait",
    "customZoomPageHeight": 1872, "customZoomPageWidth": 1404,
    "customZoomScale": 1,
    "documentMetadata": { "title": "Example", "authors": ["A. Author"] },
    "extraMetadata": { "LastPen": "Ballpointv2", "LastActiveTool": "primary" },
    "fileType": "pdf",
    "fontName": "", "lineHeight": -1, "margins": 100,
    "formatVersion": 2,
    "orientation": "portrait",
    "pageCount": 3,
    "pageTags": [], "tags": [],
    "sizeInBytes": "1048576",
    "textAlignment": "left", "textScale": 1,
    "zoomMode": "bestFit"
}
```

- **Registers.** Mutable values are `{"timestamp": "A:C", "value": ...}`:
  `A` is an author index into `cPages.uuids` (`second` is the index,
  `first` the author UUID), `C` a counter. A tablet writes everything as
  author `1`; author `0` with counter `0` (`"0:0"`) marks an initial
  default nobody has set. Timestamps from two tablets are therefore not
  comparable.
- **Page entries.** `id` is a random (v4) UUID, `idx` a fractional-index
  string that orders pages (`"ba"`, `"bb"`, ... with longer strings
  inserted between). `redir` is the index of the backing PDF page;
  **inserted note pages are the entries without `redir`** (and every
  page of a notebook). Optional: `deleted`, `template` (notebook pages),
  `scrollTime`/`verticalScroll` (reading position), and `modifed` (sic) —
  a plain millisecond string, not a register, set when the page's ink
  last changed (it matches the `.rm` file's mtime).
- **`cPages.original`** is the PDF's page count (`-1` before first open);
  **`cPages.lastOpened`** is the id of the page last viewed.
- **Derived fields**: `pageCount` (number of page entries), `sizeInBytes`
  (payload plus ink, as a string), `cPages.original` and
  `documentMetadata` (read from the PDF). `customZoomPageWidth`/`Height`
  were `1404`×`1872` on a Paper Pro, so they are a page coordinate space
  rather than the screen size.

### First open materialises the page list

A document imported by a tool (or by the cloud) can arrive with a minimal
`.content`: `"pages": []`, `pageCount: 0`, `sizeInBytes: "0"`, original
`-1`, no `cPages.uuids`. The first time it is opened, xochitl rewrites
`.content` completely: one page entry per PDF page with a **fresh random
id**, `original`, `uuids`, `pageCount`, `sizeInBytes`,
`documentMetadata`, zoom fields, `extraMetadata`, `tags`; and it sets
`new: false` in `.metadata`. Two tablets opening the same imported PDF
therefore produce different page lists for identical content.

### What changes when a document is only read

Evidence from a library of about a hundred items (file mtimes against the
timestamps inside the files; no before/after experiment):

- Opening a document rewrites `.metadata` (`lastOpened`,
  `lastOpenedPage`) **without** touching `lastModified` and without
  rewriting `.content`.
- Scrolling rewrites `.content` (`scrollTime`, `verticalScroll`,
  `cPages.lastOpened`, whose counter climbs into the hundreds on a book
  being read) and, in the observed cases, **does** move `lastModified`
  to the moment of the scroll, weeks after the last ink. One rewrite of
  both files without a `lastModified` bump was also seen.

So `lastModified` is not a reliable "content changed" signal, and tools
that compare documents must ignore it along with the reading-position
fields.

## The `.rm` page format

Each page is a binary "lines" file. Version matters:

- v3/v5: legacy format (pre-3.0 firmware); several third-party parsers exist.
- v6 (`reMarkable .lines file, version=6`): current firmware; a scene-tree
  format that also encodes text items. The Python `rmscene` library is the
  reference third-party parser; Rust coverage of v6 is limited.

A v6 file is a 43-byte ASCII header followed by blocks, each with an
8-byte header (`u32` length, one unknown byte, min version, current
version, block type). Pages written by firmware 3.28 contain, in
`rmscene`'s names: `AuthorIds` (0x09), `MigrationInfo` (0x00),
`PageInfo` (0x0A), `SceneInfo` (0x0D), `SceneTree` (0x01), `TreeNode`
(0x02), `SceneGroupItem` (0x04), then one `SceneLineItem` (0x05) per
stroke and `SceneTombstoneItem` (0x08) for erased ones.

**Author ids.** Every page has an `AuthorIds` block mapping author index
`1` to the same UUID as the document's `cPages.uuids` entry. The 16 UUID
bytes are stored in RFC 4122 (big-endian) order; `rmscene` decodes them as
little-endian, so its rendering of the UUID differs from the string in
`.content`. Scene item ids are CRDT ids `(author, counter)`: every stroke
and tombstone id uses author `1`; the built-in root and layer nodes use
author `0`. A page file is therefore self-contained: its author table
travels with it.

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
  tablet. On firmware 3.28 (Paper Pro) the unit allows 4 starts per 600 s
  and has `OnFailure=remarkable-fail.service emergency.target`, which is
  the mechanism behind that reboot.
- **Login sessions per ssh channel.** During a hub sync on a Paper Pro
  (firmware 3.28) reached over Tailscale, every rmu command showed up
  as a new login session (about 10 per second), enough to trip
  `user-runtime-dir@0.service`'s start limit; harmless otherwise. rmu
  does multiplex: a local OpenSSH server saw a whole `rmu hub sync`
  arrive as one authenticated connection carrying each command as a
  channel. OpenSSH's server opens its PAM session once per connection,
  so per-command login sessions come from a server that starts a login
  per channel instead; Tailscale SSH, for one, runs each session through
  `login`. If that is the path, plain ssh to the tablet's own server (at
  its tailnet address) should avoid it; nothing in rmu needs to change.
  To tell: `ssh -v` prints the server's version banner (which server
  answers), and `loginctl list-sessions` on the tablet shows the
  sessions while a sync runs. This cause is inferred, not yet checked on
  a tablet.
- **Cloud sync** is a separate service, `rm-sync.service`, enabled and
  running whether or not the tablet is paired; it is `BindsTo=`/`PartOf=`
  `xochitl.service`, so it stops and starts with xochitl. Pairing state
  is the `devicetoken` key in `/home/root/.config/remarkable/xochitl.conf`:
  an unpaired tablet had `devicetoken=@Invalid()` (Qt's encoding of an
  unset value), and xochitl logged that it cannot fetch a user token
  without a device token. Treat a missing key, an empty value,
  `@Invalid()` and `@ByteArray()` as unpaired. Check it without printing
  the value.

## Device shell environment (firmware 3.28)

The tools are BusyBox 1.36 applets plus a few real binaries
(`systemd-run` from systemd 255, with `--on-active`, `--unit`,
`--collect`, `--no-block`). Points that matter for scripts run over SSH:

- `stat -c` supports `%s %Y %Z %i %d`; `%y`/`%z` print mtime/ctime with
  **nanoseconds** (ext4), while `%Y`/`%Z` are whole seconds.
- `find` has `-print0`, `-exec ... +`, `-newer`, `-mmin`, but no
  `-printf` and no `-newermt`.
- `tar` reads a file list with `-T -` and supports `--no-recursion`, but
  refuses to create an empty archive (`tar: empty archive`, non-zero
  exit).
- `mv` has `-T`/`-n`/`-f`; `mktemp` has `-d`/`-p`; `flock`, `setsid`,
  `nohup`, `sha256sum` exist; `sync` takes no options; there is **no
  `timeout`** and `head` has no `-c`.
- `/proc/sys/kernel/random/boot_id` exists.
- `/home` is one ext4 filesystem (on the Paper Pro an encrypted
  device-mapper volume), so anything under `/home/root` can be renamed
  atomically into the data dir.

## References

- [rmscene](https://github.com/ricklupton/rmscene) — `.rm` v6 parser (Python)
- [remarkable_import](https://github.com/cosmolei/remarkable_import) — the
  SSH management approach `rmu` is based on
- [awesome-reMarkable](https://github.com/reHackable/awesome-reMarkable) —
  ecosystem index
