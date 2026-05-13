{
  description = "Media Consolidator & Cataloger";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      supportedSystems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
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
              pkgs.prek
              pkgs.python313
              pkgs.ruff
              pkgs.sqlite
              pkgs.uv
            ];
            shellHook = ''
              export UV_PYTHON_PREFERENCE=only-system
              uv sync --dev
              source .venv/bin/activate
              # Prefer Nix-provided native CLI tools because wheel-installed
              # binaries are not runnable on NixOS.
              export PATH="${pkgs.lib.makeBinPath [ pkgs.prek pkgs.ruff ]}:$PATH"
              prek install
            '';
          };
        }
      );
    };
}
