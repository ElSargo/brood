#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]]; then
  exec dbus-run-session -- "$0" "$@"
fi

for tool in cargo cc foot grim magick pkg-config python3 sway weston-simple-egl weston-simple-shm wtype; do
  if ! command -v "$tool" > /dev/null 2>&1; then
    echo "ERR $tool is required for the session-switcher proof" >&2
    exit 2
  fi
done

OUT="${BROOD_SESSION_SWITCHER_OUT:-$(mktemp -d /tmp/brood-session-switcher.XXXXXX)}"
PACKAGE="$OUT/package"
RUNTIME_DIR="$OUT/runtime"
COMPOSITOR_LOG="$OUT/compositor.log"
SERVER_LOG="$OUT/server.log"
CLIENT_LOG="$OUT/client.log"
CONFIG="$OUT/brood.toml"
SWAY_CONFIG="$OUT/sway.conf"
DBUS_DESTINATION="org.brood.SessionSwitcherTest.p$$"
mkdir -p "$PACKAGE" "$RUNTIME_DIR"
chmod 700 "$RUNTIME_DIR"

CLIENT_PID=""
SERVER_PID=""
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
  if ((result != 0)); then
    echo "client_log=$CLIENT_LOG" >&2
    tail -n 200 "$CLIENT_LOG" >&2
    echo "server_log=$SERVER_LOG" >&2
    tail -n 120 "$SERVER_LOG" >&2
    echo "compositor_log=$COMPOSITOR_LOG" >&2
    tail -n 100 "$COMPOSITOR_LOG" >&2
  fi
  return "$result"
}
trap 'echo "ERR command=$BASH_COMMAND line=$LINENO" >&2' ERR
trap cleanup EXIT

cargo build -p broodd -p brood-client --bins
cargo build \
  --manifest-path plugins/session-switcher/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release
read -r -a WAYLAND_BUILD_FLAGS <<< "$(pkg-config --cflags --libs wayland-client xkbcommon)"
cc \
  -std=c11 \
  -Wall \
  -Wextra \
  -Werror \
  tools/virtual-keyboard-key.c \
  -o "$OUT/virtual-keyboard-key" \
  "${WAYLAND_BUILD_FLAGS[@]}"
cc \
  -std=c11 \
  -Wall \
  -Wextra \
  -Werror \
  tools/virtual-pointer-input.c \
  -o "$OUT/virtual-pointer-input" \
  "${WAYLAND_BUILD_FLAGS[@]}"
cargo run --quiet -p brood-wasm --example componentize -- \
  plugins/session-switcher/target/wasm32-unknown-unknown/release/brood_session_switcher_plugin.wasm \
  "$PACKAGE/brood_session_switcher_plugin.component.wasm"
cargo build \
  --manifest-path plugins/picker/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release \
  --locked
cargo run --quiet -p brood-wasm --example componentize -- \
  plugins/picker/target/wasm32-unknown-unknown/release/brood_picker_plugin.wasm \
  "$PACKAGE/brood_picker_plugin.component.wasm"
cargo build \
  --manifest-path plugins/coding-agent/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release \
  --locked
cargo run --quiet -p brood-wasm --example componentize -- \
  plugins/coding-agent/target/wasm32-unknown-unknown/release/brood_coding_agent_plugin.wasm \
  "$PACKAGE/brood_coding_agent_plugin.component.wasm"
cargo build \
  --manifest-path plugins/diff-viewer/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release \
  --locked
cargo run --quiet -p brood-wasm --example componentize -- \
  plugins/diff-viewer/target/wasm32-unknown-unknown/release/brood_diff_viewer_plugin.wasm \
  "$PACKAGE/brood_diff_viewer_plugin.component.wasm"
cargo build \
  --manifest-path plugins/default-workspace/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release

printf '[init]\nclient = "%s"\n\n[client.key-policy]\nprovider = "workspace"\npath = "%s"\n' \
  "$PACKAGE/brood_session_switcher_plugin.component.wasm" \
  "$ROOT/plugins/default-workspace/target/wasm32-unknown-unknown/release/brood_default_workspace_plugin.wasm" \
  > "$CONFIG"

