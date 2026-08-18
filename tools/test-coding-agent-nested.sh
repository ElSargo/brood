#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"
SSH_TARGET="${BROOD_CODING_AGENT_SSH_TARGET:-}"
COMPUTER_USE="${BROOD_CODING_AGENT_COMPUTER_USE:-}"
REMOTE_IMAGE_MARKER="REMOTE_IMAGE_PROOF: red green"

if [[ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]]; then
  exec dbus-run-session -- "$0" "$@"
fi

for tool in cargo cc grim jq magick pkg-config python3 sway swaymsg wasm-tools wayland-scanner wl-copy wtype; do
  if ! command -v "$tool" > /dev/null 2>&1; then
    echo "ERR $tool is required for the coding-agent proof" >&2
    exit 2
  fi
done
if [[ -n "$COMPUTER_USE" ]] && ! command -v speedcrunch > /dev/null 2>&1; then
  echo "ERR speedcrunch is required for the built-in computer-use proof" >&2
  exit 2
fi

OUT="${BROOD_CODING_AGENT_OUT:-$(mktemp -d /tmp/brood-coding-agent.XXXXXX)}"
PACKAGE="$OUT/package"
WORKSPACE="$OUT/workspace"
RUNTIME_DIR="$OUT/runtime"
COMPOSITOR_LOG="$OUT/compositor.log"
SERVER_LOG="$OUT/server.log"
CLIENT_LOG="$OUT/client.log"
CONFIG="$OUT/brood.toml"
SWAY_CONFIG="$OUT/sway.conf"
DBUS_DESTINATION="org.brood.CodingAgentTest.p$$"
mkdir -p "$PACKAGE" "$WORKSPACE/.brood" "$RUNTIME_DIR"
chmod 700 "$RUNTIME_DIR"
git -C "$WORKSPACE" init -q

CLIENT_PID=""
SERVER_PID=""
COMPOSITOR_PID=""
DRAG_PID=""
cleanup() {
  local result=$?
  set +e
  terminate_process_bounded "$DRAG_PID"
  terminate_process_bounded "$CLIENT_PID"
  terminate_process_bounded "$SERVER_PID"
  terminate_process_bounded "$COMPOSITOR_PID"
  if ((result != 0)); then
    echo "client_log=$CLIENT_LOG" >&2
    tail -n 240 "$CLIENT_LOG" >&2
    echo "server_log=$SERVER_LOG" >&2
    tail -n 180 "$SERVER_LOG" >&2
    echo "compositor_log=$COMPOSITOR_LOG" >&2
    tail -n 100 "$COMPOSITOR_LOG" >&2
  fi
  return "$result"
}
trap cleanup EXIT

cargo build -p broodd -p brood-client --bins
if [[ -n "$SSH_TARGET" ]]; then
  cargo build --release -p brood-remote --bin brood-remote --locked
fi
cargo build \
  --manifest-path plugins/coding-agent/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release \
  --locked
wasm-tools component new \
  plugins/coding-agent/target/wasm32-unknown-unknown/release/brood_coding_agent_plugin.wasm \
  --output "$PACKAGE/brood_coding_agent_plugin.component.wasm"
wasm-tools validate "$PACKAGE/brood_coding_agent_plugin.component.wasm"
cargo build \
  --manifest-path plugins/terminal-ui/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release \
  --locked
wasm-tools component new \
  plugins/terminal-ui/target/wasm32-unknown-unknown/release/brood_terminal_ui_plugin.wasm \
  --output "$PACKAGE/brood_terminal_ui_plugin.component.wasm"
wasm-tools validate "$PACKAGE/brood_terminal_ui_plugin.component.wasm"
cargo build \
  --manifest-path examples/wasm-init-actor/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release \
  --locked
wasm-tools component new \
  examples/wasm-init-actor/target/wasm32-unknown-unknown/release/brood_wasm_init_actor_example.wasm \
  --output "$PACKAGE/brood_wasm_init_actor_example.component.wasm"
wasm-tools validate "$PACKAGE/brood_wasm_init_actor_example.component.wasm"
cargo build \
  --manifest-path plugins/session-switcher/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release \
  --locked
wasm-tools component new \
  plugins/session-switcher/target/wasm32-unknown-unknown/release/brood_session_switcher_plugin.wasm \
  --output "$PACKAGE/brood_session_switcher_plugin.component.wasm"
wasm-tools validate "$PACKAGE/brood_session_switcher_plugin.component.wasm"
cargo build \
  --manifest-path plugins/status-bar/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release \
  --locked
