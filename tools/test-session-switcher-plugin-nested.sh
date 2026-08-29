#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

if [[ -z "${DBUS_SESSION_BUS_ADDRESS:-}" || -n "${BROOD_SURFACE_SESSION_ID:-}" ]]; then
  exec env \
    -u brood_session \
    -u BROOD_SURFACE_SESSION_ID \
    -u BROOD_PANE_ID \
    -u DBUS_SESSION_BUS_ADDRESS \
    dbus-run-session -- "$0" "$@"
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
OUTPUT_MODE="${BROOD_KEYBINDINGS_TEST_OUTPUT_MODE:-1280x720}"
EMPTY_STARTUP_ONLY="${BROOD_EMPTY_STARTUP_ONLY:-0}"
WORKTREE_CREATION_ONLY="${BROOD_WORKTREE_CREATION_ONLY:-0}"
WORKTREE_FAILURE_ONLY="${BROOD_WORKTREE_FAILURE_ONLY:-0}"
if [[ ! "$OUTPUT_MODE" =~ ^[1-9][0-9]*x[1-9][0-9]*$ ]]; then
  echo "ERR BROOD_KEYBINDINGS_TEST_OUTPUT_MODE must be WIDTHxHEIGHT" >&2
  exit 2
fi
if [[ "$OUTPUT_MODE" != "1280x720" && "${BROOD_KEYBINDINGS_ONLY:-0}" != "1" ]]; then
  echo "ERR narrow output mode is supported by the keybindings-only proof" >&2
  exit 2
fi
mkdir -p "$PACKAGE" "$RUNTIME_DIR" "$PROJECT" "$DATA_HOME"
chmod 700 "$RUNTIME_DIR"
git -C "$PROJECT" init --quiet
for index in $(seq 1 180); do
  printf 'original project line %03d\n' "$index"
done > "$PROJECT/tracked.txt"
git -C "$PROJECT" add tracked.txt
git -C "$PROJECT" -c user.name=Brood -c user.email=brood@example.invalid \
  commit --quiet -m initial
git -C "$PROJECT" branch picker-worktree
for index in $(seq 1 180); do
  printf 'changed project line %03d with enough content to exercise split diff scrolling\n' "$index"
done > "$PROJECT/tracked.txt"
printf 'untracked session-local diff\n' > "$PROJECT/untracked.txt"

