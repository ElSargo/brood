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
            cairo
            gtk4
            libadwaita
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

          gtkRuntime = with pkgs; [
            cairo
            gdk-pixbuf
            glib
            gobject-introspection-unwrapped
            graphene
            gsettings-desktop-schemas
            gtk4
            harfbuzz
            hicolor-icon-theme
            libadwaita
            pango.out
          ];

          westonWithSimpleClients = pkgs.weston.overrideAttrs (old: {
            mesonFlags = map (
              flag: if flag == "-Dsimple-clients=" then "-Dsimple-clients=all" else flag
            ) old.mesonFlags;
          });

          sessionSwitcherPython = pkgs.python3.withPackages (
            ps: with ps; [
              dbus-python
              pycairo
              pygobject3
            ]
          );

          devTools = with pkgs; [
            bashInteractive
            coreutils
            findutils
            git
            pkg-config
            sessionSwitcherPython
            ripgrep
            rustToolchain
          ];

          smokeTools = with pkgs; [
            dbus
            gobject-introspection
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

          nativeLibs = with pkgs; [
            cairo
            gtk4
            libadwaita
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

          gtkRuntime = with pkgs; [
            cairo
            gdk-pixbuf
            glib
            gobject-introspection-unwrapped
            graphene
            gsettings-desktop-schemas
            gtk4
            harfbuzz
            hicolor-icon-theme
            libadwaita
            pango.out
          ];

          westonWithSimpleClients = pkgs.weston.overrideAttrs (old: {
            mesonFlags = map (
              flag: if flag == "-Dsimple-clients=" then "-Dsimple-clients=all" else flag
            ) old.mesonFlags;
          });

          sessionSwitcherPython = pkgs.python3.withPackages (
            ps: with ps; [
              dbus-python
              pycairo
              pygobject3
            ]
          );

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
            firefox
            sessionSwitcherPython
            ripgrep
            rustToolchain
          ];

          smokeTools = with pkgs; [
            dbus
            gobject-introspection
            glmark2
            kitty
            mesa-demos
            vulkan-tools
            wayland-utils
            westonWithSimpleClients
          ];

          devToolPath = lib.makeBinPath (devTools ++ smokeTools);
          nativeLibraryPath = lib.makeLibraryPath (nativeLibs ++ gtkRuntime);
          gtkTypelibPath = lib.makeSearchPath "lib/girepository-1.0" gtkRuntime;
          gtkDataDirs = lib.makeSearchPath "share" gtkRuntime;
          nativePkgConfigPath = lib.concatStringsSep ":" [
            (lib.makeSearchPathOutput "dev" "lib/pkgconfig" nativeLibs)
            (lib.makeSearchPath "lib/pkgconfig" nativeLibs)
            (lib.makeSearchPathOutput "dev" "share/pkgconfig" nativeLibs)
            (lib.makeSearchPath "share/pkgconfig" nativeLibs)
          ];

          cargoNix = pkgs.callPackage ./Cargo.nix {
            defaultCrateOverrides = pkgs.defaultCrateOverrides // {
              "av-scenechange" = attrs: {
                CARGO_ENCODED_RUSTFLAGS = attrs.CARGO_ENCODED_RUSTFLAGS or "";
              };
              rav1e = attrs: {
                CARGO_ENCODED_RUSTFLAGS = attrs.CARGO_ENCODED_RUSTFLAGS or "";
              };
              manifoldd = attrs: {
                nativeBuildInputs = (attrs.nativeBuildInputs or [ ]) ++ [ pkgs.pkg-config ];
                buildInputs = (attrs.buildInputs or [ ]) ++ nativeLibs;
              };
            };
          };

          manifoldd = cargoNix.workspaceMembers."manifoldd".build.override {
            runTests = false;
          };

          devWorkflow = pkgs.writeShellApplication {
            name = "manifold-dev-workflow";
            runtimeInputs = with pkgs; [
              bash
              coreutils
              findutils
              git
              util-linux
            ];
            text = ''
              set -euo pipefail

              MANIFOLDD_BIN="''${MANIFOLDD_BIN:-${manifoldd}/bin/manifoldd}"
              REPO_URL="''${MANIFOLD_DEV_REPO_URL:-https://github.com/ElSargo/manifold.git}"
              CACHE_HOME="''${XDG_CACHE_HOME:-$HOME/.cache}"
              CACHE_ROOT="''${MANIFOLD_DEV_CACHE_DIR:-$CACHE_HOME/manifold/dev-workflow}"
              SOURCE_DIR="''${MANIFOLD_DEV_SOURCE_DIR:-$CACHE_ROOT/source}"
              RUN_ROOT="''${MANIFOLD_DEV_RUN_ROOT:-$CACHE_ROOT/runs}"
              RUST_LOG="''${RUST_LOG:-manifoldd=info,smithay=warn}"

              export PATH="${sessionSwitcherPython}/bin:${devToolPath}:$PATH"
              export LD_LIBRARY_PATH="${nativeLibraryPath}:''${LD_LIBRARY_PATH:-}"
              export GI_TYPELIB_PATH="${gtkTypelibPath}:''${GI_TYPELIB_PATH:-}"
              export XDG_DATA_DIRS="${gtkDataDirs}:''${XDG_DATA_DIRS:-}"
              export PKG_CONFIG_PATH="${nativePkgConfigPath}:''${PKG_CONFIG_PATH:-}"
              export RUST_SRC_PATH="${rustToolchain}/lib/rustlib/src/rust/library"
              export MANIFOLD_DEV_SHELL=1

              "${sessionSwitcherPython}/bin/python3" - <<'PY'
              import dbus
              import dbus.mainloop.glib
              import cairo
              import gi

              gi.require_foreign("cairo")
              gi.require_version("Gdk", "4.0")
              gi.require_version("Gtk", "4.0")
              gi.require_version("Adw", "1")
              from gi.repository import Adw, Gdk, Gtk  # noqa: F401
              PY

              if [[ -z "''${WAYLAND_DISPLAY:-}" && -z "''${DISPLAY:-}" ]]; then
                echo "ERR this workflow needs a host graphical session (WAYLAND_DISPLAY or DISPLAY)" >&2
                exit 2
              fi

              ensure_source() {
                mkdir -p "$(dirname "$SOURCE_DIR")"
                if [[ -d "$SOURCE_DIR/.git" ]]; then
                  echo "source=$SOURCE_DIR"
                  return
                fi
                if [[ -e "$SOURCE_DIR" ]] && [[ -n "$(find "$SOURCE_DIR" -mindepth 1 -maxdepth 1 -print -quit 2>/dev/null)" ]]; then
                  echo "ERR MANIFOLD_DEV_SOURCE_DIR exists but is not a git checkout: $SOURCE_DIR" >&2
                  exit 2
                fi
                git clone "$REPO_URL" "$SOURCE_DIR"
                if [[ -n "''${MANIFOLD_DEV_REF:-}" ]]; then
                  git -C "$SOURCE_DIR" checkout "$MANIFOLD_DEV_REF"
                fi
                echo "source=$SOURCE_DIR"
              }

              shell_quote() {
                printf "%q" "$1"
              }

              wait_for_control() {
                local name="$1"
                local owner_pid="$2"
                local log="$3"
                local label="$4"
                for _ in $(seq 1 200); do
                  if "$MANIFOLDD_BIN" ctl --dbus-name "$name" status-json >/dev/null 2>&1; then
                    return 0
                  fi
                  if [[ -n "$owner_pid" ]] && ! kill -0 "$owner_pid" 2>/dev/null; then
                    echo "ERR $label exited before its control endpoint was ready" >&2
                    tail -120 "$log" >&2 || true
                    exit 1
                  fi
                  sleep 0.1
                done
                echo "ERR timed out waiting for $label control endpoint" >&2
                tail -120 "$log" >&2 || true
                exit 1
              }

              build_wasm_examples() {
                local found=0
                local manifest
                while IFS= read -r manifest; do
                  found=1
                  echo "building_wasm=$manifest"
                  cargo build --manifest-path "$manifest" --target wasm32-unknown-unknown --release
                done < <(find "$SOURCE_DIR/examples" -mindepth 2 -maxdepth 2 -path "$SOURCE_DIR/examples/wasm-*/Cargo.toml" -print | sort)

                if [[ "$found" != "1" ]]; then
                  echo "ERR no WASM example Cargo.toml files found under $SOURCE_DIR/examples" >&2
                  return 2
                fi
              }

              spawn_default_dev_client() {
                if [[ -n "''${MANIFOLD_DEV_CLIENT:-}" ]]; then
                  local q_source
                  q_source="$(shell_quote "$SOURCE_DIR")"
                  "$MANIFOLDD_BIN" ctl --dbus-name "$DBUS_DESTINATION" spawn \
                    bash -lc "cd $q_source && exec ''${MANIFOLD_DEV_CLIENT}" >/dev/null
                else
                  "$MANIFOLDD_BIN" ctl --dbus-name "$DBUS_DESTINATION" spawn \
                    "${pkgs.kitty}/bin/kitty" --working-directory "$SOURCE_DIR" >/dev/null
                fi
              }

              cleanup() {
                if [[ "''${MANIFOLD_DEV_KEEP_RUNNING:-0}" == "1" ]]; then
                  return
                fi
                if [[ -n "''${DBUS_DESTINATION:-}" ]]; then
                  "$MANIFOLDD_BIN" ctl --dbus-name "$DBUS_DESTINATION" quit >/dev/null 2>&1 || true
                fi
                if [[ -n "''${SESSION_PID:-}" ]] && kill -0 "$SESSION_PID" 2>/dev/null; then
                  kill "$SESSION_PID" >/dev/null 2>&1 || true
                fi
              }
              trap cleanup EXIT INT TERM

              ensure_source

              mkdir -p "$RUN_ROOT"
              RUN_DIR="$(mktemp -d "$RUN_ROOT/run.XXXXXX")"
              exec 9>"$CACHE_ROOT/workflow.lock"
              flock 9

              DBUS_DESTINATION="org.manifold.DevWorkflow.p$$"
              SESSION_LOG="$RUN_DIR/session.log"
              WASM_BUILD_LOG="$RUN_DIR/wasm-build.log"

              if ! build_wasm_examples >"$WASM_BUILD_LOG" 2>&1; then
                echo "ERR failed to build WASM examples" >&2
                tail -120 "$WASM_BUILD_LOG" >&2 || true
                exit 1
              fi

              (
                cd "$SOURCE_DIR"
                RUST_LOG="$RUST_LOG" "$MANIFOLDD_BIN" run --no-session-runner --dbus-name "$DBUS_DESTINATION" --config "$SOURCE_DIR/manifold.toml"
              ) >"$SESSION_LOG" 2>&1 &
              SESSION_PID=$!

              wait_for_control "$DBUS_DESTINATION" "$SESSION_PID" "$SESSION_LOG" "Manifold session"

              spawn_default_dev_client

              echo "OK Manifold developer workflow ready"
              echo "source=$SOURCE_DIR"
              echo "run_dir=$RUN_DIR"
              echo "dbus_destination=$DBUS_DESTINATION"
              echo "wasm_build_log=$WASM_BUILD_LOG"
              echo "session_log=$SESSION_LOG"
              echo "Stop with Ctrl-C or close the Manifold presentation."

              wait "$SESSION_PID"
            '';
          };

        in
        {
          default = manifoldd;
          "dev-workflow" = devWorkflow;
          inherit
            devWorkflow
            manifoldd
            ;
        }
      );

      formatter = forEachSystem (system: nixpkgs.legacyPackages.${system}.nixfmt);
    };
}
