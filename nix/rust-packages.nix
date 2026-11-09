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
        (craneLib.fileset.commonCargoSources ../test-support)
        (craneLib.fileset.commonCargoSources crate)
      ];
    };

  vendorDir = craneLib.vendorCargoDeps {src = ./..;};

  # Tests drive the system `git` binary and create throwaway repositories.
  # Keep them hermetic: no user/system git config, a scratch HOME, and a
  # fabricated identity so commits work without any configuration.
  # The hub device-layer tests also run the generated tablet scripts
  # locally (Linux): with the GNU tools plus `flock`/`setsid` from
  # util-linux, and with BusyBox applets and `ash`, as on a tablet (named
  # by RMU_TEST_BUSYBOX, never put on PATH).
  # gitMinimal is the git `rmu` falls back to at runtime (see the wrapper
  # below), so the sandboxed tests exercise exactly that build.
  hermeticGitTestArgs = {
    nativeCheckInputs = [pkgs.gitMinimal] ++ lib.optional pkgs.stdenv.hostPlatform.isLinux pkgs.util-linux;
    preCheck = ''
      ${lib.optionalString pkgs.stdenv.hostPlatform.isLinux "export RMU_TEST_BUSYBOX=${pkgs.busybox}/bin/busybox"}
      export HOME="$(mktemp -d)"
      export GIT_CONFIG_GLOBAL=/dev/null
      export GIT_CONFIG_NOSYSTEM=1
      export GIT_AUTHOR_NAME="Test Author"
      export GIT_AUTHOR_EMAIL="author@example.invalid"
      export GIT_COMMITTER_NAME="Test Committer"
      export GIT_COMMITTER_EMAIL="committer@example.invalid"
    '';
  };
in {
  packages = {
    inherit rustToolchain;
    cargoVendorDir = vendorDir;

    clippy-check = craneLib.cargoClippy (commonArgs
      // {
        inherit cargoArtifacts;
        cargoClippyExtraArgs = "--all-features --all-targets -- -D warnings";
      });

    # Every workspace crate's tests; `rmu`'s build only tests `-p rmu`.
    workspace-tests = craneLib.cargoTest (commonArgs
      // hermeticGitTestArgs
      // {
        inherit cargoArtifacts;
        cargoTestExtraArgs = "--workspace";
        # A check, not a build input for anything: an empty output instead
        # of a target/ archive that references the whole deps closure.
        doInstallCargoArtifacts = false;
      });

    rmu = craneLib.buildPackage (
      individualCrateArgs
      // hermeticGitTestArgs
      // {
        # Crate versions are inherited from [workspace.package], which crane's
        # crateNameFromCargoToml cannot resolve; version comes from commonArgs.
        pname = "rmu";
        cargoExtraArgs = "-p rmu";
        src = fileSetForCrate ../rmu;
        nativeBuildInputs = [pkgs.makeWrapper];
        # rmu shells out to `git` and `ssh`. Suffix (not prefix) PATH so the
        # user's own binaries and their configuration win; ours are a fallback.
        # rmu only needs git plumbing (local, ssh and https transports), which
        # gitMinimal covers without full git's perl/python closure.
        postInstall = ''
          wrapProgram $out/bin/rmu \
            --suffix PATH : ${lib.makeBinPath [pkgs.gitMinimal pkgs.openssh]}
        '';
      }
    );
  };
})