RUST_LOG="${RUST_LOG:-broodd=info,brood_client=info,smithay=warn,wgpu_core=warn,wgpu_hal=warn}"
env -u brood_session \
  RUST_LOG="$RUST_LOG" \
  target/debug/brood-sessiond \
  --headless \
  --no-init \
  --no-default-modules \
  --config "$CONFIG" \
  --dbus-destination "$DBUS_DESTINATION" \
  --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
  > "$SERVER_LOG" 2>&1 &
SERVER_PID=$!

ctl() {
  env -u brood_session target/debug/brood ctl \
    --dbus-destination "$DBUS_DESTINATION" \
    --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
    "$@"
}

for _ in $(seq 1 100); do
  ctl status > /dev/null 2>&1 && break
  kill -0 "$SERVER_PID" 2> /dev/null || {
    echo "ERR session server exited during startup" >&2
    exit 1
  }
  sleep 0.1
done
ctl status > /dev/null 2>&1 || {
  echo "ERR timed out waiting for the session server" >&2
  exit 1
}

ctl surface-session new Beta > /dev/null
ctl spawn weston-simple-egl > /dev/null
for _ in $(seq 1 100); do
  rg -q "total_windows=1" <<< "$(ctl status)" && break
  sleep 0.05
done
ctl surface-session new Gamma > /dev/null
ctl spawn weston-simple-shm > /dev/null
for _ in $(seq 1 100); do
  rg -q "total_windows=2" <<< "$(ctl status)" && break
  sleep 0.05
done
rg -q "total_windows=2" <<< "$(ctl status)"

printf '%s\n' \
  'xwayland disable' \
  'output HEADLESS-1 mode 1280x720' \
  'seat seat0 fallback true' \
  'default_border none' \
  'default_floating_border none' \
  > "$SWAY_CONFIG"

env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WLR_BACKENDS=headless \
  WLR_HEADLESS_OUTPUTS=1 \
  WLR_LIBINPUT_NO_DEVICES=1 \
  sway \
  --unsupported-gpu \
  --config "$SWAY_CONFIG" \
  > "$COMPOSITOR_LOG" 2>&1 &
COMPOSITOR_PID=$!

WAYLAND_SOCKET=""
for _ in $(seq 1 100); do
  WAYLAND_SOCKET="$(
    find "$RUNTIME_DIR" -maxdepth 1 -type s -name 'wayland-*' -printf '%f\n' | head -1
  )"
  [[ -n "$WAYLAND_SOCKET" ]] && break
  kill -0 "$COMPOSITOR_PID" 2> /dev/null || {
    echo "ERR headless Sway exited during startup" >&2
    exit 1
  }
  sleep 0.05
done
[[ -n "$WAYLAND_SOCKET" ]] || {
  echo "ERR timed out waiting for the Sway Wayland socket" >&2
  exit 1
}

env -u brood_session \
  XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  WGPU_BACKEND=vulkan \
  BROOD_DMABUF_HAPPY_PATH=1 \
  BROOD_TRACE_PLUGIN_INPUT=1 \
  BROOD_TRACE_PLUGIN_GRAPHICS=1 \
  BROOD_TRACE_PLUGIN_TEXT=1 \
  RUST_LOG="$RUST_LOG" \
  target/debug/brood-client \
  --config "$CONFIG" \
  --dbus-destination "$DBUS_DESTINATION" \
  --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
  > "$CLIENT_LOG" 2>&1 &
CLIENT_PID=$!

for _ in $(seq 1 200); do
  if rg -q "accepted plugin draw packet" "$CLIENT_LOG" \
    && rg -q "granted claimed root layout lease actor=session-switcher" "$CLIENT_LOG" \
    && rg -q "loaded presentation key policy provider=workspace" "$CLIENT_LOG"; then
    break
  fi
  kill -0 "$CLIENT_PID" 2> /dev/null || {
    echo "ERR presentation client exited before the switcher rendered" >&2
    exit 1
  }
  sleep 0.1
done
rg -q "accepted plugin draw packet" "$CLIENT_LOG"
rg -q "granted claimed root layout lease actor=session-switcher" "$CLIENT_LOG"
rg -q "loaded presentation key policy provider=workspace" "$CLIENT_LOG"