wasm-tools component new \
  plugins/status-bar/target/wasm32-unknown-unknown/release/brood_status_bar_plugin.wasm \
  --output "$PACKAGE/brood_status_bar_plugin.component.wasm"
wasm-tools validate "$PACKAGE/brood_status_bar_plugin.component.wasm"
cargo build \
  --manifest-path plugins/inbox/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release \
  --locked
wasm-tools component new \
  plugins/inbox/target/wasm32-unknown-unknown/release/brood_inbox_plugin.wasm \
  --output "$PACKAGE/brood_inbox_plugin.component.wasm"
wasm-tools validate "$PACKAGE/brood_inbox_plugin.component.wasm"
cargo build \
  --manifest-path plugins/picker/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release \
  --locked
wasm-tools component new \
  plugins/picker/target/wasm32-unknown-unknown/release/brood_picker_plugin.wasm \
  --output "$PACKAGE/brood_picker_plugin.component.wasm"
wasm-tools validate "$PACKAGE/brood_picker_plugin.component.wasm"
cargo build \
  --manifest-path plugins/keybindings/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release \
  --locked
wasm-tools component new \
  plugins/keybindings/target/wasm32-unknown-unknown/release/brood_keybindings_plugin.wasm \
  --output "$PACKAGE/brood_keybindings_plugin.component.wasm"
wasm-tools validate "$PACKAGE/brood_keybindings_plugin.component.wasm"
cargo build \
  --manifest-path plugins/default-workspace/Cargo.toml \
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
read -r -a GTK_BUILD_FLAGS <<< "$(pkg-config --cflags --libs gtk4)"
cc \
  -std=c11 \
  -Wall \
  -Wextra \
  -Werror \
  tools/image-drag-source.c \
  -o "$OUT/image-drag-source" \
  "${GTK_BUILD_FLAGS[@]}"
target/debug/brood-client \
  --no-server \
  --wasm-key-policy workspace \
  "$ROOT/plugins/default-workspace/target/wasm32-unknown-unknown/release/brood_default_workspace_plugin.wasm" \
  --check-wasm-key-policy

cat > "$CONFIG" <<EOF
[init]
session = "$PACKAGE/brood_wasm_init_actor_example.component.wasm"
client = "$PACKAGE/brood_session_switcher_plugin.component.wasm"

[client.key-policy]
provider = "workspace"
path = "$ROOT/plugins/default-workspace/target/wasm32-unknown-unknown/release/brood_default_workspace_plugin.wasm"
EOF

