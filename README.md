# remarkable-utils

Tools for working with the [reMarkable](https://remarkable.com) tablet.

Rust utilities live as members of a single Cargo workspace; Nix flake modules
under `nix/` handle packaging, dev shells, formatting, and checks.

## `rmu` — manage documents and folders over SSH

`rmu` talks to the tablet over SSH (USB connection by default:
`root@10.11.99.1`) and operates on the logical folder tree that xochitl
stores as flat metadata files. There is no config file — everything is a
flag.

```text
(root)
├─ Books/
│  ├─ Math/
│  │  └─ Linear Algebra (pdf)
│  └─ Physics (epub)
└─ Notes/
```

### Setup

Enable SSH on the tablet (Settings → Help → Copyrights and licenses shows
the root password), then install your key so no password is needed:

```sh
ssh-copy-id root@10.11.99.1
```

### Usage

```sh
rmu status                               # model, firmware, disk/RAM/battery, ...
rmu ls                                   # print the logical tree
rmu ls --show-uuid --folders-only
rmu ls --json                            # flat item list as JSON

rmu mkdir Books/Math                     # nested mkdir -p style
rmu mkdir Algebra --parent Books/Math

rmu upload ./sample.pdf                            # to root
rmu upload ./sample.pdf --parent Books/Math -n "Linear Algebra"
rmu upload ./backup.rmdoc --parent Notes           # restore an .rmdoc bundle

rmu download "Books/Math/Linear Algebra"           # to ./Linear Algebra.pdf
rmu download Books/Physics ./downloads/
rmu download "Notes/Meeting Notes"                 # notebooks -> .rmdoc bundle
rmu download Books/Physics --bundle                # force .rmdoc (incl. annotations)
rmu download 'Books/vol-*' ./downloads/            # glob -> every match

rmu mv "Books/Physics" Notes             # move into another folder
rmu mv Notes/Physics /                   # move to root
rmu mv 'Books/math-*' Archive            # move every match
rmu rename Notes/Physics "Physics II"

rmu rm "Books/Math/Linear Algebra"
rmu rm Books --recursive                 # delete non-empty folder
rmu rm old.pdf drafts notes.epub -r      # several at once (all validated first)
rmu rm 'Books/math-books-volumes-*'      # glob delete
rmu rm 'Drafts/**'                       # empty a folder (keeps the folder)
rmu empty-trash                          # permanently delete trashed items
```

Targets are logical paths or item UUIDs; ambiguous paths are rejected
instead of guessed. `rm`, `mv`, and `download` also take glob patterns
matched against logical paths (quote them so your shell doesn't expand
them locally): `*`, `?`, and `[...]` match within one path segment,
`**` crosses segments, and a trailing `**` matches everything *inside*
a folder without the folder itself. An item whose name literally
contains a glob character is still addressed exactly — exact matches
win over pattern expansion. A pattern that matches nothing is an
error, and everything a pattern expands to is validated before
anything is written. Uploads accept `.pdf` and `.epub` (what the device
renders natively) and `.rmdoc` bundles. After write operations `rmu`
restarts xochitl so changes appear on the device (`--no-restart` to
skip).

Native handwritten notebooks have no single payload file on the device, so
`rmu download` fetches them as `.rmdoc` bundles — a zip of the raw file set
in the same layout the official reMarkable apps export/import. `--bundle`
forces this for PDFs/EPUBs too, which captures your annotations (a bare
payload download does not). `rmu upload` restores `.rmdoc` bundles under a
fresh UUID, so re-importing a download never collides with the original
document. See `docs/notebook-data.md` for the details.

### Folder sync

`rmu sync` mirrors a folder one-way between your computer and the tablet's
logical tree, over SSH. Direction follows argument order (like `rsync`/
`scp`), and remote endpoints use scp syntax with full ssh-config
resolution:

```sh
rmu sync ./books remarkable:/Books          # PC -> tablet ("push")
rmu sync remarkable:/Books ./books          # tablet -> PC ("pull")
rmu sync --dry-run ./books remarkable:/     # show the plan, change nothing
rmu sync --two-way ./books remarkable:/Books        # bidirectional
rmu sync --two-way --conflict newest ./b remarkable:/b  # newer side wins
rmu sync --delete ./books remarkable:/Books # propagate deletions (opt-in)
rmu sync user@server:/docs remarkable:/Work # generic ssh host -> tablet
rmu sync remarkable:/ rm-backup:/           # tablet -> tablet (full fidelity)
rmu sync ./a user@server:/backup/a          # plain file sync (supported types)
```

Endpoints are classified at runtime: an ssh host with xochitl on it is a
tablet (logical document sync with conversions); anything else is a plain
file tree. Tablet↔tablet sync streams `.rmdoc` bundles between devices, so
notebooks and annotations arrive intact.

Only supported files participate (`.pdf`, `.epub`, `.rmdoc`); everything
else is left alone. Notebooks pull as `.rmdoc` bundles; mapped `.rmdoc`
files are pull-only (handwriting can't be merged — the tablet wins).

Sync keeps an archive per endpoint pair under `~/.local/state/rmu/`
(never inside the synced tree) recording what both sides looked like at
last sync, including content hashes: repeated syncs only transfer real
changes — touched-but-identical files and annotation-only device
changes are recognized by hash and skipped. If the archive is ever
lost, identical files re-pair automatically; only genuinely divergent
files surface. Anything that changed on the destination since the last
sync is skipped with a warning, never silently overwritten — unless
you opt into a `--conflict` policy (`newest`, `src`, `dst`).
`--delete` only ever removes files the sync itself created or tracked;
files that were never synced are never deleted. See
`docs/sync-design.md` for the full model.

Connecting over Wi-Fi or a non-default setup:

```sh
rmu --host 192.168.1.50 --user root -i ~/.ssh/remarkable ls
rmu -o ProxyJump=bastion ls              # any ssh -o option is accepted
```

### Multi-tablet library: `rmu hub`

`rmu hub` keeps one library on several tablets in sync, git style. An
**origin** (a git repository on a machine you control) holds the library
with its full history; each tablet is a working copy that you read,
annotate and write on as usual. A sync backs the tablet up to origin,
merges its changes with what other tablets pushed meanwhile, pushes the
result, and applies everyone else's changes to the tablet.

```sh
rmu hub init ~/library.git                      # a new bare repository
rmu hub init ssh://git@server.example/library.git  # or an existing empty one

rmu hub clone ~/library.git --name "Desk tablet"  # attach a tablet; first sync
rmu --host 192.168.1.60 hub clone ssh://git@server.example/library.git --name "Travel tablet"

rmu hub sync                       # back up, merge, push, apply
rmu hub status                     # changes since the last sync, conflict copies
rmu hub sync --dry-run             # what a sync would write and delete
rmu hub backup                     # back up to origin only; nothing is merged

rmu hub remote                     # the origin URL stored on the tablet
rmu hub remote set-url ssh://git@new-server.example/library.git
rmu hub detach --prune-ref         # forget the hub (documents stay), drop its backups
```

The tablet is chosen with the usual flags (`--host`, ssh config
aliases, ...). The origin's URL is stored on the tablet at clone time;
`--origin <url>` overrides it for one run, since ssh aliases differ
between computers; `remote set-url` changes it for good (the new URL
must hold the same repository). The computer running `rmu` needs git
2.30 or newer.

Things to know:

- **Keep the origin private and self-hosted.** It holds your complete
  library (documents and handwriting, with history); host it on a
  machine or server you control, reachable over ssh or as a local path.
  URLs with an embedded password are refused; use an ssh key or a git
  credential helper.
- **Not together with the reMarkable cloud.** The two would fight over
  the same documents, so `rmu hub` refuses a tablet that is paired with
  the cloud. Unpair it first.
- **The UI goes down briefly**, twice per sync: while the tablet is read
  and while changes are applied, each usually seconds; transfers happen
  with the UI up. A watchdog on the tablet starts the UI again if the
  computer disappears mid-sync (`--watchdog <secs>`, default 600).
- **Conflicts never lose ink.** When the same document changed on two
  tablets in ways that cannot be merged, the tablet that syncs second
  keeps origin's version under the original name and gets its own as a
  conflict copy (`<name> (conflict <tablet> <date>)`); rename or delete
  the copy once you have looked at it. An edit wins over a deletion,
  and reading position and zoom stay per tablet.
- **Large deletions need consent.** A sync that would delete many
  documents (more than 10 and more than 10% of the library, more than
  50, or all of them because the tablet suddenly looks empty) stops
  before pushing and lists them; rerun with
  `--allow-deletions` if that is what you want.
- **Every state is recoverable**: every sync is a git commit on origin,
  and every tablet's own state is backed up to its own ref on origin
  (`refs/rmu/devices/<id>`) before anything is merged.

Recovery:

- *Interrupted sync* (cable pulled, laptop closed): run `rmu hub sync`
  again; it works out what the interrupted run did and carries on.
- *"The tablet is locked"*: another sync is running, or one died. A lock
  goes stale by itself once the tablet reboots or the watchdog fires;
  otherwise `--break-lock` takes it over once you are sure no other run
  is working on the tablet.
- *An interrupted sync and then a reattach*: `clone --reattach` refuses
  while the interrupted sync's journal is pending; run `rmu hub sync`
  first.
- *Origin rewound or replaced*: `rmu hub clone <origin> --reattach`
  attaches the tablet again, keeping its documents and device id;
  versions that match origin's history fast-forward, the rest merge as
  new. A tablet whose hub state was lost or detached attaches with a
  plain `rmu hub clone <origin>` the same way.

Every `rmu hub` command prints a summary on a terminal and JSON with
`--json`: the command's report as the library defines it, plus a
`command` field (fields are only ever added), or `{"error": {"kind",
"exit_code", "message"}}`. When stdout is not a terminal it prints plain
results instead, and the summary goes to stderr: the repository id for
`init`, the commit the tablet is at for `clone` and `sync`, the backup
commit for `backup`, `A`/`M`/`D <id>` per change for `status`,
`write`/`delete <id>` per document for `sync --dry-run`, the stored URL
for `remote`, and the deleted ref for `detach --prune-ref`. Exit
statuses:

| Status | Meaning                                                           |
|--------|-------------------------------------------------------------------|
| 0      | success                                                           |
| 1      | error                                                             |
| 2      | invalid arguments                                                 |
| 3      | the deletion guard refused (see `--allow-deletions`)              |
| 4      | the tablet is locked or busy (see `--break-lock`)                 |
| 5      | origin does not match the tablet (see `clone --reattach`)         |
| 6      | the tablet is paired with the reMarkable cloud                    |
| 7      | an interrupted sync must be finished first (`rmu hub sync`)       |

See `docs/hub-design.md` for the full design.

### Authentication

Key-based auth (your ssh config, keys, and agent) is the default and the
recommended setup. Password auth is available:

```sh
rmu --password ls                        # prompts once, hidden input
rmu --password-file ~/.rmu-pass ls       # first line of the file
RMU_SSH_PASSWORD=... rmu ls              # for scripting
```

Password mode uses `SSH_ASKPASS` under the hood and requires OpenSSH 8.4+
(any recent Linux or macOS). The password is passed to ssh via the
environment, never argv.

### Output & scripting

stdout carries only machine-usable results: the `ls` tree/JSON, the path a
download was written to, and the UUID of a created folder/document.
Progress bars and status messages go to stderr (bars auto-disable when
stderr is not a terminal; `-q`/`--quiet` silences both). So things like
this work cleanly:

```sh
open "$(rmu download Books/Physics)"
uuid=$(rmu -q upload ./sample.pdf)
```

## Layout

| Path                   | Description                                       |
|------------------------|---------------------------------------------------|
| `libremarkable-utils/` | Library crate: SSH transport, xochitl file model, logical-tree operations |
| `rmu/`                 | `rmu` CLI — entry point for tablet utilities (`rmu hub` in `rmu/src/hub.rs`) |
| `nix/`                 | Flake modules (packages, shells, checks, treefmt) |

## Development

```sh
nix develop            # dev shell (rust toolchain, formatters, pre-commit hooks)
cargo run -p rmu -- --help
cargo build --workspace
cargo test --workspace
nix fmt                # rustfmt + nix formatters via treefmt
nix flake check        # pre-commit hooks + clippy
nix build .#rmu        # build the CLI via crane
```

With [direnv](https://direnv.net), `direnv allow` activates the dev shell
automatically (`.envrc` is `use flake`).

## Contributing

Pull requests are welcome. Before submitting, make sure `nix flake check`
passes (it runs the same treefmt + clippy checks as the pre-commit hooks).
Unless you state otherwise, any contribution you intentionally submit for
inclusion is dual-licensed as below, without additional terms or conditions.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

This project is not affiliated with or endorsed by reMarkable AS.
Inspired by [cosmolei/remarkable_import](https://github.com/cosmolei/remarkable_import).
