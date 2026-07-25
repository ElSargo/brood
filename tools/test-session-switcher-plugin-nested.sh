#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]]; then
  exec dbus-run-session -- "$0" "$@"
fi

for tool in cargo cc foot git grim magick pkg-config python3 sway wayland-scanner weston-simple-egl weston-simple-shm wtype; do
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
PROJECT="$OUT/project"
OTHER_PROJECT="$OUT/other-project"
DATA_HOME="$OUT/data"
DBUS_DESTINATION="org.brood.SessionSwitcherTest.p$$"
mkdir -p "$PACKAGE" "$RUNTIME_DIR" "$PROJECT" "$DATA_HOME"
chmod 700 "$RUNTIME_DIR"
git -C "$PROJECT" init --quiet
git -C "$PROJECT" -c user.name=Brood -c user.email=brood@example.invalid \
  commit --quiet --allow-empty -m initial
git -C "$PROJECT" branch picker-worktree

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

cargo build -p broodd -p brood-client --bins --locked
cargo build \
  --manifest-path plugins/session-switcher/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release \
  --locked
read -r -a WAYLAND_BUILD_FLAGS <<< "$(pkg-config --cflags --libs wayland-client xkbcommon)"
cc \
  -std=c11 \
  -Wall \
  -Wextra \
  -Werror \
  tools/virtual-keyboard-key.c \
  -o "$OUT/virtual-keyboard-key" \
  "${WAYLAND_BUILD_FLAGS[@]}"
VIRTUAL_POINTER_XML="$(
  pkg-config --variable=pkgdatadir wlr-protocols
)/unstable/wlr-virtual-pointer-unstable-v1.xml"
[[ -n "$VIRTUAL_POINTER_XML" ]] || {
  echo "ERR wlr-virtual-pointer protocol XML is required" >&2
  exit 2
}
wayland-scanner client-header \
  "$VIRTUAL_POINTER_XML" \
  "$OUT/wlr-virtual-pointer-unstable-v1-client-protocol.h"
wayland-scanner private-code \
  "$VIRTUAL_POINTER_XML" \
  "$OUT/wlr-virtual-pointer-unstable-v1-protocol.c"
cc \
  -std=c11 \
  -Wall \
  -Wextra \
  -Werror \
  -I "$OUT" \
  tools/virtual-pointer-click.c \
  "$OUT/wlr-virtual-pointer-unstable-v1-protocol.c" \
  -o "$OUT/virtual-pointer-click" \
  "${WAYLAND_BUILD_FLAGS[@]}"
cargo run --quiet -p brood-wasm --example componentize -- \
  plugins/session-switcher/target/wasm32-unknown-unknown/release/brood_session_switcher_plugin.wasm \
  "$PACKAGE/brood_session_switcher_plugin.component.wasm"
cargo build \
  --manifest-path plugins/worktree-manager/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release \
  --locked
cargo run --quiet -p brood-wasm --example componentize -- \
  plugins/worktree-manager/target/wasm32-unknown-unknown/release/brood_worktree_manager_plugin.wasm \
  "$PACKAGE/brood_worktree_manager_plugin.component.wasm"
cargo build \
  --manifest-path plugins/coding-agent/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release \
  --locked
cargo run --quiet -p brood-wasm --example componentize -- \
  plugins/coding-agent/target/wasm32-unknown-unknown/release/brood_coding_agent_plugin.wasm \
  "$PACKAGE/brood_coding_agent_plugin.component.wasm"
cargo build \
  --manifest-path plugins/default-workspace/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release \
  --locked

printf '[init]\nbackend = "%s"\nclient = "%s"\n\n[client.key-policy]\nprovider = "workspace"\npath = "%s"\n' \
  "$PACKAGE/brood_worktree_manager_plugin.component.wasm" \
  "$PACKAGE/brood_session_switcher_plugin.component.wasm" \
  "$ROOT/plugins/default-workspace/target/wasm32-unknown-unknown/release/brood_default_workspace_plugin.wasm" \
  > "$CONFIG"

RUST_LOG="${RUST_LOG:-broodd=info,brood_client=info,smithay=warn,wgpu_core=warn,wgpu_hal=warn}"
(
  cd "$PROJECT"
  env -u brood_session \
    XDG_DATA_HOME="$DATA_HOME" \
    RUST_LOG="$RUST_LOG" \
    "$ROOT/target/debug/brood-sessiond" \
    --headless \
    --no-default-modules \
    --config "$CONFIG" \
    --dbus-destination "$DBUS_DESTINATION" \
    --dbus-address "$DBUS_SESSION_BUS_ADDRESS"
) > "$SERVER_LOG" 2>&1 &
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
  XDG_DATA_HOME="$DATA_HOME" \
  XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  WGPU_BACKEND=vulkan \
  BROOD_DMABUF_HAPPY_PATH=1 \
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

click_at() {
  local x="$1"
  local y="$2"
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-pointer-click" "$x" "$y" 1280 720
}

