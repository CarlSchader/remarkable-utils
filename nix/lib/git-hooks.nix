{
  pkgs,
  rustToolchain,
  vendorDir,
  treefmtWrapper,
  ...
}: let
  # symlinkJoin over the full toolchain (cargo+clippy+rustfmt+rustc together),
  # with cargo-clippy re-wrapped to set up CARGO_HOME with the crane vendor
  # config so the pre-commit clippy hook works offline. Mirrors crane's
  # configureCargoVendoredDepsHook:
  # cat ${vendorDir}/config.toml >> $CARGO_HOME/config.toml
  clippyWithVendor = pkgs.symlinkJoin {
    name = "clippy-with-crane-vendor";
    paths = [rustToolchain];
    nativeBuildInputs = [pkgs.makeWrapper];
    postBuild = ''
      rm -f $out/bin/cargo-clippy
      makeWrapper ${rustToolchain}/bin/cargo-clippy $out/bin/cargo-clippy \
        --run '
          if [ -z "''${CARGO_HOME:-}" ]; then
            export CARGO_HOME=$(mktemp -d -t cargo-home-XXXXXX)
          fi
          mkdir -p "$CARGO_HOME"
          if ! grep -q "nix-sources-" "$CARGO_HOME/config.toml" 2>/dev/null; then
            cat ${vendorDir}/config.toml >> "$CARGO_HOME/config.toml"
          fi
        '
    '';
  };
in {
  hooks = {
    treefmt = {
      enable = true;
      package = treefmtWrapper;
    };
    clippy = {
      enable = true;
      packageOverrides.cargo = rustToolchain;
      packageOverrides.clippy = clippyWithVendor;
      settings.denyWarnings = true;
      settings.allFeatures = true;
    };
  };
}