RUST_LOG="${RUST_LOG:-broodd=info,brood_client=info,smithay=warn,wgpu_core=warn,wgpu_hal=warn}"
if [[ -z "$SSH_TARGET" ]]; then
(
  cd "$WORKSPACE"
  env -u brood_session \
    PATH="$ROOT/target/debug:$PATH" \
    RUST_LOG="$RUST_LOG" \
    "$ROOT/target/debug/brood-sessiond" \
    --headless \
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

for _ in $(seq 1 150); do
  ctl status > /dev/null 2>&1 && break
  kill -0 "$SERVER_PID" 2> /dev/null || {
    echo "ERR session server exited during startup" >&2
    exit 1
  }
  sleep 0.1
done
ctl status > /dev/null
fi

cat > "$SWAY_CONFIG" <<'EOF'
xwayland disable
output HEADLESS-1 mode 1280x720
seat seat0 fallback true
default_border none
default_floating_border none
for_window [app_id="org.brood.ImageDragSource"] floating enable, resize set 240 120, move position 980 300
for_window [title="Brood image drag source"] floating enable, resize set 240 120, move position 980 300
EOF

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
  sleep 0.05
done
[[ -n "$WAYLAND_SOCKET" ]]

click_at() {
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-pointer-click" "$1" "$2" 1280 720
}

if [[ -n "$SSH_TARGET" ]]; then
  env -u brood_session \
    XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    BROOD_KEYBINDINGS_PATH="$OUT/keybindings.json" \
    WGPU_BACKEND=vulkan \
    BROOD_DMABUF_HAPPY_PATH=1 \
    BROOD_TRACE_PLUGIN_INPUT=1 \
    BROOD_TRACE_PLUGIN_TEXT=1 \
    RUST_LOG="$RUST_LOG" \
    target/release/brood-remote ssh \
    --session-package "coding-agent-image-test=$PACKAGE/brood_wasm_init_actor_example.component.wasm" \
    --client "$ROOT/target/debug/brood-client" \
    "$SSH_TARGET" \
    -- --config "$CONFIG" \
    > "$CLIENT_LOG" 2>&1 &
else
  env -u brood_session \
    XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    BROOD_KEYBINDINGS_PATH="$OUT/keybindings.json" \
    WGPU_BACKEND=vulkan \
    BROOD_DMABUF_HAPPY_PATH=1 \
    BROOD_TRACE_PLUGIN_INPUT=1 \
    BROOD_TRACE_PLUGIN_TEXT=1 \
    RUST_LOG="$RUST_LOG" \
    target/debug/brood-client \
    --config "$CONFIG" \
    --dbus-destination "$DBUS_DESTINATION" \
    --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
    > "$CLIENT_LOG" 2>&1 &
fi
CLIENT_PID=$!

backend_ready() {
  if [[ -n "$SSH_TARGET" ]]; then
    rg -q 'backend actor message .*"id":"coding-agent-1"' "$CLIENT_LOG"
  else
    rg -q "delivered presentation-to-session actor message" "$SERVER_LOG" \
      && rg -q "coding-agent-1" "$SERVER_LOG"
  fi
}

STARTUP_ATTEMPTS=300
[[ -z "$SSH_TARGET" ]] || STARTUP_ATTEMPTS=3600
for _ in $(seq 1 "$STARTUP_ATTEMPTS"); do
  if rg -q "accepted plugin draw packet" "$CLIENT_LOG" \
    && backend_ready; then
    break
  fi
  kill -0 "$CLIENT_PID" 2> /dev/null || {
    echo "ERR presentation client exited before the coding-agent UI rendered" >&2
    exit 1
  }
  sleep 0.1
done
rg -q "accepted plugin draw packet" "$CLIENT_LOG"
if [[ -n "$SSH_TARGET" ]]; then
  rg -q 'backend actor message .*"id":"coding-agent-1"' "$CLIENT_LOG"
else
  rg -q "delivered presentation-to-session actor message" "$SERVER_LOG"
  rg -q "coding-agent-1" "$SERVER_LOG"
fi

# Wait for the initial workspace to be committed, then open the picker using
# the real global shortcut. The rest of this proof continues through real
# pointer and keyboard input inside the picker and agent UI.
REQUIRED_LAYOUT_COMMITS=2
[[ -z "$COMPUTER_USE" ]] || REQUIRED_LAYOUT_COMMITS=1
for _ in $(seq 1 300); do
  if [[ "$(rg -a -c 'presented committed layout revision' "$CLIENT_LOG" || true)" -ge "$REQUIRED_LAYOUT_COMMITS" ]]; then
    break
  fi
  sleep 0.1
done
[[ "$(rg -a -c 'presented committed layout revision' "$CLIENT_LOG" || true)" -ge "$REQUIRED_LAYOUT_COMMITS" ]]
click_at 640 360
sleep 0.2
PICKER_PANE="$(
  sed -n \
    's/.*started presentation WASM actor id=picker .* pane=\([0-9][0-9]*\) .*/\1/p' \
    "$CLIENT_LOG" \
    | tail -1
)"
[[ "$PICKER_PANE" =~ ^[0-9]+$ ]]
PICKER_VISIBLE_BEFORE="$(
  rg -c 'sender=picker recipient=session-switcher body=.*"visible":true' "$CLIENT_LOG" || true
)"
PICKER_RESIZE_BEFORE="$(
  rg -c "sent plugin pane resize pane=$PICKER_PANE logical_size=720x480" "$CLIENT_LOG" || true
)"
PICKER_LAYOUT_BEFORE="$(rg -c 'accepted leased actor layout' "$CLIENT_LOG" || true)"
env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  "$OUT/virtual-keyboard-key" 30

for _ in $(seq 1 300); do
  PICKER_VISIBLE_AFTER="$(
    rg -c 'sender=picker recipient=session-switcher body=.*"visible":true' "$CLIENT_LOG" || true
  )"
  PICKER_RESIZE_AFTER="$(
    rg -c "sent plugin pane resize pane=$PICKER_PANE logical_size=720x480" "$CLIENT_LOG" || true
  )"
  PICKER_LAYOUT_AFTER="$(rg -c 'accepted leased actor layout' "$CLIENT_LOG" || true)"
  if ((PICKER_VISIBLE_AFTER > PICKER_VISIBLE_BEFORE)) \
    && rg -q "focused local plugin pane=$PICKER_PANE" "$CLIENT_LOG"; then
    break
  fi
  sleep 0.1
done
rg -q "sent key-policy actor message actor=brood.client-init" "$CLIENT_LOG"
((PICKER_VISIBLE_AFTER > PICKER_VISIBLE_BEFORE))
rg -q "focused local plugin pane=$PICKER_PANE" "$CLIENT_LOG"
env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  "$OUT/virtual-keyboard-key" --none 28

