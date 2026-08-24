{
  nixpkgs,
  crane,
  rust-overlay,
  flake-utils,
  ...
}:
flake-utils.lib.eachDefaultSystem (system: let
  pkgs = import nixpkgs {
    inherit system;
    overlays = [rust-overlay.overlays.default];
  };
  inherit (pkgs) lib;

  rustToolchain = pkgs.rust-bin.stable.latest.default.override {
    extensions = ["clippy" "rustfmt" "rust-src"];
  };

  craneLib = (crane.mkLib pkgs).overrideToolchain (_: rustToolchain);

  src = craneLib.cleanCargoSource ./..;
  commonArgs = {
    inherit src;
    strictDeps = true;
    pname = "remarkable-utils";
    version = "0.1.0";
  };
  cargoArtifacts = craneLib.buildDepsOnly commonArgs;

  individualCrateArgs =
    commonArgs
    // {
      inherit cargoArtifacts;
    };
  fileSetForCrate = crate:
    lib.fileset.toSource {
      root = ./..;
      fileset = lib.fileset.unions [
        ../Cargo.toml
        ../Cargo.lock
        # Workspace library crates every binary crate may depend on.
        (craneLib.fileset.commonCargoSources ../libremarkable-utils)
        (craneLib.fileset.commonCargoSources crate)
      ];
    };

  vendorDir = craneLib.vendorCargoDeps {src = ./..;};
in {
  packages = {
    inherit rustToolchain;
    cargoVendorDir = vendorDir;

    clippy-check = craneLib.cargoClippy (commonArgs
      // {
        inherit cargoArtifacts;
        cargoClippyExtraArgs = "--all-features --all-targets -- -D warnings";
      });

    rmu = craneLib.buildPackage (
      individualCrateArgs
      // {
        # Crate versions are inherited from [workspace.package], which crane's
        # crateNameFromCargoToml cannot resolve; version comes from commonArgs.
        pname = "rmu";
        cargoExtraArgs = "-p rmu";
        src = fileSetForCrate ../rmu;
      }
    );
  };
})