for _ in $(seq 1 200); do
  if rg -q "granted rest layout lease .* actor=workspace panes=2" "$CLIENT_LOG" \
    && rg -q "accepted leased actor layout .* panes=3" "$CLIENT_LOG" \
    && rg -q "presentation shared catalog .*active=3 .*sessions=2 windows=2" "$CLIENT_LOG" \
    && rg -q "imported window DMA-BUF window_id=1 " "$CLIENT_LOG" \
    && rg -q "imported window DMA-BUF window_id=2 " "$CLIENT_LOG" \
    && rg -q "requested managed window resize .* logical_size=[0-9]+x[0-9]+" "$CLIENT_LOG"; then
    break
  fi
  sleep 0.1
done
rg -q "granted rest layout lease .* actor=workspace panes=2" "$CLIENT_LOG"
rg -q "accepted leased actor layout .* panes=3" "$CLIENT_LOG"
rg -q "presentation shared catalog .*active=3 .*sessions=2 windows=2" "$CLIENT_LOG"
rg -q "imported window DMA-BUF window_id=1 " "$CLIENT_LOG"
rg -q "imported window DMA-BUF window_id=2 " "$CLIENT_LOG"
read -r SIDEBAR_WIDTH SIDEBAR_HEIGHT < <(
  sed -n 's/.*requested managed window resize .* logical_size=\([0-9][0-9]*\)x\([0-9][0-9]*\).*/\1 \2/p' \
    "$CLIENT_LOG" \
    | tail -1
)
[[ -n "$SIDEBAR_WIDTH" && -n "$SIDEBAR_HEIGHT" ]]

for _ in $(seq 1 100); do
  if rg -q "sent plugin pane resize .* logical_size=320x${SIDEBAR_HEIGHT}" "$CLIENT_LOG" \
    && rg -q "accepted plugin draw packet .* gpu_target=320x${SIDEBAR_HEIGHT}" "$CLIENT_LOG"; then
    break
  fi
  sleep 0.05
done
rg -q "sent plugin pane resize .* logical_size=320x${SIDEBAR_HEIGHT}" "$CLIENT_LOG"
rg -q "accepted plugin draw packet .* gpu_target=320x${SIDEBAR_HEIGHT}" "$CLIENT_LOG"

capture() {
  local target="$1"
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    grim "$target"
  [[ -s "$target" ]]
}

send_toggle() {
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-keyboard-key" 50
}

send_open_terminal() {
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-keyboard-key" --ctrl-only 24
}

send_picker() {
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-keyboard-key" 25
}

send_diff_toggle() {
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-keyboard-key" 32
}

send_pointer() {
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-pointer-input" "$@"
}

capture "$OUT/sidebar-visible.png"

accepted_before="$(rg -c "accepted leased actor layout .* panes=3" "$CLIENT_LOG")"
send_picker
for _ in $(seq 1 100); do
  if rg -q "started presentation WASM actor id=picker" "$CLIENT_LOG" \
    && rg -q "accepted leased actor layout .* panes=4" "$CLIENT_LOG" \
    && rg -q "sent plugin pane resize .* logical_size=720x[0-9]+" "$CLIENT_LOG"; then
    break
  fi
  sleep 0.05
done
rg -q "started presentation WASM actor id=picker" "$CLIENT_LOG"
rg -q "accepted leased actor layout .* panes=4" "$CLIENT_LOG"
rg -q "sent plugin pane resize .* logical_size=720x[0-9]+" "$CLIENT_LOG"
capture "$OUT/picker-visible.png"

env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  wtype "Bt" -s 100 -k Return
for _ in $(seq 1 100); do
  accepted_after="$(rg -c "accepted leased actor layout .* panes=3" "$CLIENT_LOG")"
  if rg -q "presentation shared catalog .*active=2 .*sessions=2 windows=2" "$CLIENT_LOG" \
    && ((accepted_after > accepted_before)); then
    break
  fi
  sleep 0.05
done
rg -q "presentation shared catalog .*active=2 .*sessions=2 windows=2" "$CLIENT_LOG"
((accepted_after > accepted_before))
capture "$OUT/session-beta.png"

