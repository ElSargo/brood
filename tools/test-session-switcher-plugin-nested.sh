#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]]; then
  exec dbus-run-session -- "$0" "$@"
fi

for tool in cargo cc foot grim magick pkg-config python3 sway weston-simple-egl weston-simple-shm; do
  if ! command -v "$tool" > /dev/null 2>&1; then
    echo "ERR $tool is required for the session-switcher proof" >&2
    exit 2
  fi
done

OUT="${MANIFOLD_SESSION_SWITCHER_OUT:-$(mktemp -d /tmp/manifold-session-switcher.XXXXXX)}"
PACKAGE="$OUT/package"
RUNTIME_DIR="$OUT/runtime"
COMPOSITOR_LOG="$OUT/compositor.log"
SERVER_LOG="$OUT/server.log"
CLIENT_LOG="$OUT/client.log"
CONFIG="$OUT/manifold.toml"
SWAY_CONFIG="$OUT/sway.conf"
DBUS_DESTINATION="org.manifold.SessionSwitcherTest.p$$"
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
trap cleanup EXIT

cargo build -p manifoldd -p manifold-client --bins
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
cargo run --quiet -p manifold-wasm --example componentize -- \
  plugins/session-switcher/target/wasm32-unknown-unknown/release/manifold_session_switcher_plugin.wasm \
  "$PACKAGE/manifold_session_switcher_plugin.component.wasm"
cargo build \
  --manifest-path plugins/default-workspace/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release

printf '[init]\nclient = "%s"\n\n[client.key-policy]\nprovider = "workspace"\npath = "%s"\n' \
  "$PACKAGE/manifold_session_switcher_plugin.component.wasm" \
  "$ROOT/plugins/default-workspace/target/wasm32-unknown-unknown/release/manifold_default_workspace_plugin.wasm" \
  > "$CONFIG"

RUST_LOG="${RUST_LOG:-manifoldd=info,manifold_client=info,smithay=warn,wgpu_core=warn,wgpu_hal=warn}"
env -u manifold_session \
  RUST_LOG="$RUST_LOG" \
  target/debug/manifold-sessiond \
  --headless \
  --no-init \
  --no-default-modules \
  --config "$CONFIG" \
  --dbus-destination "$DBUS_DESTINATION" \
  --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
  > "$SERVER_LOG" 2>&1 &
SERVER_PID=$!

