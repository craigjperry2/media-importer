{
  description = "Media Consolidator & Cataloger";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      # Nixpkgs 26.11 (nixos-unstable) dropped x86_64-darwin.
      supportedSystems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
      pkgsFor = system: import nixpkgs { inherit system; };
    in {
      devShells = forAllSystems (system:
        let
          pkgs = pkgsFor system;
        in {
          default = pkgs.mkShell {
            packages = [
              pkgs.bash
              pkgs.cargo
              pkgs.clippy
              pkgs.prek
              pkgs.rust-analyzer
              pkgs.rustc
              pkgs.rustfmt
              pkgs.sqlite
            ];
            shellHook = ''
              prek install
            '';
          };
        }
      );

      # The release package, which hosts install from a tagged release of this
      # flake. Dependencies come from Cargo.lock, so Dependabot updates need no
      # hash change here.
      packages = forAllSystems (system:
        let
          pkgs = pkgsFor system;
          inherit (pkgs) lib;
          crate = lib.importTOML ./crates/media-importer/Cargo.toml;
        in rec {
          media-importer = pkgs.rustPlatform.buildRustPackage {
            pname = crate.package.name;
            inherit (crate.package) version;
            src = lib.fileset.toSource {
              root = ./.;
              fileset = lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./crates
              ];
            };
            cargoLock.lockFile = ./Cargo.lock;
            # portable-pty starts PTY test children in $HOME, which Nix sets to
            # a directory that does not exist.
            preCheck = ''
              export HOME=$(mktemp -d)
            '';
            # These repository-policy tests read files outside the package
            # source; CI runs them.
            checkFlags = [
              "--skip=milestone_eleven_conformance_matrix_references_existing_named_behavior_tests"
              "--skip=milestone_eleven_conformance_matrix_validates_every_requirement_row_and_reference"
              "--skip=pre_commit_keeps_filename_independent_rust_hooks"
            ];
            meta = {
              description = "Import media into a content-addressed store with a SQLite catalog";
              mainProgram = "media-importer";
              platforms = lib.platforms.linux ++ lib.platforms.darwin;
            };
          };
          default = media-importer;
        }
      );

      # `nix flake check` builds the exact derivation hosts install, including
      # its release-profile test run.
      checks = forAllSystems (system: {
        inherit (self.packages.${system}) media-importer;
      });
    };
}
