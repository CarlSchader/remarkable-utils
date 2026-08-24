{
  self,
  nixpkgs,
  flake-utils,
  treefmt-nix,
  git-hooks,
  rust-overlay,
  ...
}:
flake-utils.lib.eachDefaultSystem (
  system: let
    pkgs = import nixpkgs {
      inherit system;
      overlays = [rust-overlay.overlays.default];
    };
    treefmtEval = treefmt-nix.lib.evalModule pkgs ./lib/treefmt.nix;
    preCommitCheck = git-hooks.lib.${system}.run {
      src = self;
      inherit
        (import ./lib/git-hooks.nix {
          inherit pkgs;
          rustToolchain = self.packages.${system}.rustToolchain;
          vendorDir = self.packages.${system}.cargoVendorDir;
          treefmtWrapper = treefmtEval.config.build.wrapper;
        })
        hooks
        ;
    };
  in {
    checks = {
      pre-commit = preCommitCheck;
      clippy = self.packages.${system}.clippy-check;
    };
  }
)
