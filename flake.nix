{
  description = "Brood development shell";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      crane,
      nixpkgs,
      rust-overlay,
      ...
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forEachSystem = nixpkgs.lib.genAttrs systems;
      pkgsFor =
        system:
        import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
    in
    {
      devShells = forEachSystem (system: import ./nix/dev-shells.nix { pkgs = pkgsFor system; });

      packages = forEachSystem (
        system:
        import ./nix/packages.nix {
          inherit crane;
          sourceInfo = self;
          pkgs = pkgsFor system;
        }
      );

      checks = forEachSystem (
        system:
        let
          packages = import ./nix/packages.nix {
            inherit crane;
            sourceInfo = self;
            pkgs = pkgsFor system;
          };
        in
        {
          lint = packages.lintCheck;
        }
      );

      formatter = forEachSystem (
        system:
        (import ./nix/packages.nix {
          inherit crane;
          sourceInfo = self;
          pkgs = pkgsFor system;
        }).formatter
      );
    };
}