ctl() {
  env -u manifold_session target/debug/manifold ctl \
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

env -u manifold_session \
  XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  WGPU_BACKEND=vulkan \
  MANIFOLD_DMABUF_HAPPY_PATH=1 \
  RUST_LOG="$RUST_LOG" \
  target/debug/manifold-client \
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
    && rg -q "accepted leased actor layout .* panes=2" "$CLIENT_LOG" \
    && rg -q "presentation shared catalog .*active=3 .*sessions=2 windows=2" "$CLIENT_LOG" \
    && rg -q "imported window DMA-BUF window_id=1 " "$CLIENT_LOG" \
    && rg -q "imported window DMA-BUF window_id=2 " "$CLIENT_LOG" \
    && rg -q "requested managed window resize .* logical_size=[0-9]+x[0-9]+" "$CLIENT_LOG"; then
    break
  fi
  sleep 0.1
done
rg -q "granted rest layout lease .* actor=workspace panes=2" "$CLIENT_LOG"
rg -q "accepted leased actor layout .* panes=2" "$CLIENT_LOG"
rg -q "presentation shared catalog .*active=3 .*sessions=2 windows=2" "$CLIENT_LOG"
rg -q "imported window DMA-BUF window_id=1 " "$CLIENT_LOG"
rg -q "imported window DMA-BUF window_id=2 " "$CLIENT_LOG"
read -r SIDEBAR_WIDTH SIDEBAR_HEIGHT < <(
  sed -n 's/.*requested managed window resize .* logical_size=\([0-9][0-9]*\)x\([0-9][0-9]*\).*/\1 \2/p' \
    "$CLIENT_LOG" \
    | tail -1
)
[[ -n "$SIDEBAR_WIDTH" && -n "$SIDEBAR_HEIGHT" ]]
FULL_WIDTH=$((SIDEBAR_WIDTH + 320))

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

capture "$OUT/sidebar-visible.png"

accepted_before="$(rg -c "accepted leased actor layout .* panes=2" "$CLIENT_LOG")"
ctl surface-session switch Beta > /dev/null
for _ in $(seq 1 100); do
  accepted_after="$(rg -c "accepted leased actor layout .* panes=2" "$CLIENT_LOG")"
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

full_width_resizes="$(rg -c "requested managed window resize .* logical_size=${FULL_WIDTH}x${SIDEBAR_HEIGHT}" "$CLIENT_LOG" || true)"
full_width_frames="$(rg -c "imported window DMA-BUF .* size=${FULL_WIDTH}x${SIDEBAR_HEIGHT}" "$CLIENT_LOG" || true)"
next_full_width_resizes="$full_width_resizes"
next_full_width_frames="$full_width_frames"
for _ in 1 2; do
  send_toggle
  for _ in $(seq 1 40); do
    next_full_width_resizes="$(rg -c "requested managed window resize .* logical_size=${FULL_WIDTH}x${SIDEBAR_HEIGHT}" "$CLIENT_LOG" || true)"
    next_full_width_frames="$(rg -c "imported window DMA-BUF .* size=${FULL_WIDTH}x${SIDEBAR_HEIGHT}" "$CLIENT_LOG" || true)"
    if ((next_full_width_resizes > full_width_resizes)) \
      && ((next_full_width_frames > full_width_frames)); then
      break 2
    fi
    sleep 0.05
  done
done
((next_full_width_resizes > full_width_resizes))
((next_full_width_frames > full_width_frames))
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
    && rg -q "accepted leased actor layout .* panes=3" "$CLIENT_LOG" \
    && rg -q "imported window DMA-BUF window_id=3 " "$CLIENT_LOG"; then
    break
  fi
  sleep 0.01
done
wait "$CTRL_O_INPUT_PID"
CTRL_O_LAYOUT_MS=$(( $(date +%s%3N) - CTRL_O_STARTED_MS ))
rg -q "total_windows=3" <<< "$(ctl status)"
rg -q "presentation shared catalog .*active=3 .*sessions=2 windows=3" "$CLIENT_LOG"
rg -q "accepted leased actor layout .* panes=3" "$CLIENT_LOG"
rg -q "imported window DMA-BUF window_id=3 " "$CLIENT_LOG"
((CTRL_O_LAYOUT_MS < 150))
capture "$OUT/ctrl-o-terminal.png"

CATALOG_TWO_BEFORE="$(rg -c "presentation shared catalog .*windows=2" "$CLIENT_LOG")"
LAYOUT_TWO_BEFORE="$(rg -c "accepted leased actor layout .* panes=2" "$CLIENT_LOG")"
CLOSE_STARTED_MS="$(date +%s%3N)"
ctl close-pane 3 > /dev/null
for _ in $(seq 1 200); do
  CATALOG_TWO_AFTER="$(rg -c "presentation shared catalog .*windows=2" "$CLIENT_LOG")"
  LAYOUT_TWO_AFTER="$(rg -c "accepted leased actor layout .* panes=2" "$CLIENT_LOG")"
  if rg -q "total_windows=2" <<< "$(ctl status)" \
    && ((CATALOG_TWO_AFTER > CATALOG_TWO_BEFORE)) \
    && ((LAYOUT_TWO_AFTER > LAYOUT_TWO_BEFORE)); then
    break
  fi
  sleep 0.01
done
CLOSE_LAYOUT_MS=$(( $(date +%s%3N) - CLOSE_STARTED_MS ))
rg -q "total_windows=2" <<< "$(ctl status)"
((CATALOG_TWO_AFTER > CATALOG_TWO_BEFORE))
((LAYOUT_TWO_AFTER > LAYOUT_TWO_BEFORE))
((CLOSE_LAYOUT_MS < 150))

python3 - \
  "$OUT/sidebar-visible.png" \
  "$OUT/sidebar-hidden.png" \
  "$OUT/sidebar-restored.png" \
  "$OUT/session-beta.png" \
  "$OUT/ctrl-o-terminal.png" <<'PY'
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
PY

STATUS="$(ctl status)"
rg -q "presentation=Attached" <<< "$STATUS"
rg -q "accepted leased actor layout .* panes=1" "$CLIENT_LOG"
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
- input: real Ctrl+Alt+M and Ctrl+O events through the focused external Winit client
- recursive ownership: session-switcher root lease delegates its rest slot to workspace
- catalog: atomic shared-memory snapshots carried both sessions and all window identities
- switch: selecting Beta published a new layout revision and showed its already-mapped window
- transition: the previous actor layout remained visible until its replacement was accepted
- plugin sizing: pane-local resize and GPU canvas converged to 320x${SIDEBAR_HEIGHT}
- visible: managed pane resized to ${SIDEBAR_WIDTH}x${SIDEBAR_HEIGHT} beside the 320px right sidebar
- hidden: managed pane resized to ${FULL_WIDTH}x${SIDEBAR_HEIGHT} through the same recursive layout fragment
- restored: managed pane returned to ${SIDEBAR_WIDTH}x${SIDEBAR_HEIGHT}
- Ctrl+O: spawned foot in Gamma, expanded the layout to three panes, and imported its DMA-BUF
- Ctrl+O latency: ${CTRL_O_LAYOUT_MS}ms from input helper start through the accepted three-pane layout
- close latency: ${CLOSE_LAYOUT_MS}ms through shared catalog withdrawal and the accepted two-pane layout
- captures: sidebar-visible.png, session-beta.png, sidebar-hidden.png, sidebar-restored.png, ctrl-o-terminal.png
- status: $STATUS
EOF

ctl quit > /dev/null 2>&1 || true
echo "session_switcher_recursive=verified"
echo "proof=$OUT/proof.md"
echo "visible=$OUT/sidebar-visible.png"
echo "beta=$OUT/session-beta.png"
echo "hidden=$OUT/sidebar-hidden.png"
echo "restored=$OUT/sidebar-restored.png"
echo "ctrl_o=$OUT/ctrl-o-terminal.png"
echo "ctrl_o_layout_ms=$CTRL_O_LAYOUT_MS"