gamma_snapshots_before="$(rg -c "presentation shared catalog .*active=3 .*sessions=2 windows=2" "$CLIENT_LOG")"
ctl surface-session switch Gamma > /dev/null
for _ in $(seq 1 100); do
  gamma_snapshots_after="$(rg -c "presentation shared catalog .*active=3 .*sessions=2 windows=2" "$CLIENT_LOG")"
  ((gamma_snapshots_after > gamma_snapshots_before)) && break
  sleep 0.05
done
((gamma_snapshots_after > gamma_snapshots_before))

managed_resizes="$(rg -c "requested managed window resize .* logical_size=[0-9]+x${SIDEBAR_HEIGHT}" "$CLIENT_LOG" || true)"
HIDDEN_WIDTH="$SIDEBAR_WIDTH"
for _ in 1 2; do
  send_toggle
  for _ in $(seq 1 40); do
    next_managed_resizes="$(rg -c "requested managed window resize .* logical_size=[0-9]+x${SIDEBAR_HEIGHT}" "$CLIENT_LOG" || true)"
    HIDDEN_WIDTH="$(
      sed -n 's/.*requested managed window resize .* logical_size=\([0-9][0-9]*\)x'"${SIDEBAR_HEIGHT}"'.*/\1/p' \
        "$CLIENT_LOG" \
        | tail -1
    )"
    if ((next_managed_resizes > managed_resizes)) \
      && ((HIDDEN_WIDTH > SIDEBAR_WIDTH)) \
      && rg -q "imported window DMA-BUF .* size=${HIDDEN_WIDTH}x${SIDEBAR_HEIGHT}" "$CLIENT_LOG"; then
      break 2
    fi
    sleep 0.05
  done
done
((next_managed_resizes > managed_resizes))
((HIDDEN_WIDTH > SIDEBAR_WIDTH))
rg -q "imported window DMA-BUF .* size=${HIDDEN_WIDTH}x${SIDEBAR_HEIGHT}" "$CLIENT_LOG"
capture "$OUT/sidebar-hidden.png"

sidebar_resizes="$(rg -c "requested managed window resize .* logical_size=${SIDEBAR_WIDTH}x${SIDEBAR_HEIGHT}" "$CLIENT_LOG" || true)"
sidebar_frames="$(rg -c "imported window DMA-BUF .* size=${SIDEBAR_WIDTH}x${SIDEBAR_HEIGHT}" "$CLIENT_LOG" || true)"
send_toggle
for _ in $(seq 1 100); do
  next_sidebar_resizes="$(rg -c "requested managed window resize .* logical_size=${SIDEBAR_WIDTH}x${SIDEBAR_HEIGHT}" "$CLIENT_LOG" || true)"
  next_sidebar_frames="$(rg -c "imported window DMA-BUF .* size=${SIDEBAR_WIDTH}x${SIDEBAR_HEIGHT}" "$CLIENT_LOG" || true)"
  if ((next_sidebar_resizes > sidebar_resizes)) \
    && ((next_sidebar_frames > sidebar_frames)); then
    break
  fi
  sleep 0.05
done
((next_sidebar_resizes > sidebar_resizes))
((next_sidebar_frames > sidebar_frames))
capture "$OUT/sidebar-restored.png"

CTRL_O_STARTED_MS="$(date +%s%3N)"
send_open_terminal &
CTRL_O_INPUT_PID=$!
for _ in $(seq 1 200); do
  if rg -q "total_windows=3" <<< "$(ctl status)" \
    && rg -q "presentation shared catalog .*active=3 .*sessions=2 windows=3" "$CLIENT_LOG" \
    && rg -q "accepted leased actor layout .* panes=4" "$CLIENT_LOG" \
    && rg -q "imported window DMA-BUF window_id=3 " "$CLIENT_LOG"; then
    break
  fi
  sleep 0.01
done
wait "$CTRL_O_INPUT_PID"
CTRL_O_LAYOUT_MS=$(( $(date +%s%3N) - CTRL_O_STARTED_MS ))
rg -q "total_windows=3" <<< "$(ctl status)"
rg -q "presentation shared catalog .*active=3 .*sessions=2 windows=3" "$CLIENT_LOG"
rg -q "accepted leased actor layout .* panes=4" "$CLIENT_LOG"
rg -q "imported window DMA-BUF window_id=3 " "$CLIENT_LOG"
capture "$OUT/ctrl-o-terminal.png"

