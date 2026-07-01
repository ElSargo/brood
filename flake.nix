{
  description = "Manifold development shell";

  nixConfig = {
    extra-substituters = [ "https://manifold-platform.cachix.org" ];
    extra-trusted-public-keys = [
      "manifold-platform.cachix.org-1:cXJupbtXzFJe8vIIyt+ZYN+ganoykFuUIZOPfFQ+GP0="
    ];
  };

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
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

      packages = forEachSystem (system: import ./nix/packages.nix { pkgs = pkgsFor system; });

      checks = forEachSystem (
        system:
        let
          packages = import ./nix/packages.nix { pkgs = pkgsFor system; };
        in
        {
          lint = packages.lintCheck;
        }
      );

      formatter = forEachSystem (
        system: (import ./nix/packages.nix { pkgs = pkgsFor system; }).formatter
      );
    };
}
