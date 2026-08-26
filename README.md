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
else is left alone. Notebooks pull as `.rmdoc`
bundles; mapped `.rmdoc` files are pull-only
(handwriting can't be merged — the tablet wins). A `.rmu-sync.json` state
file in the local root (gitignore it) tracks the path↔document mapping so
repeated syncs only transfer changes, and anything that changed on the
destination since the last sync is skipped with a warning, never silently
overwritten — unless you opt into a `--conflict` policy (`newest`, `src`,
`dst`). `--delete` only ever removes files the sync itself created or
tracked; files that were never synced are never deleted. See
`docs/sync-design.md` for the full model.

Connecting over Wi-Fi or a non-default setup:

```sh
rmu --host 192.168.1.50 --user root -i ~/.ssh/remarkable ls
rmu -o ProxyJump=bastion ls              # any ssh -o option is accepted
```

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
| `rmu/`                 | `rmu` CLI — entry point for tablet utilities      |
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