CATALOG_TWO_BEFORE="$(rg -c "presentation shared catalog .*windows=2" "$CLIENT_LOG")"
LAYOUT_THREE_BEFORE="$(rg -c "accepted leased actor layout .* panes=3" "$CLIENT_LOG")"
CLOSE_STARTED_MS="$(date +%s%3N)"
ctl close-pane 3 > /dev/null
for _ in $(seq 1 200); do
  CATALOG_TWO_AFTER="$(rg -c "presentation shared catalog .*windows=2" "$CLIENT_LOG")"
  LAYOUT_THREE_AFTER="$(rg -c "accepted leased actor layout .* panes=3" "$CLIENT_LOG")"
  if rg -q "total_windows=2" <<< "$(ctl status)" \
    && ((CATALOG_TWO_AFTER > CATALOG_TWO_BEFORE)) \
    && ((LAYOUT_THREE_AFTER > LAYOUT_THREE_BEFORE)); then
    break
  fi
  sleep 0.01
done
CLOSE_LAYOUT_MS=$(( $(date +%s%3N) - CLOSE_STARTED_MS ))
rg -q "total_windows=2" <<< "$(ctl status)"
((CATALOG_TWO_AFTER > CATALOG_TWO_BEFORE))
((LAYOUT_THREE_AFTER > LAYOUT_THREE_BEFORE))

DIFF_PACKETS_BEFORE="$(rg -c "accepted plugin draw packet" "$CLIENT_LOG")"
send_diff_toggle
for _ in $(seq 1 200); do
  DIFF_PACKETS_AFTER="$(rg -c "accepted plugin draw packet" "$CLIENT_LOG")"
  if rg -q "started presentation WASM actor id=diff-viewer" "$CLIENT_LOG" \
    && rg -q "sent key-policy actor message actor=brood.client-init" "$CLIENT_LOG" \
    && ((DIFF_PACKETS_AFTER > DIFF_PACKETS_BEFORE)) \
    && rg -q "focused local plugin pane=" "$CLIENT_LOG"; then
    break
  fi
  sleep 0.05
done
rg -q "started presentation WASM actor id=diff-viewer" "$CLIENT_LOG"
rg -q "sent key-policy actor message actor=brood.client-init" "$CLIENT_LOG"
((DIFF_PACKETS_AFTER > DIFF_PACKETS_BEFORE))
DIFF_PANE="$(
  sed -n '/started presentation WASM actor id=diff-viewer/,$p' "$CLIENT_LOG" \
    | sed -n 's/.*focused local plugin pane=\([0-9][0-9]*\).*/\1/p' \
    | head -1
)"
[[ -n "$DIFF_PANE" ]]
INITIAL_DIFF_SEQUENCE="$(
  sed -n '/started presentation WASM actor id=diff-viewer/,$p' "$CLIENT_LOG" \
    | sed -n 's/.*accepted plugin draw packet pane='"$DIFF_PANE"' sequence=\([0-9][0-9]*\).*/\1/p' \
    | head -1
)"
[[ -n "$INITIAL_DIFF_SEQUENCE" ]]
for _ in $(seq 1 400); do
  POPULATED_DIFF_SEQUENCE="$(
    sed -n '/presentation actor worker finished actor=diff-viewer/,$p' "$CLIENT_LOG" \
      | sed -n 's/.*accepted plugin draw packet pane='"$DIFF_PANE"' sequence=\([0-9][0-9]*\) commands=\([2-9][0-9]\|[1-9][0-9][0-9][0-9]*\).*/\1/p' \
      | tail -1
  )"
  if [[ -n "$POPULATED_DIFF_SEQUENCE" ]] \
    && ((POPULATED_DIFF_SEQUENCE > INITIAL_DIFF_SEQUENCE)); then
    break
  fi
  sleep 0.05
done
((POPULATED_DIFF_SEQUENCE > INITIAL_DIFF_SEQUENCE))
capture "$OUT/diff-open.png"

