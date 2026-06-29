{
  pkgs,
  lib,
  manifoldd,
  wasmExamples,
  sessionSwitcherPython,
  path,
  nativeLibraryPath,
  gtkTypelibPath,
  gtkDataDirs,
  gsettingsSchemaDir,
  nativePkgConfigPath,
  rustToolchain,
  exportDevVars,
  name,
}:

pkgs.writeShellApplication {
  inherit name;
  runtimeInputs = [
    pkgs.bash
    pkgs.coreutils
    pkgs.findutils
    pkgs.git
    pkgs.util-linux
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

    export PATH="${sessionSwitcherPython}/bin:${path}:$PATH"
    export LD_LIBRARY_PATH="${nativeLibraryPath}:''${LD_LIBRARY_PATH:-}"
    export GI_TYPELIB_PATH="${gtkTypelibPath}:''${GI_TYPELIB_PATH:-}"
    export XDG_DATA_DIRS="${gtkDataDirs}:''${XDG_DATA_DIRS:-}"
    export GSETTINGS_SCHEMA_DIR="${gsettingsSchemaDir}"
    export MANIFOLD_DEV_SHELL=1
    ${lib.optionalString exportDevVars ''
      export PKG_CONFIG_PATH="${nativePkgConfigPath}:''${PKG_CONFIG_PATH:-}"
      export RUST_SRC_PATH="${rustToolchain}/lib/rustlib/src/rust/library"
    ''}

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

    install_wasm_examples() {
      local found=0
      local wasm
      while IFS= read -r wasm; do
        found=1
        local rel="''${wasm#${wasmExamples}/}"
        local target="$SOURCE_DIR/$rel"
        mkdir -p "$(dirname "$target")"
        cp "$wasm" "$target"
      done < <(find "${wasmExamples}" -type f -name '*.wasm' -print | sort)

      if [[ "$found" != "1" ]]; then
        echo "ERR no packaged WASM example artifacts found in ${wasmExamples}" >&2
        return 2
      fi
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

    if ! install_wasm_examples >"$WASM_BUILD_LOG" 2>&1; then
      echo "ERR failed to install packaged WASM examples" >&2
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
}