CLIENT_PID=""
SERVER_PID=""
COMPOSITOR_PID=""
cleanup() {
  local result=$?
  set +e
  terminate_process_bounded "$CLIENT_PID"
  terminate_process_bounded "$SERVER_PID"
  terminate_process_bounded "$COMPOSITOR_PID"
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
  tools/virtual-pointer-input.c \
  -o "$OUT/virtual-pointer-input" \
  "${WAYLAND_BUILD_FLAGS[@]}"
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
  --manifest-path plugins/status-bar/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release \
  --locked
cargo run --quiet -p brood-wasm --example componentize -- \
  plugins/status-bar/target/wasm32-unknown-unknown/release/brood_status_bar_plugin.wasm \
  "$PACKAGE/brood_status_bar_plugin.component.wasm"
cargo build \
  --manifest-path plugins/inbox/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release \
  --locked
cargo run --quiet -p brood-wasm --example componentize -- \
  plugins/inbox/target/wasm32-unknown-unknown/release/brood_inbox_plugin.wasm \
  "$PACKAGE/brood_inbox_plugin.component.wasm"
cargo build \
  --manifest-path plugins/picker/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release \
  --locked
cargo run --quiet -p brood-wasm --example componentize -- \
  plugins/picker/target/wasm32-unknown-unknown/release/brood_picker_plugin.wasm \
  "$PACKAGE/brood_picker_plugin.component.wasm"
cargo build \
  --manifest-path plugins/keybindings/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release \
  --locked
cargo run --quiet -p brood-wasm --example componentize -- \
  plugins/keybindings/target/wasm32-unknown-unknown/release/brood_keybindings_plugin.wasm \
  "$PACKAGE/brood_keybindings_plugin.component.wasm"
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
  --release \
  --locked

printf '[init]\nbackend = "%s"\nclient = "%s"\n\n[client.key-policy]\nprovider = "workspace"\npath = "%s"\n' \
  "$PACKAGE/brood_worktree_manager_plugin.component.wasm" \
  "$PACKAGE/brood_session_switcher_plugin.component.wasm" \
  "$ROOT/plugins/default-workspace/target/wasm32-unknown-unknown/release/brood_default_workspace_plugin.wasm" \
  > "$CONFIG"

RUST_LOG="${RUST_LOG:-broodd=info,brood_client=info,smithay=warn,wgpu_core=warn,wgpu_hal=warn}"
SERVER_EXTRA_ARGS=()
if [[ "$EMPTY_STARTUP_ONLY" == 1 ]]; then
  SERVER_EXTRA_ARGS+=(--no-initial-surface-session)
fi
(
  cd "$PROJECT"
  env -u brood_session -u BROOD_SURFACE_SESSION_ID -u BROOD_PANE_ID \
    XDG_DATA_HOME="$DATA_HOME" \
    RUST_LOG="$RUST_LOG" \
    "$ROOT/target/debug/brood-sessiond" \
    --headless \
    --no-default-modules \
    --config "$CONFIG" \
    --dbus-destination "$DBUS_DESTINATION" \
    --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
    "${SERVER_EXTRA_ARGS[@]}"
) > "$SERVER_LOG" 2>&1 &
SERVER_PID=$!

ctl() {
  env -u brood_session -u BROOD_SURFACE_SESSION_ID -u BROOD_PANE_ID \
    target/debug/brood ctl \
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

if [[ "$EMPTY_STARTUP_ONLY" != 1 ]]; then
  ctl surface-session new Beta > /dev/null
  ctl close-session 1 > /dev/null
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
fi

printf '%s\n' \
  'xwayland disable' \
  "output HEADLESS-1 mode $OUTPUT_MODE" \
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

env -u brood_session -u BROOD_SURFACE_SESSION_ID -u BROOD_PANE_ID \
  XDG_DATA_HOME="$DATA_HOME" \
  BROOD_KEYBINDINGS_PATH="$OUT/keybindings.json" \
  XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  WGPU_BACKEND=vulkan \
  BROOD_DMABUF_HAPPY_PATH=1 \
  BROOD_TRACE_PLUGIN_INPUT=1 \
  BROOD_TRACE_PLUGIN_GRAPHICS=1 \
  BROOD_TRACE_PLUGIN_TEXT=1 \
  BROOD_TRACE_PLUGIN_WORKERS=1 \
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

capture() {
  local target="$1"
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    grim "$target"
  [[ -s "$target" ]]
}

send_keybindings() {
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-keyboard-key" --ctrl-shift-only 53
}

if [[ "$EMPTY_STARTUP_ONLY" == 1 ]]; then
  for _ in $(seq 1 200); do
    if rg -q 'started presentation WASM actor id=session-switcher ' "$CLIENT_LOG" \
      && rg -q 'started presentation WASM actor id=status-bar ' "$CLIENT_LOG" \
      && rg -q 'started presentation WASM actor id=inbox ' "$CLIENT_LOG" \
      && rg -q 'accepted leased actor layout items=2 panes=2 ' "$CLIENT_LOG"; then
      break
    fi
    sleep 0.05
  done
  SESSION_CATALOG="$(ctl surface-session list-json)"
  python3 - "$SESSION_CATALOG" <<'PY'
import json
import sys

catalog = json.loads(sys.argv[1])
assert catalog["sessions"] == [], catalog
assert catalog["active"] == 0, catalog
PY
  ACTORS_EMPTY="$(ctl actors list)"
  if rg -q 'terminal-ui|terminal-session' <<< "$ACTORS_EMPTY"; then
    echo "ERR terminal actors unexpectedly started for an empty session catalogue" >&2
    exit 1
  fi
  printf '%s\n' "$SESSION_CATALOG" > "$OUT/empty-startup-sessions.json"
  printf '%s\n' "$ACTORS_EMPTY" > "$OUT/empty-startup-actors.txt"

  SWITCHER_PANE="$(
    sed -n 's/.*started presentation WASM actor id=session-switcher .* pane=\([0-9][0-9]*\) .*/\1/p' \
      "$CLIENT_LOG" | tail -1
  )"
  STATUS_PANE="$(
    sed -n 's/.*started presentation WASM actor id=status-bar .* pane=\([0-9][0-9]*\) .*/\1/p' \
      "$CLIENT_LOG" | tail -1
  )"
  INBOX_PANE="$(
    sed -n 's/.*started presentation WASM actor id=inbox .* pane=\([0-9][0-9]*\) .*/\1/p' \
      "$CLIENT_LOG" | tail -1
  )"
  [[ -n "$SWITCHER_PANE" && -n "$STATUS_PANE" && -n "$INBOX_PANE" ]]
  rg -q "sent plugin pane resize pane=$STATUS_PANE logical_size=1280x34 " "$CLIENT_LOG"
  rg -q "accepted plugin draw packet pane=$STATUS_PANE .* gpu_target=1280x34 " "$CLIENT_LOG"
  rg -q "sent plugin pane resize pane=$SWITCHER_PANE logical_size=320x686 " "$CLIENT_LOG"
  rg -q 'accepted leased actor layout items=2 panes=2 ' "$CLIENT_LOG"
  if rg -q "sent plugin pane resize pane=$INBOX_PANE logical_size=(320|480|640)x686 " "$CLIENT_LOG"; then
    echo "ERR hidden inbox pane received a visible-width resize" >&2
    exit 1
  fi

  capture "$OUT/empty-startup.png"
  magick identify -format '%wx%h' "$OUT/empty-startup.png" | rg -qx '1280x720'
  magick "$OUT/empty-startup.png" -crop 1280x34+0+686 +repage \
    "$OUT/empty-startup-status-bar.png"
  STATUS_COLOURS="$(magick "$OUT/empty-startup-status-bar.png" -format '%k' info:)"
  ((STATUS_COLOURS >= 8))

  EMPTY_BACKEND_PICKER_LAYOUT_BEFORE="$(
    rg -c 'accepted leased actor layout' "$CLIENT_LOG" || true
  )"
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-pointer-click" 1244 26 1280 720
  for _ in $(seq 1 200); do
    EMPTY_BACKEND_PICKER_LAYOUT_OPEN="$(
      rg -c 'accepted leased actor layout' "$CLIENT_LOG" || true
    )"
    if ((EMPTY_BACKEND_PICKER_LAYOUT_OPEN > EMPTY_BACKEND_PICKER_LAYOUT_BEFORE)) \
      && rg -q 'sender=session-switcher recipient=picker body=.*"request":"backend-discovery".*"loading":true' "$CLIENT_LOG" \
      && rg -q 'sender=session-switcher recipient=picker body=.*"type":"picker-update".*"loading":false' "$CLIENT_LOG"; then
      break
    fi
    sleep 0.05
  done
  ((EMPTY_BACKEND_PICKER_LAYOUT_OPEN > EMPTY_BACKEND_PICKER_LAYOUT_BEFORE))
  capture "$OUT/empty-startup-backend-picker.png"
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-keyboard-key" --plain 28
  for _ in $(seq 1 400); do
    if rg -q 'presentation actor message realm=Daemon sender=session-switcher recipient=init body=.*"kind":"list-directories"' "$CLIENT_LOG" \
      && rg -q 'backend actor message backend=1 realm=Daemon sender=init recipient=session-switcher body=.*"success":true.*"type":"directory-list"' "$CLIENT_LOG" \
      && rg -q 'sender=session-switcher recipient=picker body=.*"title":"Choose directory"' "$CLIENT_LOG"; then
      break
    fi
    kill -0 "$CLIENT_PID" 2>/dev/null || {
      echo "ERR presentation exited while selecting its empty backend" >&2
      exit 1
    }
    sleep 0.05
  done
  if rg -q 'addressed unknown backend' "$CLIENT_LOG"; then
    echo "ERR empty-backend selection addressed an unknown backend" >&2
    exit 1
  fi
  rg -q 'presentation actor message realm=Daemon sender=session-switcher recipient=init body=.*"kind":"list-directories"' "$CLIENT_LOG"
  rg -q 'backend actor message backend=1 realm=Daemon sender=init recipient=session-switcher body=.*"success":true.*"type":"directory-list"' "$CLIENT_LOG"
  capture "$OUT/empty-startup-directory-picker.png"
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-keyboard-key" --plain 1

  send_keybindings
  for _ in $(seq 1 200); do
    if rg -q 'started presentation WASM actor id=keybindings ' "$CLIENT_LOG" \
      && rg -q 'sent key-policy actor message actor=keybindings' "$CLIENT_LOG" \
      && rg -q 'sender=keybindings recipient=picker body=.*"type":"picker-create"' "$CLIENT_LOG"; then
      break
    fi
    sleep 0.05
  done
  rg -q 'started presentation WASM actor id=keybindings ' "$CLIENT_LOG"
  rg -q 'sent key-policy actor message actor=keybindings' "$CLIENT_LOG"
  rg -q 'sender=keybindings recipient=picker body=.*"type":"picker-create"' "$CLIENT_LOG"
  capture "$OUT/empty-startup-keybindings.png"
  python3 - \
    "$OUT/empty-startup.png" \
    "$OUT/empty-startup-keybindings.png" <<'PY'
import re
import subprocess
import sys

result = subprocess.run(
    ["magick", "compare", "-metric", "RMSE", sys.argv[1], sys.argv[2], "null:"],
    check=False,
    capture_output=True,
    text=True,
)
match = re.search(r"\(([^)]+)\)", result.stderr)
if match is None:
    raise SystemExit(f"missing RMSE metric: {result.stderr}")
normalized = float(match.group(1))
if normalized <= 0.01:
    raise SystemExit(f"keybindings did not visibly alter empty startup: RMSE={normalized}")
print(f"empty_startup_keybindings_rmse={normalized:.6f}")
PY
  cat > "$OUT/proof.md" <<EOF
# Empty startup proof