read -r DIFF_WIDTH DIFF_HEIGHT < <(
  sed -n '/started presentation WASM actor id=diff-viewer/,$p' "$CLIENT_LOG" \
    | sed -n 's/.*sent plugin pane resize pane='"$DIFF_PANE"' logical_size=\([0-9][0-9]*\)x\([0-9][0-9]*\).*/\1 \2/p' \
    | tail -1
)
[[ -n "$DIFF_WIDTH" && -n "$DIFF_HEIGHT" ]]
((DIFF_WIDTH >= 480))
((DIFF_HEIGHT >= 320))

DRAG_LOG_LINE="$(wc -l < "$CLIENT_LOG")"
DRAG_PACKETS_BEFORE="$(
  rg -c "accepted plugin draw packet pane=${DIFF_PANE} " "$CLIENT_LOG"
)"
DRAG_X=$((DIFF_WIDTH - 8))
DRAG_START_Y=70
DRAG_END_Y=$((DIFF_HEIGHT * 2 / 3))
send_pointer drag "$DRAG_X" "$DRAG_START_Y" "$DRAG_END_Y" 12 1280 720
for _ in $(seq 1 100); do
  DRAG_PACKETS_AFTER="$(
    rg -c "accepted plugin draw packet pane=${DIFF_PANE} " "$CLIENT_LOG"
  )"
  ((DRAG_PACKETS_AFTER >= DRAG_PACKETS_BEFORE + 3)) && break
  sleep 0.05
done
DRAG_PACKET_COUNT=$((DRAG_PACKETS_AFTER - DRAG_PACKETS_BEFORE))
((DRAG_PACKET_COUNT >= 3))
DRAG_TEXT_LAYOUTS="$(
  tail -n "+$((DRAG_LOG_LINE + 1))" "$CLIENT_LOG" \
    | rg -c "plugin text layout pane=${DIFF_PANE} " \
    || true
)"
capture "$OUT/diff-dragged.png"

TRACKPAD_LOG_LINE="$(wc -l < "$CLIENT_LOG")"
TRACKPAD_PACKETS_BEFORE="$(
  rg -c "accepted plugin draw packet pane=${DIFF_PANE} " "$CLIENT_LOG"
)"
TRACKPAD_STARTED_MS="$(date +%s%3N)"
send_pointer scroll 220 220 0.5 40 1280 720
stable_samples=0
previous_packets="$TRACKPAD_PACKETS_BEFORE"
for _ in $(seq 1 200); do
  TRACKPAD_PACKETS_AFTER="$(
    rg -c "accepted plugin draw packet pane=${DIFF_PANE} " "$CLIENT_LOG"
  )"
  if ((TRACKPAD_PACKETS_AFTER > TRACKPAD_PACKETS_BEFORE)) \
    && ((TRACKPAD_PACKETS_AFTER == previous_packets)); then
    stable_samples=$((stable_samples + 1))
    ((stable_samples >= 5)) && break
  else
    stable_samples=0
  fi
  previous_packets="$TRACKPAD_PACKETS_AFTER"
  sleep 0.01
done
TRACKPAD_SETTLED_MS=$(( $(date +%s%3N) - TRACKPAD_STARTED_MS ))
TRACKPAD_PACKET_COUNT=$((TRACKPAD_PACKETS_AFTER - TRACKPAD_PACKETS_BEFORE))
((TRACKPAD_PACKET_COUNT >= 2))
TRACKPAD_TEXT_LAYOUTS="$(
  tail -n "+$((TRACKPAD_LOG_LINE + 1))" "$CLIENT_LOG" \
    | rg -c "plugin text layout pane=${DIFF_PANE} " \
    || true
)"
TRACKPAD_TEXT_LAYOUTS="${TRACKPAD_TEXT_LAYOUTS:-0}"
((TRACKPAD_TEXT_LAYOUTS < TRACKPAD_PACKET_COUNT * 40))
((TRACKPAD_SETTLED_MS < 2000))

CLOSED_LAYOUTS_BEFORE="$(rg -c "accepted leased actor layout .* panes=3" "$CLIENT_LOG")"
send_diff_toggle
for _ in $(seq 1 100); do
  CLOSED_LAYOUTS_AFTER="$(rg -c "accepted leased actor layout .* panes=3" "$CLIENT_LOG")"
  if rg -q "stopped presentation init child actor id=diff-viewer" "$CLIENT_LOG" \
    && ((CLOSED_LAYOUTS_AFTER > CLOSED_LAYOUTS_BEFORE)); then
    break
  fi
  sleep 0.05
