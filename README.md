# remarkable-utils

Tools for working with the [reMarkable](https://remarkable.com) tablet.

Rust utilities live as members of a single Cargo workspace; Nix flake modules
under `nix/` handle packaging, dev shells, formatting, and checks.

## Layout

| Path                   | Description                                       |
|------------------------|---------------------------------------------------|
| `libremarkable-utils/` | Library crate: shared reMarkable types and logic  |
| `rmu/`                 | `rmu` CLI — entry point for tablet utilities      |
| `nix/`                 | Flake modules (packages, shells, checks, treefmt) |

## Usage

```sh
nix develop            # dev shell (rust toolchain, formatters, pre-commit hooks)
cargo run -p rmu -- info
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