submit_text() {
  local value="$1"
  local primer="bcdefghijklmnopqrstuvwxyz0123"
  local -a erase_primer=()
  for _ in $(seq 1 "${#primer}"); do
    erase_primer+=(-k BackSpace)
  done
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    wtype "$primer" "${erase_primer[@]}" -s 100 "$value" -s 100 -k Return
}

capture "$OUT/sidebar-visible.png"

accepted_before="$(rg -c "accepted leased actor layout .* panes=3" "$CLIENT_LOG")"
ctl surface-session switch Beta > /dev/null
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
((CTRL_O_LAYOUT_MS < 150))
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
((CLOSE_LAYOUT_MS < 150))

capture "$OUT/project-expanded.png"
click_at 980 123
sleep 0.2
capture "$OUT/project-collapsed.png"

click_at 980 123
sleep 0.2

click_at 1260 123
submit_text picker-worktree
for _ in $(seq 1 200); do
  SESSION_JSON="$(ctl surface-session list-json)"
  if rg -q '"name":"picker-worktree"' <<< "$SESSION_JSON" \
    && rg -q "operation=CreateWorktree success=true" "$CLIENT_LOG"; then
    break
  fi
  sleep 0.05
done
rg -q '"name":"picker-worktree"' <<< "$SESSION_JSON"
rg -q "operation=CreateWorktree success=true" "$CLIENT_LOG"
read -r WORKTREE_SESSION_ID WORKTREE_DIRECTORY < <(
  python3 - "$SESSION_JSON" "$PROJECT" "$DATA_HOME" <<'PY'
import json
import pathlib
import sys

catalog = json.loads(sys.argv[1])
project = pathlib.Path(sys.argv[2]).resolve()
data_home = pathlib.Path(sys.argv[3]).resolve()
source = next(session for session in catalog["sessions"] if session["name"] == "Beta")
created = next(
    session for session in catalog["sessions"] if session["name"] == "picker-worktree"
)
assert source["project_id"] == created["project_id"]
assert source["project_name"] == project.name
assert created["branch"] == "picker-worktree"
assert created["worktree"] is True
directory = pathlib.Path(created["working_directory"]).resolve()
directory.relative_to(data_home / "brood" / "worktrees")
print(created["id"], directory)
PY
)
[[ -d "$WORKTREE_DIRECTORY" ]]
[[ "$(git -C "$WORKTREE_DIRECTORY" branch --show-current)" == "picker-worktree" ]]
WORKTREE_PARENT="${WORKTREE_DIRECTORY%/*}"
WORKTREE_NAME="${WORKTREE_DIRECTORY##*/}"
WORKTREE_METADATA="$WORKTREE_PARENT/$WORKTREE_NAME.brood-worktree.json"
for _ in $(seq 1 100); do
  if python3 - "$WORKTREE_METADATA" "$WORKTREE_SESSION_ID" <<'PY'
import json
import pathlib
import sys

metadata = json.loads(pathlib.Path(sys.argv[1]).read_text())
raise SystemExit(0 if metadata["session"] == int(sys.argv[2]) else 1)
PY
  then
    break
  fi
  sleep 0.05
done
python3 - "$WORKTREE_METADATA" "$WORKTREE_SESSION_ID" <<'PY'
import json
import pathlib
import sys

metadata = json.loads(pathlib.Path(sys.argv[1]).read_text())
assert metadata["session"] == int(sys.argv[2])
assert metadata["branch"] == "picker-worktree"
assert pathlib.Path(metadata["path"]).resolve().parent == pathlib.Path(sys.argv[1]).parent.resolve()
PY
rg -q "actor=brood.backend-init" "$SERVER_LOG"
rg -q "attached managed worktree session" "$SERVER_LOG"
capture "$OUT/worktree-created.png"

WORKTREE_COMPLETIONS_BEFORE="$(
  rg -c "operation=CreateWorktree success=true" "$CLIENT_LOG"
)"
click_at 1260 123
submit_text picker-worktree
for _ in $(seq 1 100); do
  WORKTREE_COMPLETIONS_AFTER="$(
    rg -c "operation=CreateWorktree success=true" "$CLIENT_LOG"
  )"
  SESSION_JSON="$(ctl surface-session list-json)"
  MATCHING_WORKTREE_SESSIONS="$(
    python3 -c 'import json,sys; print(sum(s["name"] == "picker-worktree" for s in json.load(sys.stdin)["sessions"]))' \
      <<< "$SESSION_JSON"
  )"
  if ((WORKTREE_COMPLETIONS_AFTER > WORKTREE_COMPLETIONS_BEFORE)) \
    && [[ "$MATCHING_WORKTREE_SESSIONS" == "1" ]]; then
    break
  fi
  sleep 0.05
done
((WORKTREE_COMPLETIONS_AFTER > WORKTREE_COMPLETIONS_BEFORE))
[[ "$MATCHING_WORKTREE_SESSIONS" == "1" ]]