done
rg -q "stopped presentation init child actor id=diff-viewer" "$CLIENT_LOG"
((CLOSED_LAYOUTS_AFTER > CLOSED_LAYOUTS_BEFORE))
capture "$OUT/diff-closed.png"

python3 - \
  "$OUT/sidebar-visible.png" \
  "$OUT/sidebar-hidden.png" \
  "$OUT/sidebar-restored.png" \
  "$OUT/session-beta.png" \
  "$OUT/ctrl-o-terminal.png" \
  "$OUT/picker-visible.png" \
  "$OUT/diff-open.png" \
  "$OUT/diff-dragged.png" \
  "$OUT/diff-closed.png" <<'PY'
import pathlib
import subprocess
import sys

for value in sys.argv[1:]:
    path = pathlib.Path(value)
    data = path.read_bytes()
    if not data.startswith(b"\x89PNG\r\n\x1a\n") or len(data) < 4096:
        raise SystemExit(f"invalid graphical proof: {path}")
    colors = int(subprocess.check_output(["magick", str(path), "-format", "%k", "info:"]))
    if colors < 50:
        raise SystemExit(f"graphical proof has too little rendered content: {path} colors={colors}")

metric = subprocess.run(
    ["magick", "compare", "-metric", "RMSE", sys.argv[1], sys.argv[2], "null:"],
    text=True,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    check=False,
).stderr
normalized = float(metric.rsplit("(", 1)[1].rstrip(")\n"))
if normalized < 0.02:
    raise SystemExit(f"visible and hidden sidebar captures are too similar: RMSE={normalized}")
print(f"sidebar_capture_rmse={normalized:.6f}")

metric = subprocess.run(
    ["magick", "compare", "-metric", "RMSE", sys.argv[1], sys.argv[4], "null:"],
    text=True,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    check=False,
).stderr
normalized = float(metric.rsplit("(", 1)[1].rstrip(")\n"))
if normalized < 0.01:
    raise SystemExit(f"Gamma and Beta session captures are too similar: RMSE={normalized}")
print(f"session_switch_capture_rmse={normalized:.6f}")

metric = subprocess.run(
    ["magick", "compare", "-metric", "RMSE", sys.argv[3], sys.argv[5], "null:"],
    text=True,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    check=False,
).stderr
normalized = float(metric.rsplit("(", 1)[1].rstrip(")\n"))
if normalized < 0.01:
    raise SystemExit(f"layout did not visibly change after Ctrl+O: RMSE={normalized}")
print(f"ctrl_o_capture_rmse={normalized:.6f}")

metric = subprocess.run(
    ["magick", "compare", "-metric", "RMSE", sys.argv[1], sys.argv[6], "null:"],
    text=True,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    check=False,
).stderr
normalized = float(metric.rsplit("(", 1)[1].rstrip(")\n"))
if normalized < 0.01:
    raise SystemExit(f"picker did not visibly float above the workspace: RMSE={normalized}")
print(f"picker_capture_rmse={normalized:.6f}")

metric = subprocess.run(
    ["magick", "compare", "-metric", "RMSE", sys.argv[7], sys.argv[8], "null:"],
    text=True,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    check=False,
).stderr
normalized = float(metric.rsplit("(", 1)[1].rstrip(")\n"))
if normalized < 0.002:
    raise SystemExit(f"dragging the scrollbar did not visibly scroll the project diff: RMSE={normalized}")
print(f"diff_drag_capture_rmse={normalized:.6f}")

metric = subprocess.run(
    ["magick", "compare", "-metric", "RMSE", sys.argv[7], sys.argv[9], "null:"],
    text=True,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    check=False,
).stderr
normalized = float(metric.rsplit("(", 1)[1].rstrip(")\n"))
if normalized < 0.01:
    raise SystemExit(f"second diff toggle did not visibly close the pane: RMSE={normalized}")
print(f"diff_close_capture_rmse={normalized:.6f}")
PY

STATUS="$(ctl status)"
rg -q "presentation=Attached" <<< "$STATUS"
rg -q "accepted leased actor layout .* panes=3" "$CLIENT_LOG"
rg -q "sent key-policy actor message actor=session-switcher" "$CLIENT_LOG"
if rg -q "rejected actor-submitted layout" "$CLIENT_LOG"; then
  echo "ERR client rejected a recursive layout update" >&2
  exit 1