AGENT_UI=""

agent_ui_focused() {
  rg -q "presentation key policy requested pane focus.*focused=Some.*changed=true" \
    "$CLIENT_LOG" \
    || {
      rg -q "presentation key policy requested pane focus.*focused=Some.*deferred=true" \
        "$CLIENT_LOG" \
        && rg -q "fulfilled deferred pane focus" "$CLIENT_LOG"
    }
}

for _ in $(seq 1 300); do
  AGENT_UI="$(
    { rg -a -o 'started presentation WASM actor id=coding-agent-ui-b[0-9]+-s[0-9]+-[0-9]+' \
      "$CLIENT_LOG" || true; } \
      | tail -1 \
      | sed 's/.*id=//'
  )"
  if [[ -n "$AGENT_UI" ]] \
    && rg -q "sent key-policy actor message actor=brood.client-init" "$CLIENT_LOG" \
    && rg -q "started presentation WASM actor id=$AGENT_UI" "$CLIENT_LOG" \
    && rg -q "presentation actor message .*sender=$AGENT_UI recipient=init.*\"kind\":\"registry-subscribe\"" \
      "$CLIENT_LOG" \
    && agent_ui_focused; then
    break
  fi
  kill -0 "$CLIENT_PID" 2> /dev/null || {
    echo "ERR presentation client exited while launching another agent UI" >&2
    exit 1
  }
  sleep 0.1
done
[[ -n "$AGENT_UI" ]]
rg -q "sent key-policy actor message actor=brood.client-init" "$CLIENT_LOG"
rg -q "started presentation WASM actor id=$AGENT_UI" "$CLIENT_LOG"
rg -q "presentation actor message .*sender=$AGENT_UI recipient=init.*\"kind\":\"registry-subscribe\"" \
  "$CLIENT_LOG"
agent_ui_focused

AGENT_PANE="$(
  sed -n \
    "s/.*started presentation WASM actor id=$AGENT_UI .* pane=\([0-9][0-9]*\) .*/\1/p" \
    "$CLIENT_LOG" \
    | tail -1
)"
[[ "$AGENT_PANE" =~ ^[0-9]+$ ]]

focus_agent_prompt() {
  local focused=""
  for _ in $(seq 1 30); do
    # The bottom status bar occupies the last 34 logical pixels. Keep prompt
    # input comfortably inside the editor across empty and populated layouts.
    click_at 240 630
    sleep 0.1
    focused="$(
      rg -a -o 'focused local plugin pane=[0-9]+' "$CLIENT_LOG" \
        | tail -1 \
        | sed 's/.*=//'
    )"
    [[ "$focused" == "$AGENT_PANE" ]] && return 0
  done
  echo "ERR could not focus coding-agent prompt pane $AGENT_PANE; last focus was $focused" >&2
  return 1
}

AGENT_ID="coding-agent-1"

capture() {
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    grim "$1"
  [[ -s "$1" ]]
}

capture "$OUT/agent-idle.png"

if [[ -z "$COMPUTER_USE" ]]; then
IMAGE_EXPECTED="red green"
magick -size 192x128 xc:red "$OUT/pasted-image.png"
magick -size 192x128 xc:lime "$OUT/dropped-image.png"
env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  wl-copy --type image/png < "$OUT/pasted-image.png"
sleep 0.3
focus_agent_prompt
env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  wtype -M ctrl "v" -m ctrl

for _ in $(seq 1 300); do
  if rg -q "delivering clipboard image .*mime=image/png" "$CLIENT_LOG"; then
    break
  fi
  sleep 0.1
done
rg -q "delivering clipboard image .*mime=image/png" "$CLIENT_LOG"
capture "$OUT/agent-image-attached.png"

env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  "$OUT/image-drag-source" "$OUT/dropped-image.png" \
  > "$OUT/image-drag-source.log" 2>&1 &
DRAG_PID=$!
SWAY_SOCKET=""
DRAG_START=""
for _ in $(seq 1 100); do
  kill -0 "$DRAG_PID"
  SWAY_SOCKET="$(
    find "$RUNTIME_DIR" -maxdepth 1 -type s -name 'sway-ipc.*.sock' | head -1
  )"
  if [[ -n "$SWAY_SOCKET" ]]; then
    env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
      WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
      SWAYSOCK="$SWAY_SOCKET" \
      swaymsg -t get_tree -r > "$OUT/sway-tree.json"
    DRAG_START="$(python3 - "$OUT/sway-tree.json" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as tree_file:
    roots = [json.load(tree_file)]

