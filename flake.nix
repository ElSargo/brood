{
  description = "Manifold development shell";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forEachSystem = nixpkgs.lib.genAttrs systems;
    in
    {
      devShells = forEachSystem (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };

          rustToolchain = pkgs.rust-bin.stable.latest.default.override {
            extensions = [
              "clippy"
              "rust-analyzer"
              "rust-src"
              "rustfmt"
            ];
            targets = [ "wasm32-unknown-unknown" ];
          };

          nativeLibs = with pkgs; [
            libGL
            libgbm
            libinput
            libxkbcommon
            mesa
            seatd
            systemdLibs
            vulkan-loader
            wayland
          ];

          westonWithSimpleClients = pkgs.weston.overrideAttrs (old: {
            mesonFlags = map (
              flag: if flag == "-Dsimple-clients=" then "-Dsimple-clients=all" else flag
            ) old.mesonFlags;
          });

          devTools = with pkgs; [
            bashInteractive
            coreutils
            docker
            docker-buildx
            findutils
            git
            go
            jq
            pkg-config
            python3
            ripgrep
            rustToolchain
          ];

          smokeTools = with pkgs; [
            glmark2
            kitty
            mesa-demos
            vulkan-tools
            wayland-utils
            westonWithSimpleClients
          ];
        in
        {
          default = pkgs.mkShell {
            packages = devTools ++ nativeLibs ++ smokeTools;

            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath nativeLibs;
            RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";

            shellHook = ''
              export MANIFOLD_DEV_SHELL=1
              echo "Manifold dev shell: cargo, wasm32 target, Wayland/EGL libs, WSLg smoke tools, Docker, and Go are available."
            '';
          };
        }
      );

      formatter = forEachSystem (system: nixpkgs.legacyPackages.${system}.nixfmt);
    };
}
