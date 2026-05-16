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
    };
}