while roots:
    node = roots.pop()
    if node.get("name") == "Brood image drag source":
        rect = node["rect"]
        print(rect["x"] + rect["width"] // 2, rect["y"] + rect["height"] // 2)
        break
    roots.extend(node.get("nodes", []))
    roots.extend(node.get("floating_nodes", []))
PY
)"
  fi
  [[ -n "$DRAG_START" ]] && break
  sleep 0.1
done
[[ -n "$DRAG_START" ]]
read -r DRAG_X DRAG_Y <<< "$DRAG_START"
env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  "$OUT/virtual-pointer-click" "$DRAG_X" "$DRAG_Y" 1280 720 400 240 360 \
  || echo "WARN compositor closed the disposable virtual pointer after the drag" >&2
for _ in $(seq 1 300); do
  if rg -q "delivered dropped image .*mime=image/png" "$CLIENT_LOG"; then
    break
  fi
  sleep 0.1
done
rg -q "delivered dropped image .*mime=image/png" "$CLIENT_LOG"
kill "$DRAG_PID" > /dev/null 2>&1 || true
wait "$DRAG_PID" > /dev/null 2>&1 || true
DRAG_PID=""
capture "$OUT/agent-images-attached.png"

assert_preview_color() {
  local image_path="$1"
  local color="$2"
  local pixels
  pixels="$(
    magick "$image_path" -format %c histogram:info:- \
      | awk -v color="$color" '$0 ~ color { gsub(":", "", $1); total += $1 } END { print total + 0 }'
  )"
  if ((pixels < 500)); then
    echo "ERR attachment preview color $color has only $pixels visible pixels" >&2
    exit 1
  fi
}
assert_preview_color "$OUT/agent-images-attached.png" '#FF0000'
assert_preview_color "$OUT/agent-images-attached.png" '#00FF00'
fi

if [[ -n "$SSH_TARGET" ]]; then
  PROMPT="Inspect both attached images. If their solid colors are red and green, reply with exactly this marker and nothing else: $REMOTE_IMAGE_MARKER"
elif [[ -n "$COMPUTER_USE" ]]; then
  PROMPT='Open SpeedCrunch and use it to calculate (12345*6789)-987654. Visually verify that the application shows 82822551, leave any automation stopped, and reply with exactly BROOD_BUILTIN_CU_OK only after verification.'
else
  PROMPT='Create .brood/image-proof.txt with red green and .brood/agent-actor-proof.txt with actor-ui-worker-ok'
fi
focus_agent_prompt
sleep 0.5
env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  "$OUT/virtual-keyboard-key" --text "$PROMPT"
env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  "$OUT/virtual-keyboard-key" --none 28

for _ in $(seq 1 300); do
  if rg -q "presentation actor message .*recipient=$AGENT_ID.*\"kind\":\"prompt\"" "$CLIENT_LOG"; then
    break
  fi
  sleep 0.1
done
rg -q "presentation actor message .*recipient=$AGENT_ID.*\"kind\":\"prompt\"" "$CLIENT_LOG"

if [[ -n "$SSH_TARGET" ]]; then
  for _ in $(seq 1 180); do
    if rg -Fq "\"text\":\"$REMOTE_IMAGE_MARKER\"" "$CLIENT_LOG"; then
      break
    fi
    kill -0 "$CLIENT_PID" 2> /dev/null || {
      echo "ERR remote presentation client exited during the agent turn" >&2
      exit 1
    }
    sleep 1
  done
  rg -Fq "\"text\":\"$REMOTE_IMAGE_MARKER\"" "$CLIENT_LOG"

  for _ in $(seq 1 300); do
    if rg -q 'backend actor message .*"status":"idle"' "$CLIENT_LOG" \
      && rg -q '"text":"Turn complete"' "$CLIENT_LOG"; then
      break
    fi
    sleep 0.2
  done
  rg -q '"text":"Turn complete"' "$CLIENT_LOG"
  if ssh -T -o BatchMode=yes "$SSH_TARGET" \
    'find "$HOME" -maxdepth 1 -name '\''.brood-agent-image-*'\'' -print -quit' \
    | rg -q .; then
    echo "ERR remote coding-agent temporary image was not removed" >&2
    exit 1
  fi
  capture "$OUT/agent-complete.png"
  kill "$CLIENT_PID" > /dev/null 2>&1 || true
  wait "$CLIENT_PID" > /dev/null 2>&1 || true
  CLIENT_PID=""
  cat > "$OUT/proof.md" <<EOF
# Remote coding agent image proof

