{
  self,
  nixpkgs,
  flake-utils,
  rust-overlay,
  ...
}:
flake-utils.lib.eachDefaultSystem (
  system: let
    pkgs = import nixpkgs {
      inherit system;
      overlays = [rust-overlay.overlays.default];
    };
    preCommitCheck = self.checks.${system}.pre-commit;
    rustToolchain = self.packages.${system}.rustToolchain;
  in {
    devShells.default = pkgs.mkShellNoCC {
      # rustToolchain bundles cargo, rustc, clippy, rustfmt, and rust-src.
      nativeBuildInputs =
        [
          rustToolchain
          self.formatter.${system}
        ]
        ++ preCommitCheck.enabledPackages;

      shellHook = ''
        ${preCommitCheck.shellHook}
        export PROJECT_ROOT=$(git rev-parse --show-toplevel)
      '';
    };
  }
)
