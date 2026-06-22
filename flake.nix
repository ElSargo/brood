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

      packages = forEachSystem (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };

          lib = pkgs.lib;

          rustToolchain = pkgs.rust-bin.stable.latest.default.override {
            targets = [ "wasm32-unknown-unknown" ];
          };

          rustPlatform = pkgs.makeRustPlatform {
            cargo = rustToolchain;
            rustc = rustToolchain;
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

          source = lib.cleanSourceWith {
            src = ./.;
            filter =
              path: type:
              let
                base = baseNameOf path;
              in
              !(
                type == "directory"
                && builtins.elem base [
                  "target"
                  ".git"
                ]
              );
          };

          manifoldd = rustPlatform.buildRustPackage {
            pname = "manifoldd";
            version = "0.1.0";
            src = source;
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = with pkgs; [ pkg-config ];
            buildInputs = nativeLibs;
            cargoBuildFlags = [
              "-p"
              "manifoldd"
            ];
            cargoTestFlags = [
              "-p"
              "manifoldd"
            ];
          };

          firefoxCloseReattachHarness =
            pkgs.runCommand "manifold-firefox-close-reattach-harness.wasm"
              {
                nativeBuildInputs = [ rustToolchain ];
              }
              ''
                rustc \
                  --edition=2024 \
                  --crate-name manifold_firefox_close_reattach_harness \
                  --crate-type cdylib \
                  --target wasm32-unknown-unknown \
                  -O \
                  ${./examples/wasm-firefox-close-reattach-harness/src/lib.rs} \
                  -o "$out"
              '';

          firefoxCloseReattachTestScript =
            pkgs.runCommand "manifold-firefox-close-reattach-test-script"
              {
                nativeBuildInputs = [ pkgs.makeWrapper ];
              }
              ''
                mkdir -p "$out/bin"
                makeWrapper ${pkgs.bash}/bin/bash "$out/bin/manifold-firefox-close-reattach-test" \
                  --add-flags ${./tools/run-headless-firefox-close-reattach.sh} \
                  --prefix PATH : "${
                    lib.makeBinPath [
                      pkgs.bash
                      pkgs.coreutils
                      pkgs.firefox
                      pkgs.gnugrep
                      pkgs.gnused
                      pkgs.jq
                      westonWithSimpleClients
                    ]
                  }" \
                  --set LD_LIBRARY_PATH "${lib.makeLibraryPath nativeLibs}" \
                  --set MANIFOLDD_BIN "${manifoldd}/bin/manifoldd" \
                  --set HARNESS_WASM "${firefoxCloseReattachHarness}" \
                  --set FIREFOX_BIN "${pkgs.firefox}/bin/firefox" \
                  --set WESTON_SIMPLE_SHM "${westonWithSimpleClients}/bin/weston-simple-shm"
              '';

          firefoxCloseReattachTest =
            pkgs.runCommand "manifold-firefox-close-reattach-test"
              {
                nativeBuildInputs = [ firefoxCloseReattachTestScript ];
              }
              ''
                export EGL_PLATFORM=wayland
                export LIBGL_ALWAYS_SOFTWARE=1
                export LIBGL_DRIVERS_PATH="${pkgs.mesa}/lib/dri"
                export MESA_LOADER_DRIVER_OVERRIDE=llvmpipe
                export __EGL_VENDOR_LIBRARY_FILENAMES="${pkgs.mesa}/share/glvnd/egl_vendor.d/50_mesa.json"
                export WORKDIR="$TMPDIR/firefox-close-reattach"

                manifold-firefox-close-reattach-test

                mkdir -p "$out"
                mkdir -p "$out/artifacts/nested"
                cp "$WORKDIR/test.log" "$out/artifacts/test.log"
                cp "$WORKDIR"/nested/*.log "$out/artifacts/nested/" 2>/dev/null || true
                echo "OK" > "$out/result"
              '';
        in
        {
          default = manifoldd;
          inherit
            manifoldd
            firefoxCloseReattachHarness
            firefoxCloseReattachTestScript
            firefoxCloseReattachTest
            ;
        }
      );

      checks = forEachSystem (system: {
        firefox-close-reattach = self.packages.${system}.firefoxCloseReattachTest;
      });

      formatter = forEachSystem (system: nixpkgs.legacyPackages.${system}.nixfmt);
    };
}