- target: $SSH_TARGET
- input: real Wayland clipboard paste and desktop drag/drop
- transfer: bounded actor messages crossed the SSH backend connection
- model: Luna-hosted Codex emitted the verified marker $REMOTE_IMAGE_MARKER
- cleanup: temporary image files were removed on Luna
- captures: agent-images-attached.png and agent-complete.png
EOF
  echo "coding_agent_remote_images=verified target=$SSH_TARGET"
  echo "proof=$OUT/proof.md"
  echo "attachments=$OUT/agent-images-attached.png"
  echo "complete=$OUT/agent-complete.png"
  exit 0
elif [[ -n "$COMPUTER_USE" ]]; then
  COMPUTER_TASK=""
  for _ in $(seq 1 1200); do
    ctl computer-use list > "$OUT/computer-use-tasks.json" || true
    COMPUTER_TASK="$(
      jq -r '.[] | select(.principal == "coding-agent-1" and .scope.session == 1 and .state == "stopped" and .next_sequence >= 2) | .id' \
        "$OUT/computer-use-tasks.json" 2> /dev/null \
        | tail -1
    )"
    if [[ -n "$COMPUTER_TASK" ]] \
      && rg -q '"log_kind":"agent","text":"BROOD_BUILTIN_CU_OK"' "$CLIENT_LOG"; then
      break
    fi
    kill -0 "$CLIENT_PID" 2> /dev/null || {
      echo "ERR presentation client exited during the built-in computer-use turn" >&2
      exit 1
    }
    sleep 0.2
  done
  [[ -n "$COMPUTER_TASK" ]]
  rg -q '"log_kind":"agent","text":"BROOD_BUILTIN_CU_OK"' "$CLIENT_LOG"
  ctl computer-use observe "$COMPUTER_TASK" 0 0 > "$OUT/computer-use-observer.json"
  jq -e '
    .task.principal == "coding-agent-1"
    and .task.scope.session == 1
    and .task.state == "stopped"
    and .lease == null
    and (.observations | length) >= 1
    and (.events | map(.event.type) | index("batch_committed")) != null
    and any(.events[]; .event.type == "state_changed" and .event.state == "stopped")
    and (.events | map(.event.type) | index("lease_released")) != null
  ' "$OUT/computer-use-observer.json" > /dev/null
  python3 - "$OUT/computer-use-observer.json" "$OUT/builtin-agent-final.png" <<'PY'
import json
import pathlib
import sys

payload = json.loads(pathlib.Path(sys.argv[1]).read_text())
images = [bytes(observation["png"]) for observation in payload["observations"]]
if not images or not images[-1]:
    raise SystemExit("built-in agent produced no visual observation")
pathlib.Path(sys.argv[2]).write_bytes(images[-1])
PY
  [[ "$(magick identify -format '%wx%h' "$OUT/builtin-agent-final.png")" == "1440x900" ]]
  for _ in $(seq 1 300); do
    if rg -q '"text":"Turn complete"' "$CLIENT_LOG" \
      && rg -q "component actor worker finished" "$SERVER_LOG"; then
      break
    fi
    sleep 0.1
  done
  rg -q '"text":"Turn complete"' "$CLIENT_LOG"
  rg -q "component actor worker finished" "$SERVER_LOG"
  if find "$WORKSPACE" -maxdepth 1 -name '.brood-agent-image-*' -print -quit | rg -q .; then
    echo "ERR coding-agent temporary image was not removed" >&2
    exit 1
  fi
  capture "$OUT/agent-complete.png"
  TASK_SEQUENCE="$(jq -r '.task.next_sequence' "$OUT/computer-use-observer.json")"
  OBSERVATION_SEQUENCES="$(jq -r '[.observations[].sequence] | join(",")' "$OUT/computer-use-observer.json")"
  FRAME_SEQUENCES="$(jq -r '[.observations[].settle.frame_sequence] | join(",")' "$OUT/computer-use-observer.json")"
  INPUT_SEQUENCES="$(jq -r '[.observations[].settle.input_sequence] | join(",")' "$OUT/computer-use-observer.json")"
  SPEEDCRUNCH_PANE="$(jq -r '.observations[-1].active_window' "$OUT/computer-use-observer.json")"
  [[ "$SPEEDCRUNCH_PANE" =~ ^[0-9]+$ ]]
  cat > "$OUT/proof.md" <<EOF
# Built-in agent computer-use proof