- session catalogue: zero sessions and no active session
- backend actors: no terminal-session actor
- backend picker: New discovered backend 1 with no sessions, routed a real daemon directory request, and opened the directory picker without an unknown-backend failure
- presentation actors: session-switcher and status-bar visible; inbox started but excluded from the two-pane composition
- status bar: composed at 1280x34 with visible text/detail below a 320x686 session picker and empty content viewport
- synthetic input: Ctrl+Shift+/ made the keybindings actor request the shared picker
- captures: empty-startup.png, empty-startup-status-bar.png, empty-startup-backend-picker.png, empty-startup-directory-picker.png, and empty-startup-keybindings.png
EOF
  echo "empty_startup_plugins=verified"
  echo "proof=$OUT/proof.md"
  exit 0
fi

if [[ "$WORKTREE_CREATION_ONLY" == 1 || "$WORKTREE_FAILURE_ONLY" == 1 ]]; then
  for _ in $(seq 1 200); do
    if rg -q "accepted leased actor layout .* panes=[1-9][0-9]*" "$CLIENT_LOG" \
      && rg -q "backend session catalogue backend=1 sessions=2 active=3" "$CLIENT_LOG"; then
      break
    fi
    sleep 0.05
  done
  rg -q "accepted leased actor layout .* panes=[1-9][0-9]*" "$CLIENT_LOG"
  rg -q "backend session catalogue backend=1 sessions=2 active=3" "$CLIENT_LOG"
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    grim "$OUT/worktree-first-open.png"

  # Exercise the public header action reported by users, rather than the
  # project-row shortcut: New -> Worktree -> branch name.
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-pointer-click" 1244 26 1280 720
  sleep 0.4
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    grim "$OUT/worktree-creation-picker.png"
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-pointer-click" 500 185 1280 720
  sleep 0.3
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    grim "$OUT/worktree-branch-prompt.png"
  if [[ "$WORKTREE_FAILURE_ONLY" == 1 ]]; then
    env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
      wtype x -k BackSpace -s 100 bad..branch -s 100 -k Return
    for _ in $(seq 1 200); do
      if rg -q 'backend actor message backend=1 realm=Daemon sender=init recipient=session-switcher body=.*"success":false.*"type":"worktree-session-completed"' "$CLIENT_LOG"; then
        break
      fi
      sleep 0.05
    done
    rg -q 'backend actor message backend=1 realm=Daemon sender=init recipient=session-switcher body=.*"success":false.*"type":"worktree-session-completed"' "$CLIENT_LOG"
    sleep 0.2
    env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
      grim "$OUT/worktree-failed.png"
    SESSION_JSON="$(ctl surface-session list-json)"
    python3 - "$SESSION_JSON" <<'PY'
import json
import sys

catalog = json.loads(sys.argv[1])
assert len(catalog["sessions"]) == 2
assert all(session["branch"] != "bad..branch" for session in catalog["sessions"])
PY
    [[ -s "$OUT/worktree-first-open.png" ]]
    [[ -s "$OUT/worktree-creation-picker.png" ]]
    [[ -s "$OUT/worktree-branch-prompt.png" ]]
    [[ -s "$OUT/worktree-failed.png" ]]
    echo "session_switcher_worktree_failure=verified"
    echo "artifacts=$OUT"
    exit 0
  fi
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    wtype x -k BackSpace -s 100 picker-worktree -s 100 -k Return

  for _ in $(seq 1 200); do
    if rg -q 'presentation actor message realm=Daemon sender=session-switcher recipient=init body=.*"kind":"create-worktree-session"' "$CLIENT_LOG" \
      && rg -q 'backend actor message backend=1 realm=Daemon sender=init recipient=session-switcher body=.*"success":true.*"type":"worktree-session-completed"' "$CLIENT_LOG" \
      && rg -q "presentation shared catalog .*sessions=3 " "$CLIENT_LOG" \
      && rg -q "backend session catalogue backend=1 sessions=3 active=[1-9][0-9]*" "$CLIENT_LOG"; then
      break
    fi
    sleep 0.05
  done
  rg -q 'presentation actor message realm=Daemon sender=session-switcher recipient=init body=.*"kind":"create-worktree-session"' "$CLIENT_LOG"
  rg -q 'backend actor message backend=1 realm=Daemon sender=init recipient=session-switcher body=.*"success":true.*"type":"worktree-session-completed"' "$CLIENT_LOG"
  rg -q "presentation shared catalog .*sessions=3 " "$CLIENT_LOG"
  rg -q "backend session catalogue backend=1 sessions=3 active=[1-9][0-9]*" "$CLIENT_LOG"

  # This is deliberately the first DBus query after submission. The client
  # must already have received and rendered the async catalogue transition.
  SESSION_JSON="$(ctl surface-session list-json)"
  python3 - "$SESSION_JSON" <<'PY'
import json
import sys

catalog = json.loads(sys.argv[1])
created = next(session for session in catalog["sessions"] if session["name"] == "picker-worktree")
assert created["active"] is True
assert created["worktree"] is True
assert created["branch"] == "picker-worktree"
PY
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    grim "$OUT/worktree-created.png"
  [[ -s "$OUT/worktree-creation-picker.png" ]]
  [[ -s "$OUT/worktree-branch-prompt.png" ]]
  [[ -s "$OUT/worktree-created.png" ]]
  echo "session_switcher_worktree_creation=verified"
  echo "artifacts=$OUT"
  exit 0
fi

for _ in $(seq 1 200); do
  if rg -q "granted rest layout lease .* actor=workspace panes=[1-9][0-9]*" "$CLIENT_LOG" \
    && rg -q "accepted leased actor layout .* panes=[1-9][0-9]*" "$CLIENT_LOG" \
    && rg -q "presentation shared catalog .*active=3 .*sessions=2 windows=2" "$CLIENT_LOG" \
    && rg -q "imported window DMA-BUF window_id=1 " "$CLIENT_LOG" \
    && rg -q "imported window DMA-BUF window_id=2 " "$CLIENT_LOG" \
    && rg -q "requested managed window resize .* logical_size=[0-9]+x[0-9]+" "$CLIENT_LOG"; then
    break
  fi
  sleep 0.1
done
rg -q "granted rest layout lease .* actor=workspace panes=[1-9][0-9]*" "$CLIENT_LOG"
rg -q "accepted leased actor layout .* panes=[1-9][0-9]*" "$CLIENT_LOG"
rg -q "presentation shared catalog .*active=3 .*sessions=2 windows=2" "$CLIENT_LOG"
rg -q "imported window DMA-BUF window_id=1 " "$CLIENT_LOG"
rg -q "imported window DMA-BUF window_id=2 " "$CLIENT_LOG"
read -r SIDEBAR_WIDTH SIDEBAR_HEIGHT < <(
  sed -n 's/.*requested managed window resize .* logical_size=\([0-9][0-9]*\)x\([0-9][0-9]*\).*/\1 \2/p' \
    "$CLIENT_LOG" \
    | tail -1
)
[[ -n "$SIDEBAR_WIDTH" && -n "$SIDEBAR_HEIGHT" ]]

STATUS_PANE="$(
  sed -n 's/.*started presentation WASM actor id=status-bar .* pane=\([0-9][0-9]*\) .*/\1/p' \
    "$CLIENT_LOG" | tail -1
)"
OUTPUT_WIDTH="${OUTPUT_MODE%x*}"
[[ -n "$STATUS_PANE" ]]
for _ in $(seq 1 100); do
  if rg -q "sent plugin pane resize pane=$STATUS_PANE logical_size=${OUTPUT_WIDTH}x34 " "$CLIENT_LOG" \
    && rg -q "accepted plugin draw packet pane=$STATUS_PANE .* gpu_target=${OUTPUT_WIDTH}x34 " "$CLIENT_LOG"; then
    break
  fi
  sleep 0.05