ctl spawn weston-simple-shm > /dev/null
for _ in $(seq 1 100); do
  rg -q "total_windows=3" <<< "$(ctl status)" && break
  sleep 0.05
done
rg -q "total_windows=3" <<< "$(ctl status)"

mkdir -p "$OTHER_PROJECT"
git -C "$OTHER_PROJECT" init --quiet
git -C "$OTHER_PROJECT" -c user.name=Brood -c user.email=brood@example.invalid \
  commit --quiet --allow-empty -m initial
ctl surface-session dir-new "$OTHER_PROJECT" default Other > /dev/null
ctl spawn weston-simple-shm > /dev/null
for _ in $(seq 1 100); do
  SESSION_JSON="$(ctl surface-session list-json)"
  if rg -q "total_windows=4" <<< "$(ctl status)" \
    && [[ "$(python3 -c 'import json,sys; print(len({s["project_id"] for s in json.load(sys.stdin)["sessions"]}))' <<< "$SESSION_JSON")" == "2" ]]; then
    break
  fi
  sleep 0.05
done
rg -q "total_windows=4" <<< "$(ctl status)"
[[ "$(python3 -c 'import json,sys; print(len({s["project_id"] for s in json.load(sys.stdin)["sessions"]}))' <<< "$SESSION_JSON")" == "2" ]]
capture "$OUT/projects.png"

python3 - \
  "$OUT/sidebar-visible.png" \
  "$OUT/sidebar-hidden.png" \
  "$OUT/sidebar-restored.png" \
  "$OUT/session-beta.png" \
  "$OUT/ctrl-o-terminal.png" \
  "$OUT/project-expanded.png" \
  "$OUT/project-collapsed.png" \
  "$OUT/worktree-created.png" \
  "$OUT/projects.png" <<'PY'
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
    ["magick", "compare", "-metric", "RMSE", sys.argv[6], sys.argv[7], "null:"],
    text=True,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    check=False,
).stderr
normalized = float(metric.rsplit("(", 1)[1].rstrip(")\n"))
if normalized < 0.01:
    raise SystemExit(f"project collapse did not visibly change the picker: RMSE={normalized}")
print(f"project_collapse_capture_rmse={normalized:.6f}")
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

ctl close-session "$WORKTREE_SESSION_ID" > /dev/null
for _ in $(seq 1 100); do
  if python3 - "$WORKTREE_METADATA" <<'PY'
import json
import pathlib
import sys

metadata = json.loads(pathlib.Path(sys.argv[1]).read_text())
raise SystemExit(0 if metadata["session"] is None else 1)
PY
  then
    break
  fi
  sleep 0.05
done
python3 - "$WORKTREE_METADATA" <<'PY'
import json
import pathlib
import sys

metadata = json.loads(pathlib.Path(sys.argv[1]).read_text())
assert metadata["session"] is None
PY

cat > "$OUT/proof.md" <<EOF
# Recursive session-switcher proof

- backend: headless frame exporter, no presenter and no backend layout
- input: real keyboard and virtual-pointer events through the focused external Winit client
- recursive ownership: session-switcher root lease delegates its rest slot to workspace
- catalog: atomic shared-memory snapshots carried both sessions and all window identities
- projects: two Git repositories rendered as distinct collapsible project lists
- worktree: branch picker-worktree was checked out beneath $DATA_HOME/brood/worktrees and activated as a new session
- retry: requesting the same branch reused the checkout and existing session
- occupation: closing that session persisted an unoccupied worktree record
- switch: selecting Beta published a new layout revision and showed its already-mapped window
- transition: the previous actor layout remained visible until its replacement was accepted
- plugin sizing: pane-local resize and GPU canvas converged to 320x${SIDEBAR_HEIGHT}
- visible: managed pane resized to ${SIDEBAR_WIDTH}x${SIDEBAR_HEIGHT} beside the 320px right sidebar
- hidden: managed pane resized to ${HIDDEN_WIDTH}x${SIDEBAR_HEIGHT} through the same recursive layout fragment
- restored: managed pane returned to ${SIDEBAR_WIDTH}x${SIDEBAR_HEIGHT}
- Ctrl+O: spawned foot in Gamma, expanded the layout to four panes, and imported its DMA-BUF
- Ctrl+O latency: ${CTRL_O_LAYOUT_MS}ms from input helper start through the accepted four-pane layout
- close latency: ${CLOSE_LAYOUT_MS}ms through shared catalog withdrawal and the accepted two-pane layout
- captures: sidebar-visible.png, session-beta.png, sidebar-hidden.png, sidebar-restored.png, ctrl-o-terminal.png, project-expanded.png, project-collapsed.png, worktree-created.png, projects.png
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
echo "worktree=$OUT/worktree-created.png"
echo "projects=$OUT/projects.png"
echo "ctrl_o_layout_ms=$CTRL_O_LAYOUT_MS"
