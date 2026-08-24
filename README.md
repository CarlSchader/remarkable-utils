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

rmu mv "Books/Physics" Notes             # move into another folder
rmu mv Notes/Physics /                   # move to root
rmu rename Notes/Physics "Physics II"

rmu rm "Books/Math/Linear Algebra"
rmu rm Books --recursive                 # delete non-empty folder
```

Targets are logical paths or item UUIDs; ambiguous paths are rejected
instead of guessed. Only `.pdf` and `.epub` uploads are supported (that is
what the device renders). After write operations `rmu` restarts xochitl so
changes appear on the device (`--no-restart` to skip).

Native handwritten notebooks have no single payload file on the device, so
`rmu download` fetches them as `.rmdoc` bundles — a zip of the raw file set
in the same layout the official reMarkable apps export/import. `--bundle`
forces this for PDFs/EPUBs too, which captures your annotations (a bare
payload download does not). `rmu upload` restores `.rmdoc` bundles under a
fresh UUID, so re-importing a download never collides with the original
document. See `docs/notebook-data.md` for the details.

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