done
rg -q "sent plugin pane resize pane=$STATUS_PANE logical_size=${OUTPUT_WIDTH}x34 " "$CLIENT_LOG"
rg -q "accepted plugin draw packet pane=$STATUS_PANE .* gpu_target=${OUTPUT_WIDTH}x34 " "$CLIENT_LOG"

if [[ "$OUTPUT_MODE" == "1280x720" ]]; then
  for _ in $(seq 1 100); do
    if rg -q "sent plugin pane resize .* logical_size=320x${SIDEBAR_HEIGHT}" "$CLIENT_LOG" \
      && rg -q "accepted plugin draw packet .* gpu_target=320x${SIDEBAR_HEIGHT}" "$CLIENT_LOG"; then
      break
    fi
    sleep 0.05
  done
  rg -q "sent plugin pane resize .* logical_size=320x${SIDEBAR_HEIGHT}" "$CLIENT_LOG"
  rg -q "accepted plugin draw packet .* gpu_target=320x${SIDEBAR_HEIGHT}" "$CLIENT_LOG"
fi

capture() {
  local target="$1"
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    grim "$target"
  [[ -s "$target" ]]
}

accepted_layout_count() {
  rg -c "accepted leased actor layout" "$CLIENT_LOG" || true
}

send_toggle() {
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-keyboard-key" 23
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

send_keybindings() {
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-keyboard-key" --ctrl-shift-only 53
}

send_diff_toggle() {
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-keyboard-key" 32
}

send_close_pane() {
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-keyboard-key" 16
}

send_pointer() {
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-pointer-input" "$@"
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

KEYBINDINGS_LAYOUT_BEFORE="$(accepted_layout_count)"
send_keybindings
for _ in $(seq 1 100); do
  KEYBINDINGS_LAYOUT_OPEN="$(accepted_layout_count)"
  if rg -q "started presentation WASM actor id=keybindings" "$CLIENT_LOG" \
    && rg -q "sent key-policy actor message actor=keybindings" "$CLIENT_LOG" \
    && rg -q 'sender=keybindings recipient=picker body=.*"type":"picker-create"' "$CLIENT_LOG" \
    && ((KEYBINDINGS_LAYOUT_OPEN > KEYBINDINGS_LAYOUT_BEFORE)); then
    break
  fi
  sleep 0.05
done
rg -q "started presentation WASM actor id=keybindings" "$CLIENT_LOG"
rg -q "sent key-policy actor message actor=keybindings" "$CLIENT_LOG"
rg -q 'sender=keybindings recipient=picker body=.*"type":"picker-create"' "$CLIENT_LOG"
((KEYBINDINGS_LAYOUT_OPEN > KEYBINDINGS_LAYOUT_BEFORE))
KEYBINDINGS_PANE="$(
  sed -n 's/.*started presentation WASM actor id=keybindings .* pane=\([0-9][0-9]*\) .*/\1/p' \
    "$CLIENT_LOG" \
    | tail -1
)"
[[ -n "$KEYBINDINGS_PANE" ]]
for _ in $(seq 1 100); do
  KEYBINDINGS_RESIZE_LINE="$(
    rg -n "sent plugin pane resize pane=${KEYBINDINGS_PANE} " "$CLIENT_LOG" \
      | tail -1 \
      | cut -d: -f1 \
      || true
  )"
  KEYBINDINGS_REDRAW_LINE="$(
    rg -n "accepted plugin draw packet pane=${KEYBINDINGS_PANE} " "$CLIENT_LOG" \
      | tail -1 \
      | cut -d: -f1 \
      || true
  )"
  if [[ -n "$KEYBINDINGS_RESIZE_LINE" && -n "$KEYBINDINGS_REDRAW_LINE" ]] \
    && ((KEYBINDINGS_REDRAW_LINE > KEYBINDINGS_RESIZE_LINE)); then
    break
  fi
  sleep 0.05
done
((KEYBINDINGS_REDRAW_LINE > KEYBINDINGS_RESIZE_LINE))
capture "$OUT/keybindings-visible.png"

env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  wtype "toggle session switcher" -s 100
capture "$OUT/keybindings-filtered.png"
KEYBINDINGS_CAPTURE_LAYOUT_BEFORE="$(accepted_layout_count)"
env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  "$OUT/virtual-keyboard-key" --none 28
for _ in $(seq 1 100); do
  KEYBINDINGS_CAPTURE_LAYOUT_AFTER="$(accepted_layout_count)"
  if rg -q 'sender=picker recipient=keybindings body=.*"type":"picker-selected"' "$CLIENT_LOG" \
    && rg -q 'sender=keybindings recipient=session-switcher body=.*"type":"keybindings-pane".*"visible":true' "$CLIENT_LOG" \
    && ((KEYBINDINGS_CAPTURE_LAYOUT_AFTER > KEYBINDINGS_CAPTURE_LAYOUT_BEFORE)); then
    break
  fi
  sleep 0.05
done
rg -q 'sender=picker recipient=keybindings body=.*"type":"picker-selected"' "$CLIENT_LOG"
rg -q 'sender=keybindings recipient=session-switcher body=.*"type":"keybindings-pane".*"visible":true' "$CLIENT_LOG"
((KEYBINDINGS_CAPTURE_LAYOUT_AFTER > KEYBINDINGS_CAPTURE_LAYOUT_BEFORE))
env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  "$OUT/virtual-keyboard-key" 23
for _ in $(seq 1 100); do
  if [[ -s "$OUT/keybindings.json" ]] \
    && rg -q '"action": "sessions.toggle"' "$OUT/keybindings.json" \
    && rg -q '"action": "inbox.show"' "$OUT/keybindings.json"; then
    break
  fi
  sleep 0.05
done
rg -q '"action": "sessions.toggle"' "$OUT/keybindings.json"
rg -q '"action": "inbox.show"' "$OUT/keybindings.json"
python3 - "$OUT/keybindings.json" <<'PY'
import json
import sys

document = json.load(open(sys.argv[1], encoding="utf-8"))
bindings = {
    item["action"]: (item["key_code"], item["modifiers"])
    for item in document["bindings"]
}
assert bindings["sessions.toggle"] == (31, 3), bindings
assert bindings["inbox.show"] == (58, 3), bindings
PY

KEYBINDINGS_LAYOUT_VISIBLE="$(accepted_layout_count)"
if [[ "$OUTPUT_MODE" == "1280x720" ]]; then
  KEYBINDINGS_CLOSE_PROOF="Clicking the dimmed backdrop closed the window."
  click_at 32 360
else
  KEYBINDINGS_CLOSE_PROOF="Escape closed the window in the narrow presentation."
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-keyboard-key" --none 1
fi
for _ in $(seq 1 100); do
  KEYBINDINGS_LAYOUT_CLOSED="$(accepted_layout_count)"
  ((KEYBINDINGS_LAYOUT_CLOSED > KEYBINDINGS_LAYOUT_VISIBLE)) && break
  sleep 0.05