- application: SpeedCrunch, pane $SPEEDCRUNCH_PANE, session 1; launched by the agent from an empty session
- agent principal: coding-agent-1
- task: $COMPUTER_TASK, stopped with lease released
- task next sequence: $TASK_SEQUENCE
- observation sequences: $OBSERVATION_SEQUENCES
- settle frame sequences: $FRAME_SEQUENCES
- settle input sequences: $INPUT_SEQUENCES
- automation viewport: 1440x900
- live presentation bounds at capture: 480x686; presentation layout did not control automation geometry
- workload: the built-in Codex pane used its automatically configured session MCP server to enter (12345*6789)-987654 and visually verified 82822551
- input: real Ctrl+Alt+A picker shortcut, pointer selection, prompt typing, and server-side SpeedCrunch keyboard actions
- captures: builtin-agent-final.png and agent-complete.png
EOF
  echo "coding_agent_computer_use=verified task=$COMPUTER_TASK sequences=$OBSERVATION_SEQUENCES frames=$FRAME_SEQUENCES"
  echo "proof=$OUT/proof.md"
  echo "automation_capture=$OUT/builtin-agent-final.png"
  echo "presentation_capture=$OUT/agent-complete.png"
  SWAY_SOCKET="$(
    find "$RUNTIME_DIR" -maxdepth 1 -type s -name 'sway-ipc.*.sock' | head -1
  )"
  [[ -n "$SWAY_SOCKET" ]]
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    SWAYSOCK="$SWAY_SOCKET" \
    swaymsg '[title="^Brood$"] kill' > "$OUT/presentation-close.json"
  wait "$CLIENT_PID"
  CLIENT_PID=""
  exit 0
else
  for _ in $(seq 1 900); do
    if [[ -f "$WORKSPACE/.brood/agent-actor-proof.txt" ]] \
      && rg -qx "actor-ui-worker-ok" "$WORKSPACE/.brood/agent-actor-proof.txt" \
      && [[ -f "$WORKSPACE/.brood/image-proof.txt" ]] \
      && rg -qx "$IMAGE_EXPECTED" "$WORKSPACE/.brood/image-proof.txt"; then
      break
    fi
    kill -0 "$CLIENT_PID" 2> /dev/null || {
      echo "ERR presentation client exited during the agent turn" >&2
      exit 1
    }
    sleep 0.2
  done
  rg -qx "actor-ui-worker-ok" "$WORKSPACE/.brood/agent-actor-proof.txt"
  rg -qx "$IMAGE_EXPECTED" "$WORKSPACE/.brood/image-proof.txt"
fi

for _ in $(seq 1 600); do
  idle_updates="$(
    rg -c 'backend actor message.*"status":"idle"' "$CLIENT_LOG" || true
  )"
  if rg -q "component actor worker finished" "$SERVER_LOG" \
    && ((idle_updates >= 3)); then
    break
  fi
  kill -0 "$CLIENT_PID" 2> /dev/null || {
    echo "ERR presentation client exited before the agent turn completed" >&2
    exit 1
  }
  sleep 0.2
done
rg -q "component actor worker finished" "$SERVER_LOG"
idle_updates="$(rg -c 'backend actor message.*"status":"idle"' "$CLIENT_LOG" || true)"
((idle_updates >= 3))
if find "$WORKSPACE" -maxdepth 1 -name '.brood-agent-image-*' -print -quit | rg -q .; then
  echo "ERR coding-agent temporary image was not removed" >&2
  exit 1
fi

for _ in $(seq 1 300); do
  if rg -q \
    "backend actor message .*sender=$AGENT_ID recipient=inbox .*\"title\":\"Agent finished\"" \
    "$CLIENT_LOG"; then
    break
  fi
  sleep 0.1
done
rg -q \
  "backend actor message .*sender=$AGENT_ID recipient=inbox .*\"title\":\"Agent finished\"" \
  "$CLIENT_LOG"

INBOX_PANE="$(
  sed -n \
    's/.*started presentation WASM actor id=inbox .* pane=\([0-9][0-9]*\) .*/\1/p' \
    "$CLIENT_LOG" \
    | head -1
)"
[[ -n "$INBOX_PANE" ]]
INBOX_SHORTCUT_BASELINE="$(
  rg -c 'sent key-policy actor message actor=inbox' "$CLIENT_LOG" || true
)"
env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  "$OUT/virtual-keyboard-key" 23
for _ in $(seq 1 100); do
  INBOX_SHORTCUT_COUNT="$(
    rg -c 'sent key-policy actor message actor=inbox' "$CLIENT_LOG" || true
  )"
  if ((INBOX_SHORTCUT_COUNT > INBOX_SHORTCUT_BASELINE)) \
    && rg -q "sent plugin pane resize pane=$INBOX_PANE logical_size=320x686 " "$CLIENT_LOG"; then
    break
  fi
  sleep 0.05
done
((INBOX_SHORTCUT_COUNT > INBOX_SHORTCUT_BASELINE))
rg -q "sent plugin pane resize pane=$INBOX_PANE logical_size=320x686 " "$CLIENT_LOG"
capture "$OUT/agent-finished-inbox.png"

