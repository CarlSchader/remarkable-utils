{
  nixpkgs,
  flake-utils,
  treefmt-nix,
  ...
}:
flake-utils.lib.eachDefaultSystem (
  system: let
    pkgs = import nixpkgs {inherit system;};
    treefmtEval = treefmt-nix.lib.evalModule pkgs ./lib/treefmt.nix;
  in {
    formatter = treefmtEval.config.build.wrapper;
  }
)