done
((KEYBINDINGS_LAYOUT_CLOSED > KEYBINDINGS_LAYOUT_VISIBLE))

KEYBINDINGS_TOGGLE_LOGS="$(
  rg -c "sent key-policy actor message actor=session-switcher" "$CLIENT_LOG" || true
)"
env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  "$OUT/virtual-keyboard-key" 23
for _ in $(seq 1 100); do
  CURRENT_TOGGLE_LOGS="$(
    rg -c "sent key-policy actor message actor=session-switcher" "$CLIENT_LOG" || true
  )"
  ((CURRENT_TOGGLE_LOGS > KEYBINDINGS_TOGGLE_LOGS)) && break
  sleep 0.05
done
((CURRENT_TOGGLE_LOGS > KEYBINDINGS_TOGGLE_LOGS))
capture "$OUT/keybindings-remapped-toggle.png"
env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  "$OUT/virtual-keyboard-key" 23

if [[ "${BROOD_KEYBINDINGS_ONLY:-0}" == "1" ]]; then
  cat > "$OUT/proof.md" <<EOF
# Global keybinding remapping proof

- Ctrl+Shift+/ made the keybindings actor send its action catalogue to the shared picker.
- Selecting the action closed the picker and opened the keybindings actor only for physical-chord capture.
- $KEYBINDINGS_CLOSE_PROOF
- Fuzzy text \`toggle session switcher\` narrowed the action catalogue.
- Capturing Ctrl+Alt+I rebound \`sessions.toggle\` and swapped the conflicting
  \`inbox.show\` action onto the old Ctrl+Alt+M chord.
- The client persisted both overrides in $OUT/keybindings.json.
- Pressing Ctrl+Alt+I after closing the popup invoked the remapped session
  switcher action and visibly changed the recursive layout.
- captures: keybindings-visible.png, keybindings-filtered.png, keybindings-remapped-toggle.png
EOF
  ctl quit > /dev/null 2>&1 || true
  echo "keybindings_remap=verified"
  echo "proof=$OUT/proof.md"
  echo "visible=$OUT/keybindings-visible.png"
  echo "filtered=$OUT/keybindings-filtered.png"
  echo "remapped=$OUT/keybindings-remapped-toggle.png"
  exit 0
fi

send_pointer hover 1244 26 700 1280 720 &
HOVER_POINTER_PID=$!
sleep 0.25
capture "$OUT/sidebar-new-hover.png"
wait "$HOVER_POINTER_PID"

env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  "$OUT/virtual-pointer-click" 1244 26 1280 720 700 1000 26 &
PRESS_POINTER_PID=$!
sleep 0.25
capture "$OUT/sidebar-new-pressed.png"
wait "$PRESS_POINTER_PID"

if [[ "${BROOD_UI_INTERACTION_ONLY:-0}" == "1" ]]; then
  magick "$OUT/sidebar-visible.png" -crop 320x52+960+0 +repage "$OUT/new-idle-crop.png"
  magick "$OUT/sidebar-new-hover.png" -crop 320x52+960+0 +repage "$OUT/new-hover-crop.png"
  magick "$OUT/sidebar-new-pressed.png" -crop 320x52+960+0 +repage "$OUT/new-pressed-crop.png"
  python3 - \
    "$OUT/new-idle-crop.png" \
    "$OUT/new-hover-crop.png" \
    "$OUT/new-pressed-crop.png" <<'PY'
import subprocess
import sys

def rmse(left, right):
    output = subprocess.run(
        ["magick", "compare", "-metric", "RMSE", left, right, "null:"],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    ).stderr
    return float(output.rsplit("(", 1)[1].rstrip(")\n"))

hover = rmse(sys.argv[1], sys.argv[2])
pressed = rmse(sys.argv[2], sys.argv[3])
if hover < 0.005:
    raise SystemExit(f"New hover feedback was not visible: RMSE={hover}")
if pressed < 0.005:
    raise SystemExit(f"New pressed feedback was not visible: RMSE={pressed}")
print(f"new_button_hover_rmse={hover:.6f}")
print(f"new_button_pressed_rmse={pressed:.6f}")
PY
  if rg -q "sent plugin pane resize .* logical_size=720x[0-9]+" "$CLIENT_LOG"; then
    echo "ERR releasing away from New unexpectedly opened the picker" >&2
    exit 1
  fi
  cat > "$OUT/proof.txt" <<EOF
UI interaction feedback proof
- real workload: offscreen nested Brood presentation with inbox, session switcher, and managed clients
- pointer: hover over New, hold primary button, move away, release
- behavior: hover and pressed frames differ visibly; release-away cancels activation
- captures: sidebar-visible.png, sidebar-new-hover.png, sidebar-new-pressed.png
EOF
  cat "$OUT/proof.txt"
  echo "nested UI interaction feedback proof passed"
  echo "artifacts=$OUT"
  exit 0
fi

BACKEND_PICKER_LAYOUT_BEFORE="$(accepted_layout_count)"
click_at 1244 26
for _ in $(seq 1 100); do
  BACKEND_PICKER_LAYOUT_OPEN="$(accepted_layout_count)"
  if ((BACKEND_PICKER_LAYOUT_OPEN > BACKEND_PICKER_LAYOUT_BEFORE)) \
    && rg -q 'sender=session-switcher recipient=picker body=.*"request":"backend-discovery".*"loading":true' "$CLIENT_LOG" \
    && rg -q 'sender=session-switcher recipient=picker body=.*"type":"picker-update".*"loading":false' "$CLIENT_LOG"; then
    break
  fi
  sleep 0.05
done
((BACKEND_PICKER_LAYOUT_OPEN > BACKEND_PICKER_LAYOUT_BEFORE))
rg -q 'sender=session-switcher recipient=picker body=.*"request":"backend-discovery".*"loading":true' "$CLIENT_LOG"
rg -q 'sender=session-switcher recipient=picker body=.*"type":"picker-update".*"loading":false' "$CLIENT_LOG"
capture "$OUT/backend-picker-visible.png"
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  "$OUT/virtual-keyboard-key" --plain 1
for _ in $(seq 1 100); do
  BACKEND_PICKER_LAYOUT_CLOSED="$(accepted_layout_count)"
  ((BACKEND_PICKER_LAYOUT_CLOSED > BACKEND_PICKER_LAYOUT_OPEN)) && break
  sleep 0.05
done
((BACKEND_PICKER_LAYOUT_CLOSED > BACKEND_PICKER_LAYOUT_OPEN))

accepted_before="$(accepted_layout_count)"
send_picker
for _ in $(seq 1 100); do
  accepted_picker="$(accepted_layout_count)"
  if rg -q "started presentation WASM actor id=picker" "$CLIENT_LOG" \
    && ((accepted_picker > accepted_before)) \
    && rg -q "sent plugin pane resize .* logical_size=720x[0-9]+" "$CLIENT_LOG"; then
    break
  fi
  sleep 0.05
done
rg -q "started presentation WASM actor id=picker" "$CLIENT_LOG"
((accepted_picker > accepted_before))
rg -q "sent plugin pane resize .* logical_size=720x[0-9]+" "$CLIENT_LOG"
capture "$OUT/picker-visible.png"
PICKER_BOTTOM_LEFT="$(
  magick "$OUT/picker-visible.png" -format '%[pixel:p{280,599}]' info:
)"
PICKER_BOTTOM_LEFT_INSET="$(
  magick "$OUT/picker-visible.png" -format '%[pixel:p{288,591}]' info:
)"
PICKER_BOTTOM_RIGHT="$(
  magick "$OUT/picker-visible.png" -format '%[pixel:p{999,599}]' info:
)"
PICKER_BOTTOM_RIGHT_INSET="$(
  magick "$OUT/picker-visible.png" -format '%[pixel:p{991,591}]' info:
)"
if [[ "$PICKER_BOTTOM_LEFT" == "$PICKER_BOTTOM_LEFT_INSET" \
  && "$PICKER_BOTTOM_RIGHT" == "$PICKER_BOTTOM_RIGHT_INSET" ]]; then
  echo "ERR picker footer painted over its rounded bottom corners" >&2
  exit 1
fi

env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  wtype "Bt" -s 100 -k Return
for _ in $(seq 1 100); do
  accepted_after="$(accepted_layout_count)"
  if rg -q "presentation shared catalog .*active=2 .*sessions=2 windows=2" "$CLIENT_LOG" \
    && ((accepted_after > accepted_picker)); then
    break
  fi
  sleep 0.05
done
rg -q "presentation shared catalog .*active=2 .*sessions=2 windows=2" "$CLIENT_LOG"
((accepted_after > accepted_picker))
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
CTRL_O_LAYOUTS_BEFORE="$(accepted_layout_count)"
send_open_terminal &
CTRL_O_INPUT_PID=$!
for _ in $(seq 1 200); do
  CTRL_O_LAYOUTS_AFTER="$(accepted_layout_count)"
  if rg -q "total_windows=3" <<< "$(ctl status)" \
    && rg -q "presentation shared catalog .*active=3 .*sessions=2 windows=3" "$CLIENT_LOG" \
    && ((CTRL_O_LAYOUTS_AFTER > CTRL_O_LAYOUTS_BEFORE)) \
    && rg -q "imported window DMA-BUF window_id=3 " "$CLIENT_LOG"; then
    break
  fi
  sleep 0.01
done
wait "$CTRL_O_INPUT_PID"
CTRL_O_LAYOUT_MS=$(( $(date +%s%3N) - CTRL_O_STARTED_MS ))
rg -q "total_windows=3" <<< "$(ctl status)"
rg -q "presentation shared catalog .*active=3 .*sessions=2 windows=3" "$CLIENT_LOG"
((CTRL_O_LAYOUTS_AFTER > CTRL_O_LAYOUTS_BEFORE))
rg -q "imported window DMA-BUF window_id=3 " "$CLIENT_LOG"
capture "$OUT/ctrl-o-terminal.png"

CATALOG_TWO_BEFORE="$(rg -c "presentation shared catalog .*windows=2" "$CLIENT_LOG")"
LAYOUT_TWO_BEFORE="$(accepted_layout_count)"
CLOSE_STARTED_MS="$(date +%s%3N)"
send_close_pane
for _ in $(seq 1 200); do
  CATALOG_TWO_AFTER="$(rg -c "presentation shared catalog .*windows=2" "$CLIENT_LOG")"
  LAYOUT_TWO_AFTER="$(accepted_layout_count)"
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

DIFF_PACKETS_BEFORE="$(rg -c "accepted plugin draw packet" "$CLIENT_LOG")"
DIFF_ACTOR="diff-viewer-sb1-s3"
send_diff_toggle
for _ in $(seq 1 200); do
  DIFF_PACKETS_AFTER="$(rg -c "accepted plugin draw packet" "$CLIENT_LOG")"
  if rg -q "started presentation WASM actor id=$DIFF_ACTOR" "$CLIENT_LOG" \
    && rg -q "sent key-policy actor message actor=brood.client-init" "$CLIENT_LOG" \
    && ((DIFF_PACKETS_AFTER > DIFF_PACKETS_BEFORE)) \
    && rg -q "focused local plugin pane=" "$CLIENT_LOG"; then
    break
  fi
  sleep 0.05
done
rg -q "started presentation WASM actor id=$DIFF_ACTOR" "$CLIENT_LOG"
rg -q "sent key-policy actor message actor=brood.client-init" "$CLIENT_LOG"
((DIFF_PACKETS_AFTER > DIFF_PACKETS_BEFORE))
DIFF_PANE="$(
  sed -n "/started presentation WASM actor id=$DIFF_ACTOR/,\$p" "$CLIENT_LOG" \
    | sed -n 's/.*focused local plugin pane=\([0-9][0-9]*\).*/\1/p' \
    | head -1
)"
[[ -n "$DIFF_PANE" ]]
INITIAL_DIFF_SEQUENCE="$(
  sed -n "/started presentation WASM actor id=$DIFF_ACTOR/,\$p" "$CLIENT_LOG" \
    | sed -n 's/.*accepted plugin draw packet pane='"$DIFF_PANE"' sequence=\([0-9][0-9]*\).*/\1/p' \
    | head -1
)"
[[ -n "$INITIAL_DIFF_SEQUENCE" ]]
for _ in $(seq 1 400); do
  POPULATED_DIFF_SEQUENCE="$(
    sed -n "/presentation actor worker finished actor=$DIFF_ACTOR/,\$p" "$CLIENT_LOG" \
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
  sed -n "/started presentation WASM actor id=$DIFF_ACTOR/,\$p" "$CLIENT_LOG" \
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
send_pointer drag "$DRAG_X" "$DRAG_START_Y" "$DRAG_X" "$DRAG_END_Y" 12 1280 720
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

CLOSED_LAYOUTS_BEFORE="$(accepted_layout_count)"
send_close_pane
for _ in $(seq 1 100); do
  CLOSED_LAYOUTS_AFTER="$(accepted_layout_count)"
  if rg -q "stopped presentation init child actor id=$DIFF_ACTOR" "$CLIENT_LOG" \
    && ((CLOSED_LAYOUTS_AFTER > CLOSED_LAYOUTS_BEFORE)); then
    break
  fi
  sleep 0.05
done
rg -q "stopped presentation init child actor id=$DIFF_ACTOR" "$CLIENT_LOG"
((CLOSED_LAYOUTS_AFTER > CLOSED_LAYOUTS_BEFORE))
capture "$OUT/diff-closed.png"

capture "$OUT/project-expanded.png"
click_at 980 68
sleep 0.2
capture "$OUT/project-collapsed.png"

click_at 980 68
sleep 0.2

WORKTREE_DAEMON_REQUESTS_BEFORE="$(
  rg -c "delivered presentation-to-daemon actor message" "$SERVER_LOG" || true
)"
click_at 1260 68
submit_text picker-worktree
for _ in $(seq 1 200); do
  WORKTREE_DAEMON_REQUESTS_AFTER="$(
    rg -c "delivered presentation-to-daemon actor message" "$SERVER_LOG" || true
  )"
  if ((WORKTREE_DAEMON_REQUESTS_AFTER > WORKTREE_DAEMON_REQUESTS_BEFORE)) \
    && rg -q "attached managed worktree session" "$SERVER_LOG" \
    && rg -q "presentation shared catalog .*sessions=3 " "$CLIENT_LOG" \
    && rg -q "backend session catalogue backend=1 sessions=3 active=[1-9][0-9]*" "$CLIENT_LOG"; then
    break
  fi
  sleep 0.05
done
((WORKTREE_DAEMON_REQUESTS_AFTER > WORKTREE_DAEMON_REQUESTS_BEFORE))
rg -q "attached managed worktree session" "$SERVER_LOG"
rg -q "presentation shared catalog .*sessions=3 " "$CLIENT_LOG"
rg -q "backend session catalogue backend=1 sessions=3 active=[1-9][0-9]*" "$CLIENT_LOG"
# Query only after the presentation has independently observed the new
# catalogue. Polling DBus here used to mask missing async catalogue publication
# because every control request publishes the daemon's latest snapshot.
SESSION_JSON="$(ctl surface-session list-json)"
rg -q '"name":"picker-worktree"' <<< "$SESSION_JSON"
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
assert created["active"] is True
assert created["windows"] == 0
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
rg -q "delivered presentation-to-daemon actor message" "$SERVER_LOG"
rg -q "realm=Daemon sender=\"init\" actor=\"session-switcher\"" "$SERVER_LOG"
rg -q "attached managed worktree session" "$SERVER_LOG"
capture "$OUT/worktree-created.png"

WORKTREE_DIFF_ACTOR="diff-viewer-sb1-s${WORKTREE_SESSION_ID}"
WORKTREE_DIFF_STARTS_BEFORE="$(
  rg -c "started presentation WASM actor id=$WORKTREE_DIFF_ACTOR" "$CLIENT_LOG" || true
)"
WORKTREE_DIFF_PACKETS_BEFORE="$(rg -c "accepted plugin draw packet" "$CLIENT_LOG")"
send_diff_toggle
for _ in $(seq 1 200); do
  WORKTREE_DIFF_STARTS_AFTER="$(
    rg -c "started presentation WASM actor id=$WORKTREE_DIFF_ACTOR" "$CLIENT_LOG" || true
  )"
  WORKTREE_DIFF_PACKETS_AFTER="$(rg -c "accepted plugin draw packet" "$CLIENT_LOG")"
  if ((WORKTREE_DIFF_STARTS_AFTER > WORKTREE_DIFF_STARTS_BEFORE)) \
    && ((WORKTREE_DIFF_PACKETS_AFTER > WORKTREE_DIFF_PACKETS_BEFORE)); then
    break
  fi
  sleep 0.05
done
((WORKTREE_DIFF_STARTS_AFTER > WORKTREE_DIFF_STARTS_BEFORE))
((WORKTREE_DIFF_PACKETS_AFTER > WORKTREE_DIFF_PACKETS_BEFORE))
WORKTREE_DIFF_PANE="$(
  sed -n "/started presentation WASM actor id=$WORKTREE_DIFF_ACTOR/,\$p" "$CLIENT_LOG" \
    | sed -n 's/.*focused local plugin pane=\([0-9][0-9]*\).*/\1/p' \
    | head -1
)"
[[ -n "$WORKTREE_DIFF_PANE" ]]
capture "$OUT/worktree-plugin-before-switch.png"

send_picker
env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  wtype "Bt" -s 100 -k Return
for _ in $(seq 1 100); do
  SESSION_JSON="$(ctl surface-session list-json)"
  if python3 - "$SESSION_JSON" "$WORKTREE_SESSION_ID" <<'PY'
import json
import sys

catalog = json.loads(sys.argv[1])
worktree = next(session for session in catalog["sessions"] if session["id"] == int(sys.argv[2]))
active = next(session for session in catalog["sessions"] if session["active"])
raise SystemExit(0 if active["name"] == "Beta" and worktree["windows"] == 0 else 1)
PY
  then
    break
  fi
  sleep 0.05
done
python3 - "$SESSION_JSON" "$WORKTREE_SESSION_ID" <<'PY'
import json
import sys

catalog = json.loads(sys.argv[1])
worktree = next(session for session in catalog["sessions"] if session["id"] == int(sys.argv[2]))
active = next(session for session in catalog["sessions"] if session["active"])
assert active["name"] == "Beta"
assert worktree["windows"] == 0
PY
if sed -n "/started presentation WASM actor id=$WORKTREE_DIFF_ACTOR/,\$p" "$CLIENT_LOG" \
  | rg -q "stopped presentation init child actor id=$WORKTREE_DIFF_ACTOR"; then
  echo "ERR session-scoped plugin actor stopped while its windowless session was inactive" >&2
  exit 1
fi

WORKTREE_DIFF_LAYOUTS_BEFORE_RETURN="$(
  accepted_layout_count
)"
send_picker
env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  wtype "picker-worktree" -s 100 -k Return
for _ in $(seq 1 200); do
  SESSION_JSON="$(ctl surface-session list-json)"
  WORKTREE_DIFF_LAYOUTS_AFTER_RETURN="$(
    accepted_layout_count
  )"
  if python3 - "$SESSION_JSON" "$WORKTREE_SESSION_ID" <<'PY'
import json
import sys

catalog = json.loads(sys.argv[1])
worktree = next(session for session in catalog["sessions"] if session["id"] == int(sys.argv[2]))
raise SystemExit(0 if worktree["active"] and worktree["windows"] == 0 else 1)
PY
  then
    ((WORKTREE_DIFF_LAYOUTS_AFTER_RETURN > WORKTREE_DIFF_LAYOUTS_BEFORE_RETURN)) && break
  fi
  sleep 0.05
done
python3 - "$SESSION_JSON" "$WORKTREE_SESSION_ID" <<'PY'
import json
import sys

catalog = json.loads(sys.argv[1])
worktree = next(session for session in catalog["sessions"] if session["id"] == int(sys.argv[2]))
assert worktree["active"] is True
assert worktree["windows"] == 0
PY
[[ "$(
  rg -c "started presentation WASM actor id=$WORKTREE_DIFF_ACTOR" "$CLIENT_LOG"
)" == "$WORKTREE_DIFF_STARTS_AFTER" ]]
((WORKTREE_DIFF_LAYOUTS_AFTER_RETURN > WORKTREE_DIFF_LAYOUTS_BEFORE_RETURN))
capture "$OUT/worktree-plugin-after-switch.png"

WORKTREE_DIFF_FOCUSES_BEFORE="$(
  rg -c "focused local plugin pane=$WORKTREE_DIFF_PANE" "$CLIENT_LOG"
)"
click_at 480 360
for _ in $(seq 1 100); do
  WORKTREE_DIFF_FOCUSES_AFTER="$(
    rg -c "focused local plugin pane=$WORKTREE_DIFF_PANE" "$CLIENT_LOG"
  )"
  if ((WORKTREE_DIFF_FOCUSES_AFTER > WORKTREE_DIFF_FOCUSES_BEFORE)); then
    break
  fi
  sleep 0.05
done
((WORKTREE_DIFF_FOCUSES_AFTER > WORKTREE_DIFF_FOCUSES_BEFORE))
send_close_pane
for _ in $(seq 1 100); do
  if rg -q "stopped presentation init child actor id=$WORKTREE_DIFF_ACTOR" "$CLIENT_LOG"; then
    break
  fi
  sleep 0.05
done
rg -q "stopped presentation init child actor id=$WORKTREE_DIFF_ACTOR" "$CLIENT_LOG"
SESSION_JSON="$(ctl surface-session list-json)"
python3 - "$SESSION_JSON" "$WORKTREE_SESSION_ID" <<'PY'
import json
import sys

catalog = json.loads(sys.argv[1])
worktree = next(session for session in catalog["sessions"] if session["id"] == int(sys.argv[2]))
assert worktree["active"] is True
assert worktree["windows"] == 0
PY

WORKTREE_COMPLETIONS_BEFORE="$(
  rg -c "delivered presentation-to-daemon actor message" "$SERVER_LOG" || true
)"
click_at 1260 68
submit_text picker-worktree
for _ in $(seq 1 100); do
  WORKTREE_COMPLETIONS_AFTER="$(
    rg -c "delivered presentation-to-daemon actor message" "$SERVER_LOG" || true
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
  "$OUT/picker-visible.png" \
  "$OUT/diff-open.png" \
  "$OUT/diff-dragged.png" \
  "$OUT/diff-closed.png" \
  "$OUT/project-expanded.png" \
  "$OUT/project-collapsed.png" \
  "$OUT/worktree-created.png" \
  "$OUT/worktree-plugin-before-switch.png" \
  "$OUT/worktree-plugin-after-switch.png" \
  "$OUT/projects.png" \
  "$OUT/sidebar-new-hover.png" \
  "$OUT/sidebar-new-pressed.png" <<'PY'
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
    raise SystemExit(f"close-pane binding did not visibly close the diff pane: RMSE={normalized}")
print(f"diff_close_capture_rmse={normalized:.6f}")

metric = subprocess.run(
    ["magick", "compare", "-metric", "RMSE", sys.argv[10], sys.argv[11], "null:"],
    text=True,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    check=False,
).stderr
normalized = float(metric.rsplit("(", 1)[1].rstrip(")\n"))
if normalized < 0.01:
    raise SystemExit(f"project collapse did not visibly change the picker: RMSE={normalized}")
print(f"project_collapse_capture_rmse={normalized:.6f}")

metric = subprocess.run(
    ["magick", "compare", "-metric", "RMSE", sys.argv[1], sys.argv[16], "null:"],
    text=True,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    check=False,
).stderr
normalized = float(metric.rsplit("(", 1)[1].rstrip(")\n"))
if normalized < 0.0005:
    raise SystemExit(f"hovering New did not visibly change the control: RMSE={normalized}")
print(f"new_button_hover_rmse={normalized:.6f}")

metric = subprocess.run(
    ["magick", "compare", "-metric", "RMSE", sys.argv[16], sys.argv[17], "null:"],
    text=True,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    check=False,
).stderr
normalized = float(metric.rsplit("(", 1)[1].rstrip(")\n"))
if normalized < 0.0005:
    raise SystemExit(f"pressing New did not visibly change the control: RMSE={normalized}")
print(f"new_button_pressed_rmse={normalized:.6f}")
PY

STATUS="$(ctl status)"
rg -q "presentation=Attached" <<< "$STATUS"
rg -q "accepted leased actor layout .* panes=[1-9][0-9]*" "$CLIENT_LOG"
rg -q "sent key-policy actor message actor=session-switcher" "$CLIENT_LOG"
if rg -q "rejected actor-submitted layout; using native fallback" "$CLIENT_LOG"; then
  echo "ERR client abandoned actor layout after rejecting a recursive update" >&2
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
- input: real Ctrl+Alt+M, Ctrl+Alt+P, Ctrl+Alt+D, Ctrl+Alt+Q, fuzzy text, Enter, pointer clicks and drag, touchpad scroll, and Ctrl+O events through the focused external Winit client
- recursive ownership: session-switcher root lease delegates its rest slot to workspace
- catalog: atomic shared-memory snapshots carried both sessions and all window identities
- projects: two Git repositories rendered as distinct collapsible project lists
- worktree: an addressed presentation-to-daemon actor message checked out branch picker-worktree beneath $DATA_HOME/brood/worktrees and activated it as a new session
- durable plugin session: the windowless worktree session and its diff pane survived switching to Beta; returning reused the same presentation actor generation and rendered pane
- retry: requesting the same branch reused the checkout and existing session
- occupation: closing that session persisted an unoccupied worktree record
- switch: selecting Beta published a new layout revision and showed its already-mapped window
- picker: Ctrl+Alt+P opened the reusable actor as a 720px floating pane with visibly rounded bottom corners; Bt fuzzy-matched Beta and Enter returned the original session result
- backend picker: clicking New opened a loading picker before catalogue discovery and updated the same picker with the discovered backend options
- transition: the previous actor layout remained visible until its replacement was accepted
- plugin sizing: pane-local resize and GPU canvas converged to 320x${SIDEBAR_HEIGHT}
- visible: managed pane resized to ${SIDEBAR_WIDTH}x${SIDEBAR_HEIGHT} beside the 320px right sidebar
- hidden: managed pane resized to ${HIDDEN_WIDTH}x${SIDEBAR_HEIGHT} through the same recursive layout fragment
- restored: managed pane returned to ${SIDEBAR_WIDTH}x${SIDEBAR_HEIGHT}
- Ctrl+O: spawned foot in Gamma, expanded the layout to three panes, and imported its DMA-BUF
- Ctrl+O latency: ${CTRL_O_LAYOUT_MS}ms from input helper start through the accepted three-pane layout
- managed close: Ctrl+Alt+Q closed the focused foot pane in ${CLOSE_LAYOUT_MS}ms through shared catalog withdrawal and the accepted two-pane layout
- diff viewer: Ctrl+Alt+D created and focused the session-scoped split-diff pane; a real pointer press, motion burst, and release dragged its scrollbar; Ctrl+Alt+Q stopped the focused actor and closed its pane
- diff drag rendering: ${DRAG_PACKET_COUNT} draw packets covered the pointer grab from the first thumb press through release
- diff touchpad rendering: 40 fractional-pixel finger-axis events settled in ${TRACKPAD_SETTLED_MS}ms; ${TRACKPAD_PACKET_COUNT} draw packets created ${TRACKPAD_TEXT_LAYOUTS} text layouts
- interaction feedback: the New control visibly changed between idle, hover, and held-press captures; releasing away from the control cancelled activation
- captures: sidebar-visible.png, sidebar-new-hover.png, sidebar-new-pressed.png, backend-picker-visible.png, picker-visible.png, session-beta.png, sidebar-hidden.png, sidebar-restored.png, ctrl-o-terminal.png, diff-open.png, diff-dragged.png, diff-closed.png, project-expanded.png, project-collapsed.png, worktree-created.png, worktree-plugin-before-switch.png, worktree-plugin-after-switch.png, projects.png
- status: $STATUS
EOF

ctl quit > /dev/null 2>&1 || true
echo "session_switcher_recursive=verified"
echo "proof=$OUT/proof.md"
echo "visible=$OUT/sidebar-visible.png"
echo "picker=$OUT/picker-visible.png"
echo "backend_picker=$OUT/backend-picker-visible.png"
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
echo "worktree=$OUT/worktree-created.png"
echo "projects=$OUT/projects.png"
echo "ctrl_o_layout_ms=$CTRL_O_LAYOUT_MS"
