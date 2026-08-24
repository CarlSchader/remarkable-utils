{flake-utils, ...} @ inputs:
flake-utils.lib.meld inputs [
  ./checks.nix
  ./formatter.nix
  ./rust-packages.nix
  ./shells.nix
]
