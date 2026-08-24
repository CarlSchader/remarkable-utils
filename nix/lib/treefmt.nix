_: {
  projectRootFile = "flake.nix";
  programs = {
    # Nix
    alejandra.enable = true;
    statix.enable = true;
    deadnix.enable = true;

    # Rust
    rustfmt.enable = true;
  };
}
