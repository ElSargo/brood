#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: software-e2e-capture.sh --out-dir DIR [--manifold-bin PATH] [--client-bin PATH]" >&2
}

OUT_DIR=""
MANIFOLD_BIN="${MANIFOLD_BIN:-manifold}"
CLIENT_BIN="${MANIFOLD_CLIENT_BIN:-}"
while (($# > 0)); do
  case "$1" in
    --out-dir)
      OUT_DIR="$2"
      shift 2
      ;;
    --manifold-bin)
      MANIFOLD_BIN="$2"
      shift 2
      ;;
    --client-bin)
      CLIENT_BIN="$2"
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
if [[ -z "$CLIENT_BIN" ]]; then
  CLIENT_BIN="$(dirname "$(realpath "$(command -v "$MANIFOLD_BIN")")")/manifold-client"
fi

for tool in "$MANIFOLD_BIN" "$CLIENT_BIN" dbus-daemon grim python3 sway weston-terminal; do
  if ! command -v "$tool" > /dev/null 2>&1; then
    echo "ERR missing required command: $tool" >&2
    exit 127
  fi
done

export XDG_CACHE_HOME="$OUT_DIR/cache"
export XDG_CONFIG_HOME="$OUT_DIR/config"
SERVER_RUNTIME_DIR="$OUT_DIR/server-runtime"
PRESENTATION_RUNTIME_DIR="$OUT_DIR/presentation-runtime"
mkdir -p \
  "$XDG_CACHE_HOME" \
  "$XDG_CONFIG_HOME" \
  "$SERVER_RUNTIME_DIR" \
  "$PRESENTATION_RUNTIME_DIR"
chmod 700 "$SERVER_RUNTIME_DIR" "$PRESENTATION_RUNTIME_DIR"

export LIBGL_ALWAYS_SOFTWARE=1
export GALLIUM_DRIVER=llvmpipe
export WGPU_BACKEND=gl
unset DISPLAY WAYLAND_DISPLAY manifold_session MANIFOLD_SURFACE_SESSION_ID
unset MANIFOLDD_DBUS_ADDRESS MANIFOLDD_DBUS_DESTINATION

dbus_args=(--session)
if [[ -n "${DBUS_SESSION_BUS_CONFIG:-}" ]]; then
  dbus_args=(--config-file "$DBUS_SESSION_BUS_CONFIG")
fi
dbus_info="$(dbus-daemon "${dbus_args[@]}" --fork --print-address=1 --print-pid=1)"
DBUS_SESSION_BUS_ADDRESS="$(sed -n '1p' <<< "$dbus_info")"
DBUS_SESSION_BUS_PID="$(sed -n '2p' <<< "$dbus_info")"
export DBUS_SESSION_BUS_ADDRESS

DESTINATION="org.manifold.SoftwareE2E.p$$"
SERVER_LOG="$OUT_DIR/server.log"
CLIENT_LOG="$OUT_DIR/client.log"
RECONNECTED_LOG="$OUT_DIR/client-reconnected.log"
COMPOSITOR_LOG="$OUT_DIR/compositor.log"
SWAY_CONFIG="$OUT_DIR/sway.conf"
EMPTY_CONFIG="$OUT_DIR/manifold.toml"
SERVER_PID=""
CLIENT_PID=""
COMPOSITOR_PID=""

cleanup() {
  local result=$?
  set +e
  for pid in "$CLIENT_PID" "$SERVER_PID" "$COMPOSITOR_PID"; do
    if [[ -n "$pid" ]] && kill -0 "$pid" 2> /dev/null; then
      kill "$pid" > /dev/null 2>&1
      wait "$pid" > /dev/null 2>&1
    fi
  done
  if [[ -n "$DBUS_SESSION_BUS_PID" ]]; then
    kill "$DBUS_SESSION_BUS_PID" > /dev/null 2>&1
  fi
  if ((result != 0)); then
    tail -n 160 "$CLIENT_LOG" >&2
    tail -n 120 "$SERVER_LOG" >&2
    tail -n 80 "$COMPOSITOR_LOG" >&2
  fi
  return "$result"
}
trap cleanup EXIT
printf '' > "$EMPTY_CONFIG"

env XDG_RUNTIME_DIR="$SERVER_RUNTIME_DIR" "$MANIFOLD_BIN" run \
  --no-session-runner \
  --no-default-modules \
  --no-init \
  --headless \
  --config "$EMPTY_CONFIG" \
  --dbus-destination "$DESTINATION" \
  --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
  > "$SERVER_LOG" 2>&1 &
SERVER_PID=$!

ctl() {
  "$MANIFOLD_BIN" ctl \
    --dbus-destination "$DESTINATION" \
    --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
    "$@"
}

for _ in $(seq 1 150); do
  ctl status > /dev/null 2>&1 && break
  kill -0 "$SERVER_PID" 2> /dev/null || exit 1
  sleep 0.1
