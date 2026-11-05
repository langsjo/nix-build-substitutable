{
  description = "Build only the derivations a binary cache client would actually fetch";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";

  outputs =
    { self, nixpkgs }:
    let
      forAllSystems = nixpkgs.lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "nix-build-substitutable";
            version = "0.1.0";
            src = self;
            cargoLock = {
              lockFile = ./Cargo.lock;
              # Harmonia is not on crates.io; this reads its pinned rev from Cargo.lock.
              allowBuiltinFetchGit = true;
            };
            # The end-to-end tests run real Nix in a private store.
            nativeCheckInputs = [ pkgs.nix ];
            meta.mainProgram = "nix-build-substitutable";
          };
        }
      );

      devShells = forAllSystems (system: {
        default = nixpkgs.legacyPackages.${system}.mkShell {
          inputsFrom = [ self.packages.${system}.default ];
          packages = with nixpkgs.legacyPackages.${system}; [
            clippy
            rustfmt
          ];
        };
      });
    };
}