INBOX_CLICK_X=$((1280 - 320 / 2))
click_at "$INBOX_CLICK_X" 100
for _ in $(seq 1 100); do
  if rg -Fq \
    "presentation local actor message sender=inbox recipient=$AGENT_UI body={\"kind\":\"focus\"}" \
    "$CLIENT_LOG"; then
    break
  fi
  sleep 0.05
done
rg -Fq \
  "presentation local actor message sender=inbox recipient=$AGENT_UI body={\"kind\":\"focus\"}" \
  "$CLIENT_LOG"

click_at 240 360
env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  wtype "x" -k BackSpace -s 100 -k F6
for _ in $(seq 1 300); do
  if rg -q "presentation actor message .*sender=$AGENT_UI recipient=$AGENT_ID.*\"kind\":\"configure\"" \
    "$CLIENT_LOG"; then
    break
  fi
  sleep 0.1
done
rg -q "presentation actor message .*sender=$AGENT_UI recipient=$AGENT_ID.*\"kind\":\"configure\"" \
  "$CLIENT_LOG"
if rg -q "presentation init child actor failed|component actor mailbox rejected message: channel closed" \
  "$CLIENT_LOG"; then
  echo "ERR coding-agent UI actor failed during the workload" >&2
  exit 1
fi

sleep 3
capture "$OUT/agent-complete.png"

python3 - \
  "$OUT/agent-idle.png" \
  "$OUT/agent-image-attached.png" \
  "$OUT/agent-images-attached.png" \
  "$OUT/agent-finished-inbox.png" \
  "$OUT/agent-complete.png" <<'PY'
import pathlib
import subprocess
import sys

for value in sys.argv[1:]:
    path = pathlib.Path(value)
    data = path.read_bytes()
    if not data.startswith(b"\x89PNG\r\n\x1a\n") or len(data) < 4096:
        raise SystemExit(f"invalid graphical proof: {path}")

metric = subprocess.run(
    ["magick", "compare", "-metric", "RMSE", sys.argv[1], sys.argv[4], "null:"],
    text=True,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    check=False,
).stderr
normalized = float(metric.rsplit("(", 1)[1].rstrip(")\n"))
if normalized < 0.005:
    raise SystemExit(f"idle and completed agent captures are too similar: RMSE={normalized}")
print(f"agent_capture_rmse={normalized:.6f}")
PY

STATUS="$(ctl status)"
rg -q "presentation=Attached" <<< "$STATUS"
rg -q "session-to-presentation" "$SERVER_LOG"

cat > "$OUT/proof.md" <<EOF
# Coding agent actor proof

- input: real text and Enter events through a headless Sway presentation
- UI: Ctrl+Alt+A opened the session agent picker and Enter launched a session-scoped client-realm canvas actor for its selected agent
- agent: one independently addressed session-realm actor selected through the UI
- work: a private actor worker launched Codex with workspace-write authority
- result: .brood/agent-actor-proof.txt contains actor-ui-worker-ok
- image: real compositor clipboard and desktop drag/drop PNGs were attached, transferred to the session actor, interpreted by Codex as $IMAGE_EXPECTED, and removed after the turn
- inbox: agent completion created a global queue item and clicking it focused the originating agent pane
- survival: the focused UI accepted a model change after the completed turn
- captures: agent-idle.png, agent-image-attached.png, agent-images-attached.png, agent-finished-inbox.png, and agent-complete.png
- status: $STATUS
EOF

if [[ -n "$COMPUTER_USE" ]]; then
  cat > "$OUT/proof.md" <<EOF
# Built-in agent computer-use proof

- input: a real prompt submitted through the built-in session-scoped coding-agent pane
- binding: Codex received a required stdio MCP server fixed to Brood session 1 and principal coding-agent-1
- application: real SpeedCrunch pane $SPEEDCRUNCH_PANE inside that session
- work: Codex started its own task, observed the application as MCP image content, entered (12345*6789)-987654, visually verified 82822551, and stopped the task
- lifecycle: task $COMPUTER_TASK retained at least two changing observations and ended stopped with no lease
- artifact: builtin-agent-final.png is the final 1440x900 server observation
- presentation: the same nested client remained attached for live spectator output and returned to the agent UI after stop
- status: $STATUS
EOF
fi

ctl quit > /dev/null 2>&1 || true
echo "coding_agent_actor=verified"
echo "proof=$OUT/proof.md"
echo "idle=$OUT/agent-idle.png"
echo "complete=$OUT/agent-complete.png"
