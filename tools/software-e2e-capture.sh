#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 << 'EOF'
usage: software-e2e-capture.sh --out-dir DIR [--manifold-bin PATH]

Runs an offscreen nested Manifold topology with Mesa software rendering, spawns
real Weston clients in the inner compositor, applies a split layout, and writes
debug dumps to DIR.
EOF
}

OUT_DIR=""
MANIFOLD_BIN="${MANIFOLD_BIN:-manifold}"

while (($# > 0)); do
  case "$1" in
    --out-dir)
      if (($# < 2)); then
        usage
        exit 2
      fi
      OUT_DIR="$2"
      shift 2
      ;;
    --manifold-bin)
      if (($# < 2)); then
        usage
        exit 2
      fi
      MANIFOLD_BIN="$2"
      shift 2
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      usage
      exit 2
      ;;
  esac
done

if [[ -z "$OUT_DIR" ]]; then
  OUT_DIR="$(mktemp -d "${TMPDIR:-/tmp}/manifold-software-e2e.XXXXXX")"
fi

mkdir -p "$OUT_DIR"
OUT_DIR="$(realpath "$OUT_DIR")"

require_command() {
  local name="$1"
  if ! command -v "$name" > /dev/null 2>&1; then
    echo "ERR missing required command: $name" >&2
    exit 127
  fi
}

require_command "$MANIFOLD_BIN"
require_command bash
require_command dbus-daemon
require_command python3
require_command weston-flower
require_command weston-simple-egl
require_command weston-simple-shm

export HOME="$OUT_DIR/home"
export XDG_CACHE_HOME="$OUT_DIR/cache"
export XDG_CONFIG_HOME="$OUT_DIR/config"
export XDG_RUNTIME_DIR="$OUT_DIR/xdg-runtime"
mkdir -p "$HOME" "$XDG_CACHE_HOME" "$XDG_CONFIG_HOME" "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

export LIBGL_ALWAYS_SOFTWARE=1
export GALLIUM_DRIVER=llvmpipe
export MESA_LOADER_DRIVER_OVERRIDE=llvmpipe
export MESA_NO_ERROR=1
export MANIFOLD_ALLOW_SOFTWARE_EGL_CONFIG=1
export WGPU_BACKEND=gl
unset WAYLAND_DISPLAY DISPLAY manifold_session MANIFOLD_SURFACE_SESSION_ID MANIFOLDD_DBUS_ADDRESS MANIFOLDD_DBUS_DESTINATION

dbus_args=(--session)
if [[ -n "${DBUS_SESSION_BUS_CONFIG:-}" ]]; then
  dbus_args=(--config-file "$DBUS_SESSION_BUS_CONFIG")
fi

dbus_info="$(dbus-daemon "${dbus_args[@]}" --fork --print-address=1 --print-pid=1)"
DBUS_SESSION_BUS_ADDRESS="$(printf '%s\n' "$dbus_info" | sed -n '1p')"
DBUS_SESSION_BUS_PID="$(printf '%s\n' "$dbus_info" | sed -n '2p')"
export DBUS_SESSION_BUS_ADDRESS

outer_dest="org.manifold.SoftwareE2E.p$$.r$RANDOM.outer"
inner_dest="org.manifold.SoftwareE2E.p$$.r$RANDOM.inner"
outer_log="$OUT_DIR/outer.log"
inner_log="$OUT_DIR/inner.log"
outer_dump="$OUT_DIR/outer-dump"
inner_dump="$OUT_DIR/inner-dump"
outer_pid=""

cleanup() {
  set +e
  if [[ -n "$inner_dest" ]]; then
    "$MANIFOLD_BIN" ctl --dbus-destination "$inner_dest" quit > /dev/null 2>&1
  fi
  if [[ -n "$outer_dest" ]]; then
    "$MANIFOLD_BIN" ctl --dbus-destination "$outer_dest" quit > /dev/null 2>&1
  fi
  if [[ -n "$outer_pid" ]]; then
    for _ in 1 2 3 4 5; do
      if ! kill -0 "$outer_pid" > /dev/null 2>&1; then
        break
      fi
      sleep 0.1
    done
    if kill -0 "$outer_pid" > /dev/null 2>&1; then
      kill "$outer_pid" > /dev/null 2>&1
    fi
    wait "$outer_pid" > /dev/null 2>&1
  fi
  if [[ -n "${DBUS_SESSION_BUS_PID:-}" ]]; then
    kill "$DBUS_SESSION_BUS_PID" > /dev/null 2>&1
  fi
}
trap cleanup EXIT

ctl() {
  local destination="$1"
  shift
  "$MANIFOLD_BIN" ctl --dbus-destination "$destination" "$@"
}

status_json() {
  ctl "$1" status-json
}

wait_status() {
  local destination="$1"
  local label="$2"
  local expression="$3"
  local log_path="$4"
  local timeout_seconds="${5:-20}"
  local deadline=$((SECONDS + timeout_seconds))
  local status=""

  while ((SECONDS <= deadline)); do
    if [[ -n "$outer_pid" ]] && ! kill -0 "$outer_pid" > /dev/null 2>&1; then
      echo "ERR outer process exited while waiting for $label" >&2
      tail -n 120 "$outer_log" >&2 || true
      exit 1
    fi
    if status="$(status_json "$destination" 2> /dev/null)" &&
      python3 - "$expression" "$status" << 'PY'; then
import json
import sys

expr = sys.argv[1]
data = json.loads(sys.argv[2])
sys.exit(0 if eval(expr, {"__builtins__": {}}, {"data": data}) else 1)
PY
      printf '%s\n' "$status" > "$log_path"
      return 0
    fi
    sleep 0.1
  done

  echo "ERR timed out waiting for $label" >&2
  if [[ -n "$status" ]]; then
    printf 'last_status=%s\n' "$status" >&2
  fi
  tail -n 120 "$outer_log" >&2 || true
  tail -n 120 "$inner_log" >&2 || true
  exit 1
}

"$MANIFOLD_BIN" run \
  --no-session-runner \
  --no-default-modules \
  --dbus-destination "$outer_dest" \
  --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
  --offscreen-presentation \
  --test-surface-markers \
  --nested-inner-dbus-destination "$inner_dest" \
  > "$outer_log" 2>&1 &
outer_pid="$!"

wait_status \
  "$outer_dest" \
  "outer offscreen presentation" \
  'data.get("presentation") == "Attached" and data.get("presentation_target") == "offscreen"' \
  "$OUT_DIR/outer-ready.json"

inner_command=$(
  printf 'exec %q run --no-session-runner --no-default-modules --dbus-destination %q --dbus-address %q --test-surface-markers >%q 2>&1' \
    "$MANIFOLD_BIN" \
    "$inner_dest" \
    "$DBUS_SESSION_BUS_ADDRESS" \
    "$inner_log"
)
ctl "$outer_dest" spawn bash -lc "$inner_command" > /dev/null

wait_status \
  "$inner_dest" \
  "inner attached presentation" \
  'data.get("presentation") == "Attached" and data.get("presentation_target") == "winit"' \
  "$OUT_DIR/inner-ready.json"
wait_status \
  "$outer_dest" \
  "outer hosting inner presentation" \
  'data.get("windows", 0) >= 1 and data.get("frames", 0) >= 1' \
  "$OUT_DIR/outer-hosting-inner.json"

ctl "$inner_dest" spawn weston-simple-egl > /dev/null
wait_status \
  "$inner_dest" \
  "first GLES client" \
  'data.get("windows", 0) >= 1 and data.get("frames", 0) >= 2' \
  "$OUT_DIR/inner-client-1.json"

ctl "$inner_dest" layout split-right > /dev/null
ctl "$inner_dest" spawn weston-simple-shm > /dev/null
wait_status \
  "$inner_dest" \
  "second SHM client" \
  'data.get("windows", 0) >= 2 and data.get("resolved_layout_panes", 0) >= 2' \
  "$OUT_DIR/inner-client-2.json"

ctl "$inner_dest" layout split-vertical > /dev/null
ctl "$inner_dest" spawn weston-flower > /dev/null
wait_status \
  "$inner_dest" \
  "third real client in split layout" \
  'data.get("windows", 0) >= 3 and data.get("resolved_layout_panes", 0) >= 3 and data.get("resolved_layout_items", 0) >= 3 and data.get("frames", 0) >= 4' \
  "$OUT_DIR/inner-client-3.json"

mkdir -p "$inner_dump" "$outer_dump"
ctl "$inner_dest" debug dump "$inner_dump" state,composed > /dev/null
ctl "$outer_dest" debug dump "$outer_dump" state,composed > /dev/null

python3 - "$inner_dump/state.json" "$outer_dump/state.json" "$inner_dump/composed.png" "$outer_dump/composed.png" << 'PY'
import json
import pathlib
import sys

inner_state = json.loads(pathlib.Path(sys.argv[1]).read_text())
outer_state = json.loads(pathlib.Path(sys.argv[2]).read_text())
inner_png = pathlib.Path(sys.argv[3])
outer_png = pathlib.Path(sys.argv[4])

def require(condition, message):
    if not condition:
        raise SystemExit(message)

inner_status = inner_state["status"]
outer_status = outer_state["status"]
require(inner_state["test_surface_markers"] is True, "inner markers were not enabled")
require(outer_state["test_surface_markers"] is True, "outer markers were not enabled")
require(inner_status["windows"] >= 3, f"expected at least 3 inner windows: {inner_status}")
require(inner_status["resolved_layout_panes"] >= 3, f"expected 3 inner layout panes: {inner_status}")
require(outer_status["windows"] >= 1, f"expected outer to host inner presentation: {outer_status}")
require(len(inner_state["windows"]) >= 3, f"inner dump missing window entries: {inner_state}")
require(all(window.get("marker_id") is not None for window in inner_state["windows"]), inner_state["windows"])
for path in (inner_png, outer_png):
    require(path.exists(), f"missing capture {path}")
    data = path.read_bytes()
    require(data.startswith(b"\x89PNG\r\n\x1a\n"), f"capture is not a PNG: {path}")
    require(len(data) > 4096, f"capture is unexpectedly small: {path}")
PY

cat > "$OUT_DIR/summary.txt" << EOF
software_e2e=passed
outer_destination=$outer_dest
inner_destination=$inner_dest
inner_dump=$inner_dump
outer_dump=$outer_dump
EOF

echo "OK software e2e capture"
echo "out_dir=$OUT_DIR"
echo "inner_dump=$inner_dump"
echo "outer_dump=$outer_dump"