fi
if rg -q "presentation backend snapshot" "$CLIENT_LOG"; then
  echo "ERR client used the retired Zbus catalog refresh path" >&2
  exit 1
fi
if sed -n '/presentation layout source changed source=actor/,$p' "$CLIENT_LOG" \
  | rg -q "presentation layout source changed source=native-fallback"; then
  echo "ERR client flashed through native fallback after accepting actor layout" >&2
  exit 1
fi

cat > "$OUT/proof.md" <<EOF
# Recursive session-switcher proof

- backend: headless frame exporter, no presenter and no backend layout
- input: real Ctrl+Alt+M, Ctrl+Alt+P, Ctrl+Alt+D, fuzzy text, Enter, pointer drag, touchpad scroll, and Ctrl+O events through the focused external Winit client
- recursive ownership: session-switcher root lease delegates its rest slot to workspace
- catalog: atomic shared-memory snapshots carried both sessions and all window identities
- switch: selecting Beta published a new layout revision and showed its already-mapped window
- picker: Ctrl+Alt+P opened the reusable actor as a 720px floating pane; Bt fuzzy-matched Beta and Enter returned the original session result
- transition: the previous actor layout remained visible until its replacement was accepted
- plugin sizing: pane-local resize and GPU canvas converged to 320x${SIDEBAR_HEIGHT}
- visible: managed pane resized to ${SIDEBAR_WIDTH}x${SIDEBAR_HEIGHT} beside the 320px right sidebar
- hidden: managed pane resized to ${HIDDEN_WIDTH}x${SIDEBAR_HEIGHT} through the same recursive layout fragment
- restored: managed pane returned to ${SIDEBAR_WIDTH}x${SIDEBAR_HEIGHT}
- Ctrl+O: spawned foot in Gamma, expanded the layout to four panes, and imported its DMA-BUF
- Ctrl+O latency: ${CTRL_O_LAYOUT_MS}ms from input helper start through the accepted four-pane layout
- close latency: ${CLOSE_LAYOUT_MS}ms through shared catalog withdrawal and the accepted two-pane layout
- diff viewer: Ctrl+Alt+D created and focused an ordinary split-diff pane; a real pointer press, motion burst, and release dragged its scrollbar; the second press stopped the actor and closed its pane
- diff drag rendering: ${DRAG_PACKET_COUNT} draw packets covered the pointer grab from the first thumb press through release
- diff touchpad rendering: 40 fractional-pixel finger-axis events settled in ${TRACKPAD_SETTLED_MS}ms; ${TRACKPAD_PACKET_COUNT} draw packets created ${TRACKPAD_TEXT_LAYOUTS} text layouts
- captures: sidebar-visible.png, picker-visible.png, session-beta.png, sidebar-hidden.png, sidebar-restored.png, ctrl-o-terminal.png, diff-open.png, diff-dragged.png, diff-closed.png
- status: $STATUS
EOF

ctl quit > /dev/null 2>&1 || true
echo "session_switcher_recursive=verified"
echo "proof=$OUT/proof.md"
echo "visible=$OUT/sidebar-visible.png"
echo "picker=$OUT/picker-visible.png"
echo "beta=$OUT/session-beta.png"
echo "hidden=$OUT/sidebar-hidden.png"
echo "restored=$OUT/sidebar-restored.png"
echo "ctrl_o=$OUT/ctrl-o-terminal.png"
echo "diff_open=$OUT/diff-open.png"
echo "diff_dragged=$OUT/diff-dragged.png"
echo "diff_closed=$OUT/diff-closed.png"
echo "diff_drag_packets=$DRAG_PACKET_COUNT"
echo "diff_drag_text_layouts=$DRAG_TEXT_LAYOUTS"
echo "diff_trackpad_settled_ms=$TRACKPAD_SETTLED_MS"
echo "diff_trackpad_packets=$TRACKPAD_PACKET_COUNT"
echo "diff_trackpad_text_layouts=$TRACKPAD_TEXT_LAYOUTS"
echo "ctrl_o_layout_ms=$CTRL_O_LAYOUT_MS"
