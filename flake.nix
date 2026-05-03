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
              pkgs.python313
              pkgs.prek
              pkgs.uv
              pkgs.sqlite
            ];
            shellHook = ''
              export UV_PYTHON_PREFERENCE=only-system
              uv sync --dev
              source .venv/bin/activate
              # Prefer the Nix-provided binary because the wheel-installed one
              # is not runnable on NixOS.
              export PATH="${pkgs.prek}/bin:$PATH"
              prek install
            '';
          };
        }
      );
    };
}