done
ctl status > /dev/null

printf '%s\n' \
  'xwayland disable' \
  'output HEADLESS-1 mode 1280x720' \
  'seat seat0 fallback true' \
  'default_border none' \
  'default_floating_border none' \
  > "$SWAY_CONFIG"
env \
  XDG_RUNTIME_DIR="$PRESENTATION_RUNTIME_DIR" \
  WLR_BACKENDS=headless \
  WLR_HEADLESS_OUTPUTS=1 \
  WLR_LIBINPUT_NO_DEVICES=1 \
  sway --unsupported-gpu --config "$SWAY_CONFIG" \
  > "$COMPOSITOR_LOG" 2>&1 &
COMPOSITOR_PID=$!

WAYLAND_SOCKET=""
for _ in $(seq 1 100); do
  WAYLAND_SOCKET="$(
    find "$PRESENTATION_RUNTIME_DIR" -maxdepth 1 -type s -name 'wayland-*' -printf '%f\n' | head -1
  )"
  [[ -n "$WAYLAND_SOCKET" ]] && break
  kill -0 "$COMPOSITOR_PID" 2> /dev/null || exit 1
  sleep 0.05
done
[[ -n "$WAYLAND_SOCKET" ]]

start_client() {
  local log="$1"
  env \
    XDG_RUNTIME_DIR="$PRESENTATION_RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    WGPU_BACKEND=gl \
    "$CLIENT_BIN" \
    --no-init \
    --config "$EMPTY_CONFIG" \
    --dbus-destination "$DESTINATION" \
    --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
    > "$log" 2>&1 &
  CLIENT_PID=$!
}

start_client "$CLIENT_LOG"
for _ in $(seq 1 200); do
  rg -q "configured presentation surface" "$CLIENT_LOG" && break
  kill -0 "$CLIENT_PID" 2> /dev/null || exit 1
  sleep 0.1
done
rg -q "configured presentation surface" "$CLIENT_LOG"

ctl spawn weston-terminal > /dev/null
ctl spawn weston-terminal > /dev/null
for _ in $(seq 1 250); do
  STATUS="$(ctl status)"
  if [[ "$STATUS" =~ windows=2([[:space:]]|$) ]] \
    && [[ "$STATUS" =~ presentation=Attached ]] \
    && [[ "$(rg -c "imported window DMA-BUF" "$CLIENT_LOG" || true)" -ge 2 ]]; then
    break
  fi
  sleep 0.1
done
[[ "$STATUS" =~ windows=2([[:space:]]|$) ]]
[[ "$STATUS" =~ presentation=Attached ]]
[[ "$(rg -c "imported window DMA-BUF" "$CLIENT_LOG" || true)" -ge 2 ]]

env XDG_RUNTIME_DIR="$PRESENTATION_RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  grim "$OUT_DIR/frame.png"
mkdir -p "$OUT_DIR/server-dump"
ctl debug dump "$OUT_DIR/server-dump" state,surfaces > /dev/null

ctl close-frontend > /dev/null
for _ in $(seq 1 150); do
  STATUS="$(ctl status)"
  [[ "$STATUS" =~ presentation=Detached ]] && break
  sleep 0.1
done
[[ "$STATUS" =~ presentation=Detached ]]
[[ "$STATUS" =~ windows=2([[:space:]]|$) ]]
wait "$CLIENT_PID" || true
CLIENT_PID=""

start_client "$RECONNECTED_LOG"
for _ in $(seq 1 200); do
  STATUS="$(ctl status)"
  if [[ "$STATUS" =~ presentation=Attached ]] \
    && [[ "$(rg -c "imported window DMA-BUF" "$RECONNECTED_LOG" || true)" -ge 2 ]]; then
    break
  fi
  sleep 0.1
done
[[ "$STATUS" =~ presentation=Attached ]]
[[ "$(rg -c "imported window DMA-BUF" "$RECONNECTED_LOG" || true)" -ge 2 ]]
env XDG_RUNTIME_DIR="$PRESENTATION_RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  grim "$OUT_DIR/reconnected.png"

python3 - "$OUT_DIR/frame.png" "$OUT_DIR/reconnected.png" <<'PY'
import pathlib
import sys

for value in sys.argv[1:]:
    path = pathlib.Path(value)
    data = path.read_bytes()
    if not data.startswith(b"\x89PNG\r\n\x1a\n") or len(data) < 4096:
        raise SystemExit(f"invalid external-client capture: {path}")
PY

cat > "$OUT_DIR/summary.txt" <<EOF
software_e2e=passed
backend=headless-frame-exporter
presentation=external-client
managed_windows=2
disconnect_preserved_windows=true
reconnect_imported_windows=true
EOF

ctl quit > /dev/null 2>&1 || true
echo "OK software e2e capture"
echo "out_dir=$OUT_DIR"
