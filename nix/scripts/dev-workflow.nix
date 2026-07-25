{
  pkgs,
  lib,
  broodd,
  runtimeFiles,
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
    pkgs.foot
    pkgs.git
    pkgs.util-linux
  ];
  text = ''
    set -euo pipefail

    BROOD_BIN="''${BROOD_BIN:-''${BROODD_BIN:-${broodd}/bin/brood}}"
    REPO_URL="''${BROOD_DEV_REPO_URL:-https://github.com/ElSargo/brood.git}"
    CACHE_HOME="''${XDG_CACHE_HOME:-$HOME/.cache}"
    CACHE_ROOT="''${BROOD_DEV_CACHE_DIR:-$CACHE_HOME/brood/dev-workflow}"
    SOURCE_DIR="''${BROOD_DEV_SOURCE_DIR:-$CACHE_ROOT/source}"
    RUN_ROOT="''${BROOD_DEV_RUN_ROOT:-$CACHE_ROOT/runs}"
    RUST_LOG="''${RUST_LOG:-broodd=info,smithay=warn}"

    export PATH="${sessionSwitcherPython}/bin:${path}:$PATH"
    export LD_LIBRARY_PATH="${nativeLibraryPath}:''${LD_LIBRARY_PATH:-}"
    export GI_TYPELIB_PATH="${gtkTypelibPath}:''${GI_TYPELIB_PATH:-}"
    export XDG_DATA_DIRS="${gtkDataDirs}:''${XDG_DATA_DIRS:-}"
    export GSETTINGS_SCHEMA_DIR="${gsettingsSchemaDir}"
    export BROOD_DEV_SHELL=1
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
        echo "ERR BROOD_DEV_SOURCE_DIR exists but is not a git checkout: $SOURCE_DIR" >&2
        exit 2
      fi
      git clone "$REPO_URL" "$SOURCE_DIR"
      if [[ -n "''${BROOD_DEV_REF:-}" ]]; then
        git -C "$SOURCE_DIR" checkout "$BROOD_DEV_REF"
      fi
      echo "source=$SOURCE_DIR"
    }

    install_runtime_files() {
      local found=0
      local file
      while IFS= read -r file; do
        found=1
        local rel="''${file#${runtimeFiles}/}"
        local target="$SOURCE_DIR/$rel"
        mkdir -p "$(dirname "$target")"
        rm -f "$target"
        cp "$file" "$target"
        chmod u+w "$target"
      done < <(find "${runtimeFiles}" -type f -print | sort)

      if [[ "$found" != "1" ]]; then
        echo "ERR no packaged runtime files found in ${runtimeFiles}" >&2
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
        if "$BROOD_BIN" ctl --dbus-name "$name" status-json >/dev/null 2>&1; then
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
      if [[ -n "''${BROOD_DEV_CLIENT:-}" ]]; then
        local q_source
        q_source="$(shell_quote "$SOURCE_DIR")"
        "$BROOD_BIN" ctl --dbus-name "$DBUS_DESTINATION" spawn \
          bash -lc "cd $q_source && exec ''${BROOD_DEV_CLIENT}" >/dev/null
      else
        "$BROOD_BIN" ctl --dbus-name "$DBUS_DESTINATION" spawn \
          "${pkgs.foot}/bin/foot" --working-directory "$SOURCE_DIR" >/dev/null
      fi
    }

    cleanup() {
      if [[ "''${BROOD_DEV_KEEP_RUNNING:-0}" == "1" ]]; then
        return
      fi
      if [[ -n "''${DBUS_DESTINATION:-}" ]]; then
        "$BROOD_BIN" ctl --dbus-name "$DBUS_DESTINATION" quit >/dev/null 2>&1 || true
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

    DBUS_DESTINATION="org.brood.DevWorkflow.p$$"
    SESSION_LOG="$RUN_DIR/session.log"
    RUNTIME_INSTALL_LOG="$RUN_DIR/runtime-install.log"

    if ! install_runtime_files >"$RUNTIME_INSTALL_LOG" 2>&1; then
      echo "ERR failed to install packaged runtime files" >&2
      tail -120 "$RUNTIME_INSTALL_LOG" >&2 || true
      exit 1
    fi

    (
      cd "$SOURCE_DIR"
      RUST_LOG="$RUST_LOG" "$BROOD_BIN" run --no-session-runner --dbus-name "$DBUS_DESTINATION" --config "$SOURCE_DIR/brood.toml"
    ) >"$SESSION_LOG" 2>&1 &
    SESSION_PID=$!

    wait_for_control "$DBUS_DESTINATION" "$SESSION_PID" "$SESSION_LOG" "Brood session"

    spawn_default_dev_client

    echo "OK Brood developer workflow ready"
    echo "source=$SOURCE_DIR"
    echo "run_dir=$RUN_DIR"
    echo "dbus_destination=$DBUS_DESTINATION"
    echo "runtime_install_log=$RUNTIME_INSTALL_LOG"
    echo "session_log=$SESSION_LOG"
    echo "Stop with Ctrl-C or close the Brood presentation."

    wait "$SESSION_PID"
  '';
}
