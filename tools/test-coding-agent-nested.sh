#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"
SSH_TARGET="${BROOD_CODING_AGENT_SSH_TARGET:-}"
COMPUTER_USE="${BROOD_CODING_AGENT_COMPUTER_USE:-}"
CREATE_AGENT_ONLY="${BROOD_CODING_AGENT_CREATE_ONLY:-}"
STYLE_ONLY="${BROOD_CODING_AGENT_STYLE_ONLY:-}"
OFFLINE="${BROOD_CODING_AGENT_OFFLINE:-}"
PURE_RENDERER_ARGS=()
if [[ "${BROOD_CODING_AGENT_PURE_RENDERER:-1}" == 1 ]]; then
  PURE_RENDERER_ARGS=(--features pure-renderer)
else
  PURE_RENDERER_ARGS=(--no-default-features)
fi
REMOTE_IMAGE_MARKER="REMOTE_IMAGE_PROOF: red green"
FIRST_CU_MARKER="BROOD_BUILTIN_CU_FIRST_OK"
SECOND_CU_MARKER="BROOD_BUILTIN_CU_SECOND_OK"

if [[ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]]; then
  exec dbus-run-session -- "$0" "$@"
fi

for tool in cargo cc grim jq magick pkg-config python3 sway swaymsg wasm-tools wayland-scanner wl-copy wtype; do
  if ! command -v "$tool" > /dev/null 2>&1; then
    echo "ERR $tool is required for the coding-agent proof" >&2
    exit 2
  fi
done

# Terminal actor protocol diagnostics can contain NUL bytes. Keep log matching
# in text mode so readiness checks receive actual records and counts.
rg() {
  command rg --text "$@"
}

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
mkdir -p "$PACKAGE" "$WORKSPACE/.brood" "$RUNTIME_DIR" "$OUT/bin"
if [[ -n "$COMPUTER_USE" ]]; then
  mkdir -p "$OUT/xdg-config" "$OUT/xdg-data"
fi
chmod 700 "$RUNTIME_DIR"
git -C "$WORKSPACE" init -q

if [[ -n "$STYLE_ONLY" ]]; then
  cat > "$OUT/bin/brood-agent" << 'EOF'
#!/usr/bin/env python3
import json
import sys

request = json.load(sys.stdin)
events = [
    {"type": "reasoning", "text": "Planning a concise visual response"},
    {"type": "tool_start", "tool": "brood_computer_use__computer_observe"},
    {"type": "tool_end", "tool": "brood_computer_use__computer_observe", "text": "Captured the active application and verified the requested state.", "is_error": False},
    {"type": "assistant", "text": "# Agent pane review\n\nThe pane now treats conversation output as a readable document instead of a stream of clipped status lines. Long sentences wrap naturally at the card edge, so narrow panes remain useful without horizontal scrolling.\n\n## What changed\n\n- **Readable chat cards** keep user prompts, agent replies, and errors visually distinct.\n- Tool activity has a dedicated surface with clear progress and completion states.\n- Search highlights matching cards and moves directly between results.\n\n> The conversation stays scrollable while new output arrives; tail following resumes when you return to the bottom.\n\n```rust\nlet verification = PaneQuality::Polished;\nassert!(verification.is_visible());\n```\n\n## Verification notes\n\nThis intentionally long final paragraph exercises proper word wrapping across several visual lines. It also provides enough transcript height to prove wheel scrolling and the draggable scrollbar without relying on a live provider response or timing-sensitive network activity."},
    {"type": "status", "text": "Visual verification complete"},
    {"type": "result", "history": [{"user": request.get("prompt", ""), "assistant": "Visual verification complete"}]},
]
for event in events:
    print(json.dumps(event), flush=True)
EOF
  chmod +x "$OUT/bin/brood-agent"
fi
if [[ -n "$OFFLINE" && -n "$STYLE_ONLY$CREATE_AGENT_ONLY$COMPUTER_USE$SSH_TARGET" ]]; then
  echo "ERR offline worker verification requires the local full coding-agent lane" >&2
  exit 2
fi
CLIENT_PID=""
SERVER_PID=""
COMPOSITOR_PID=""
DRAG_PID=""
PROVIDER_PID=""
cleanup() {
  local result=$?
  set +e
  if ((result != 0)) && [[ -n "${WAYLAND_SOCKET:-}" ]]; then
    env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
      grim "$OUT/failure.png"
    local failure_sway_socket
    failure_sway_socket="$(find "$RUNTIME_DIR" -maxdepth 1 -type s -name 'sway-ipc.*.sock' | head -1)"
    if [[ -n "$failure_sway_socket" ]]; then
      env SWAYSOCK="$failure_sway_socket" swaymsg -t get_tree -r > "$OUT/failure-tree.json"
    fi
  fi
  terminate_process_bounded "$DRAG_PID"
  terminate_process_bounded "$CLIENT_PID"
  terminate_process_bounded "$SERVER_PID"
  terminate_process_bounded "$COMPOSITOR_PID"
  terminate_process_bounded "$PROVIDER_PID"
  if [[ -n "$OFFLINE" && -d "$OUT/agent-state" ]]; then
    node --input-type=module - "$ROOT" "$OUT/agent-state" << 'JS'
import { readdir, readFile } from 'node:fs/promises';
import { pathToFileURL } from 'node:url';
const [root, directory] = process.argv.slice(2);
const { CodeRepl } = await import(pathToFileURL(`${root}/agents/brood-agent/src/repl.mjs`));
for (const file of await readdir(directory)) {
  if (!file.endsWith('.json')) continue;
  const state = JSON.parse(await readFile(`${directory}/${file}`, 'utf8'));
  await new CodeRepl({id: state.id, workspace: state.workspace, callTool() {}}).run({}, 'close');
}
JS
  fi
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
trap 'echo "ERR coding-agent line=$LINENO command=$BASH_COMMAND callers=${BASH_LINENO[*]}" >&2' ERR

if [[ -n "$OFFLINE" ]]; then
  python3 "$ROOT/tools/coding-agent-provider-fixture.py" "$OUT/provider-port" "$OUT/provider-evidence.json" > "$OUT/provider.log" 2>&1 &
  PROVIDER_PID=$!
  for _ in $(seq 1 100); do
    [[ -s "$OUT/provider-port" ]] && break
    sleep 0.05
  done
  python3 - "$OUT/provider-port" "$OUT/agent.json" << 'PY'
import json, pathlib, sys
port = pathlib.Path(sys.argv[1]).read_text()
pathlib.Path(sys.argv[2]).write_text(json.dumps({"provider": "fixture", "model": "fixture", "providers": {"fixture": {"base_url": f"http://127.0.0.1:{port}/v1"}}}))
PY
fi

# Cargo unlocks a core artifact before componentization. Serialize this build
# and copy phase with other nested gates so feature variants cannot race.
mkdir -p "$ROOT/target"
exec 9> "$ROOT/target/brood-plugin-artifacts.lock"
flock 9
bash "$ROOT/tools/brood-build.sh" --host-only
if [[ -n "$COMPUTER_USE" ]]; then
  cargo build \
    --manifest-path runtimes/computer-use-js/Cargo.toml \
    --target wasm32-wasip1 \
    --release \
    --locked
fi
if [[ -n "$SSH_TARGET" ]]; then
  bash "$ROOT/tools/brood-build.sh" --host-only --release
fi
cargo build \
  --manifest-path plugins/coding-agent/Cargo.toml \
  "${PURE_RENDERER_ARGS[@]}" \
  --target wasm32-unknown-unknown \
  --release \
  --locked
wasm-tools component new \
  plugins/coding-agent/target/wasm32-unknown-unknown/release/brood_coding_agent_plugin.wasm \
  --output "$PACKAGE/brood_coding_agent_plugin.component.wasm"
wasm-tools validate "$PACKAGE/brood_coding_agent_plugin.component.wasm"
cargo build \
  --manifest-path plugins/terminal-ui/Cargo.toml \
  "${PURE_RENDERER_ARGS[@]}" \
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
  "${PURE_RENDERER_ARGS[@]}" \
  --target wasm32-unknown-unknown \
  --release \
  --locked
wasm-tools component new \
  plugins/session-switcher/target/wasm32-unknown-unknown/release/brood_session_switcher_plugin.wasm \
  --output "$PACKAGE/brood_session_switcher_plugin.component.wasm"
wasm-tools validate "$PACKAGE/brood_session_switcher_plugin.component.wasm"
cargo build \
  --manifest-path plugins/theme-switcher/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release \
  --locked
wasm-tools component new \
  plugins/theme-switcher/target/wasm32-unknown-unknown/release/brood_theme_switcher_plugin.wasm \
  --output "$PACKAGE/brood_theme_switcher_plugin.component.wasm"
wasm-tools validate "$PACKAGE/brood_theme_switcher_plugin.component.wasm"
cargo build \
  --manifest-path plugins/status-bar/Cargo.toml \
  "${PURE_RENDERER_ARGS[@]}" \
  --target wasm32-unknown-unknown \
  --release \
  --locked
wasm-tools component new \
  plugins/status-bar/target/wasm32-unknown-unknown/release/brood_status_bar_plugin.wasm \
  --output "$PACKAGE/brood_status_bar_plugin.component.wasm"
wasm-tools validate "$PACKAGE/brood_status_bar_plugin.component.wasm"
cargo build \
  --manifest-path plugins/inbox/Cargo.toml \
  "${PURE_RENDERER_ARGS[@]}" \
  --target wasm32-unknown-unknown \
  --release \
  --locked
wasm-tools component new \
  plugins/inbox/target/wasm32-unknown-unknown/release/brood_inbox_plugin.wasm \
  --output "$PACKAGE/brood_inbox_plugin.component.wasm"
wasm-tools validate "$PACKAGE/brood_inbox_plugin.component.wasm"
cargo build \
  --manifest-path plugins/picker/Cargo.toml \
  "${PURE_RENDERER_ARGS[@]}" \
  --target wasm32-unknown-unknown \
  --release \
  --locked
wasm-tools component new \
  plugins/picker/target/wasm32-unknown-unknown/release/brood_picker_plugin.wasm \
  --output "$PACKAGE/brood_picker_plugin.component.wasm"
wasm-tools validate "$PACKAGE/brood_picker_plugin.component.wasm"
cargo build \
  --manifest-path plugins/keybindings/Cargo.toml \
  "${PURE_RENDERER_ARGS[@]}" \
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
cc \
  -std=c11 \
  -Wall \
  -Wextra \
  -Werror \
  tools/virtual-pointer-input.c \
  -o "$OUT/virtual-pointer-input" \
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

cat > "$CONFIG" << EOF
[init]
session = "$PACKAGE/brood_wasm_init_actor_example.component.wasm"
client = "$PACKAGE/brood_session_switcher_plugin.component.wasm"

[client.key-policy]
provider = "workspace"
path = "$ROOT/plugins/default-workspace/target/wasm32-unknown-unknown/release/brood_default_workspace_plugin.wasm"
EOF

flock -u 9
exec 9>&-

# The assertions below consume these actor lifecycle records even when the
# invoking desktop exports a quiet global RUST_LOG filter.
RUST_LOG="${RUST_LOG:-warn},broodd=info,brood_client=info"
if [[ -z "$SSH_TARGET" ]]; then
  SERVER_PROCESS_ENV=()
  if [[ -n "$OFFLINE" ]]; then
    SERVER_PROCESS_ENV+=("BROOD_AGENT_CONFIG=$OUT/agent.json" "BROOD_AGENT_STATE_DIR=$OUT/agent-state")
  fi
  if [[ -n "$COMPUTER_USE" ]]; then
    SERVER_PROCESS_ENV+=(
      "XDG_CONFIG_HOME=$OUT/xdg-config"
      "XDG_DATA_HOME=$OUT/xdg-data"
    )
  fi
  (
    cd "$WORKSPACE"
    env -u brood_session \
      "${SERVER_PROCESS_ENV[@]}" \
      PATH="$OUT/bin:$ROOT/tools:$ROOT/target/debug:$PATH" \
      BROOD_AGENT_REQUIRE_DBUS_FREE="${COMPUTER_USE:+1}" \
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

cat > "$SWAY_CONFIG" << 'EOF'
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
    BROOD_TRACE_RESIZE=1 \
    WAYLAND_DEBUG="${BROOD_TEST_WAYLAND_DEBUG:-0}" \
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
    BROOD_TRACE_RESIZE=1 \
    WAYLAND_DEBUG="${BROOD_TEST_WAYLAND_DEBUG:-0}" \
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
    rg -q 'backend actor message .*"id":"coding-agent-1"' "$CLIENT_LOG" ||
      {
        rg -q "delivered presentation-to-session actor message" "$SERVER_LOG" &&
          rg -q "coding-agent-1" "$SERVER_LOG"
      }
  fi
}

STARTUP_ATTEMPTS=300
[[ -z "$SSH_TARGET" ]] || STARTUP_ATTEMPTS=3600
for _ in $(seq 1 "$STARTUP_ATTEMPTS"); do
  if rg -q "compiled plugin GPU scene|facet native pair committed" "$CLIENT_LOG" &&
    backend_ready; then
    break
  fi
  kill -0 "$CLIENT_PID" 2> /dev/null || {
    echo "ERR presentation client exited before the coding-agent UI rendered" >&2
    exit 1
  }
  sleep 0.1
done
rg -q "compiled plugin GPU scene|facet native pair committed" "$CLIENT_LOG"
if [[ -n "$SSH_TARGET" ]]; then
  rg -q 'backend actor message .*"id":"coding-agent-1"' "$CLIENT_LOG"
else
  backend_ready
fi

# Wait for the actual initial shell frame, not an earlier quiescent layout.
# Startup publishes the terminal and sidebar in separate native transactions;
# injecting short-lived devices between those transactions can lose their
# capability events while the first GPU scenes are still being prepared.
startup_frame_ready() {
  rg -a 'render-content-projection panes=3 projected_panes=0 ' "$CLIENT_LOG" |
    rg ':960:686:960:686' |
    rg -q ':320:686:320:686'
}
for _ in $(seq 1 300); do
  startup_frame_ready && break
  kill -0 "$CLIENT_PID" 2> /dev/null || break
  sleep 0.1
done
startup_frame_ready
click_at 640 360
sleep 0.2
PICKER_PANE="$(
  sed -n \
    's/.*started presentation WASM actor id=picker .* pane=\([0-9][0-9]*\) .*/\1/p' \
    "$CLIENT_LOG" |
    tail -1
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
  if ((PICKER_VISIBLE_AFTER > PICKER_VISIBLE_BEFORE)) &&
    rg -q "focused local plugin pane=$PICKER_PANE" "$CLIENT_LOG"; then
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
    "$CLIENT_LOG" ||
    {
      rg -q "presentation key policy requested pane focus.*focused=Some.*deferred=true" \
        "$CLIENT_LOG" &&
        rg -q "fulfilled deferred pane focus" "$CLIENT_LOG"
    }
}

for _ in $(seq 1 300); do
  AGENT_UI="$(
    { rg -a -o 'started presentation WASM actor id=coding-agent-ui-b[0-9]+-s[0-9]+-[0-9]+' \
      "$CLIENT_LOG" || true; } |
      tail -1 |
      sed 's/.*id=//'
  )"
  if [[ -n "$AGENT_UI" ]] &&
    rg -q "sent key-policy actor message actor=brood.client-init" "$CLIENT_LOG" &&
    rg -q "started presentation WASM actor id=$AGENT_UI" "$CLIENT_LOG" &&
    rg -q "presentation actor message .*sender=$AGENT_UI recipient=init.*\"kind\":\"registry-subscribe\"" \
      "$CLIENT_LOG" &&
    agent_ui_focused; then
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
    "$CLIENT_LOG" |
    tail -1
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
      rg -a -o 'focused local plugin pane=[0-9]+' "$CLIENT_LOG" |
        tail -1 |
        sed 's/.*=//'
    )"
    [[ "$focused" == "$AGENT_PANE" ]] && return 0
  done
  echo "ERR could not focus coding-agent prompt pane $AGENT_PANE; last focus was $focused" >&2
  return 1
}

submit_agent_prompt() {
  local prompt="$1"
  local prompt_count_before
  prompt_count_before="$({ rg -a -c "presentation actor message .*recipient=$AGENT_ID.*\"kind\":\"prompt\"" "$CLIENT_LOG" || true; } | tail -1)"
  prompt_count_before="${prompt_count_before:-0}"
  focus_agent_prompt
  sleep 0.5
  local offset chunk
  for ((offset = 0; offset < ${#prompt}; offset += 24)); do
    chunk="${prompt:offset:24}"
    env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
      WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
      "$OUT/virtual-keyboard-key" --text "$chunk"
    sleep 0.03
  done
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-keyboard-key" --none 28
  for _ in $(seq 1 300); do
    local prompt_count
    prompt_count="$({ rg -a -c "presentation actor message .*recipient=$AGENT_ID.*\"kind\":\"prompt\"" "$CLIENT_LOG" || true; } | tail -1)"
    prompt_count="${prompt_count:-0}"
    ((prompt_count > prompt_count_before)) && return 0
    sleep 0.1
  done
  echo "ERR coding-agent prompt was not submitted" >&2
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

if [[ -n "$STYLE_ONLY" ]]; then
  focus_agent_prompt
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-keyboard-key" --text "Review the agent pane visual desgn"
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-keyboard-key" --none 105
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-keyboard-key" --none 105
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-keyboard-key" --text "i"
  sleep 0.2
  capture "$OUT/agent-editor.png"
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-keyboard-key" --none 28
  for _ in $(seq 1 300); do
    if rg -q '"text":"Visual verification complete"' "$CLIENT_LOG" &&
      rg -q '"log_kind":"tool"' "$CLIENT_LOG"; then
      break
    fi
    kill -0 "$CLIENT_PID"
    sleep 0.1
  done
  rg -q '"text":"Visual verification complete"' "$CLIENT_LOG"
  rg -q '"log_kind":"tool"' "$CLIENT_LOG"
  rg -q '"prompt":"Review the agent pane visual design"' "$CLIENT_LOG"
  # Text shaping and glyph upload are asynchronous. Wait for the completed
  # transcript replacement to reach the renderer before judging its pixels.
  sleep 1
  capture "$OUT/agent-markdown.png"

  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-pointer-input" scroll 240 330 -3 12 1280 720
  sleep 0.3
  capture "$OUT/agent-scrolled.png"

  AGENT_WIDTH="$(
    sed -n \
      "s/.*accepted plugin draw packet pane=$AGENT_PANE .*gpu_target=\([0-9][0-9]*\)x.*/\1/p" \
      "$CLIENT_LOG" |
      tail -1
  )"
  [[ "$AGENT_WIDTH" =~ ^[0-9]+$ ]]
  click_at "$((AGENT_WIDTH - 60))" 20
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-keyboard-key" --text "verification"
  sleep 0.3
  capture "$OUT/agent-search.png"
  if magick compare -metric RMSE "$OUT/agent-scrolled.png" "$OUT/agent-search.png" null: \
    2> "$OUT/search-rmse.txt"; then
    echo "ERR search did not change the rendered pane" >&2
    exit 1
  fi

  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-keyboard-key" --none 1
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-keyboard-key" --none 60
  for _ in $(seq 1 300); do
    rg -q 'backend actor message .*"id":"coding-agent-2"' "$CLIENT_LOG" && break
    sleep 0.1
  done
  rg -q 'backend actor message .*"id":"coding-agent-2"' "$CLIENT_LOG"
  capture "$OUT/agent-tabs-before-drag.png"
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-pointer-input" drag 52 20 244 20 16 1280 720
  sleep 0.3
  capture "$OUT/agent-tabs-after-drag.png"

  if magick compare -metric RMSE "$OUT/agent-markdown.png" "$OUT/agent-scrolled.png" null: \
    2> "$OUT/scroll-rmse.txt"; then
    echo "ERR scrolling did not change the rendered transcript" >&2
    exit 1
  fi
  if magick compare -metric RMSE "$OUT/agent-tabs-before-drag.png" "$OUT/agent-tabs-after-drag.png" null: \
    2> "$OUT/tab-drag-rmse.txt"; then
    echo "ERR dragging did not change the rendered agent tabs" >&2
    exit 1
  fi
  kill -0 "$CLIENT_PID"
  # Exercise image delivery through the isolated interpreter without a provider.
  magick -size 192x128 xc:red "$OUT/style-pasted-image.png"
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    wl-copy --type image/png < "$OUT/style-pasted-image.png"
  sleep 0.2
  focus_agent_prompt
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-keyboard-key" --ctrl-only 47
  IMAGE_PIXELS=0
  for _ in $(seq 1 100); do
    capture "$OUT/agent-style-image.png"
    IMAGE_PIXELS="$(magick "$OUT/agent-style-image.png" -format %c histogram:info:- | awk '/#FF0000/ { gsub(":", "", $1); total += $1 } END { print total + 0 }')"
    ((IMAGE_PIXELS >= 500)) && break
    kill -0 "$CLIENT_PID"
    sleep 0.1
  done
  ((IMAGE_PIXELS >= 500)) || {
    echo "ERR clipboard image did not reach the rendered preview" >&2
    exit 1
  }
  if rg -a -q 'visual facet .*failed|facet invocation failed' "$CLIENT_LOG"; then
    echo "ERR a facet failed during the coding-agent workload" >&2
    exit 1
  fi
  cat > "$OUT/proof.md" << EOF
# Coding agent pane style proof

- input: real Wayland text editing, cursor navigation, shortcut, wheel, and pointer-drag events
- output: Markdown headings, list, quote, code block, status, and tool events crossed the normal agent stream
- interaction: transcript scrolling, Ctrl+F search, and tab drag each changed the rendered pane
- attachment: a real clipboard PNG crossed the input facet and rendered a red preview with at least 500 pixels
- stability: the presentation client remained alive throughout the workload; no facet failure was logged
- captures: agent-idle.png, agent-editor.png, agent-markdown.png, agent-scrolled.png, agent-search.png, agent-tabs-before-drag.png, and agent-tabs-after-drag.png
EOF
  echo "coding_agent_style=verified"
  echo "proof=$OUT/proof.md"
  echo "markdown=$OUT/agent-markdown.png"
  echo "search=$OUT/agent-search.png"
  echo "tabs=$OUT/agent-tabs-after-drag.png"
  exit 0
fi

if [[ -n "$CREATE_AGENT_ONLY" ]]; then
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-keyboard-key" --none 60
  for _ in $(seq 1 300); do
    if rg -q 'backend actor message .*"id":"coding-agent-2"' "$CLIENT_LOG" &&
      rg -q 'backend actor message .*"agent":"coding-agent-2".*"status":"idle"' \
        "$CLIENT_LOG"; then
      break
    fi
    kill -0 "$CLIENT_PID" 2> /dev/null || {
      echo "ERR presentation client exited while creating coding-agent-2" >&2
      exit 1
    }
    sleep 0.1
  done
  rg -q 'backend actor message .*"id":"coding-agent-2"' "$CLIENT_LOG"
  rg -q 'backend actor message .*"agent":"coding-agent-2".*"status":"idle"' "$CLIENT_LOG"
  kill -0 "$CLIENT_PID"
  capture "$OUT/agent-created.png"

  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-keyboard-key" --none 15
  sleep 0.5
  kill -0 "$CLIENT_PID"
  capture "$OUT/agent-selected.png"
  if magick compare -metric RMSE "$OUT/agent-created.png" "$OUT/agent-selected.png" null: \
    2> "$OUT/agent-selection-rmse.txt"; then
    echo "ERR selecting coding-agent-2 did not change the rendered pane" >&2
    exit 1
  fi

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
  cat > "$OUT/proof.md" << EOF
# Coding agent creation proof

- input: real F2 key release in the focused coding-agent pane
- actor: coding-agent-2 started in session 1 and published idle state
- UI: the registry rendered both agent tabs and Tab selected coding-agent-2
- stability: the presentation client remained alive through creation, selection, and capture
- captures: agent-created.png and agent-selected.png
EOF
  echo "coding_agent_create=verified"
  echo "proof=$OUT/proof.md"
  echo "created=$OUT/agent-created.png"
  echo "selected=$OUT/agent-selected.png"
  exit 0
fi

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

  env GDK_BACKEND=wayland GTK_A11Y=none NO_AT_BRIDGE=1 \
    XDG_RUNTIME_DIR="$RUNTIME_DIR" \
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
      DRAG_START="$(
        python3 - "$OUT/sway-tree.json" << 'PY'
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
    "$OUT/virtual-pointer-click" "$DRAG_X" "$DRAG_Y" 1280 720 400 240 360 ||
    echo "WARN compositor closed the disposable virtual pointer after the drag" >&2
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
      magick "$image_path" -format %c histogram:info:- |
        awk -v color="$color" '$0 ~ color { gsub(":", "", $1); total += $1 } END { print total + 0 }'
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
  PROMPT="Open SpeedCrunch and use it to calculate (12345*6789)-987654. Visually verify that the application shows 82822551, leave any automation stopped, and reply with exactly $FIRST_CU_MARKER only after verification."
else
  PROMPT='Create .brood/image-proof.txt with red green and .brood/agent-actor-proof.txt with actor-ui-worker-ok'
fi
submit_agent_prompt "$PROMPT"

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
    if rg -q 'backend actor message .*"status":"idle"' "$CLIENT_LOG" &&
      rg -q '"text":"Turn complete"' "$CLIENT_LOG"; then
      break
    fi
    sleep 0.2
  done
  rg -q '"text":"Turn complete"' "$CLIENT_LOG"
  if ssh -T -o BatchMode=yes "$SSH_TARGET" \
    'find "$HOME" -maxdepth 1 -name '\''.brood-agent-image-*'\'' -print -quit' |
    rg -q .; then
    echo "ERR remote coding-agent temporary image was not removed" >&2
    exit 1
  fi
  capture "$OUT/agent-complete.png"
  kill "$CLIENT_PID" > /dev/null 2>&1 || true
  wait "$CLIENT_PID" > /dev/null 2>&1 || true
  CLIENT_PID=""
  cat > "$OUT/proof.md" << EOF
# Remote coding agent image proof

- target: $SSH_TARGET
- input: real Wayland clipboard paste and desktop drag/drop
- transfer: bounded actor messages crossed the SSH backend connection
- model: the remote Brood agent emitted the verified marker $REMOTE_IMAGE_MARKER
- cleanup: temporary image files were removed on Luna
- captures: agent-images-attached.png and agent-complete.png
EOF
  echo "coding_agent_remote_images=verified target=$SSH_TARGET"
  echo "proof=$OUT/proof.md"
  echo "attachments=$OUT/agent-images-attached.png"
  echo "complete=$OUT/agent-complete.png"
  exit 0
elif [[ -n "$COMPUTER_USE" ]]; then
  wait_for_agent_computer_task() {
    local marker="$1"
    local excluded_task="$2"
    local task_list="$3"
    local task=""
    for _ in $(seq 1 1200); do
      ctl computer-use list > "$task_list" || true
      task="$(
        jq -r --arg excluded "$excluded_task" '
          .[]
          | select(
              .principal == "coding-agent-1"
              and .scope.session == 1
              and .state == "stopped"
              and .next_sequence >= 2
              and ($excluded == "" or .id != $excluded)
            )
          | .id
        ' "$task_list" 2> /dev/null |
          tail -1
      )"
      if [[ -n "$task" ]] &&
        rg -a -Fq "\"log_kind\":\"agent\",\"text\":\"$marker\"" "$CLIENT_LOG"; then
        printf '%s\n' "$task"
        return 0
      fi
      kill -0 "$CLIENT_PID" 2> /dev/null || {
        echo "ERR presentation client exited during the built-in computer-use turn" >&2
        return 1
      }
      sleep 0.2
    done
    echo "ERR timed out waiting for stopped agent computer-use task and marker $marker" >&2
    return 1
  }

  wait_for_agent_idle() {
    for _ in $(seq 1 300); do
      if { rg -a 'backend actor message .*"agent":"coding-agent-1".*"status":' "$CLIENT_LOG" || true; } |
        tail -1 |
        rg -q '"status":"idle"'; then
        return 0
      fi
      sleep 0.1
    done
    echo "ERR coding-agent did not return to idle" >&2
    return 1
  }

  extract_last_observation() {
    local observer="$1"
    local image="$2"
    python3 - "$observer" "$image" << 'PY'
import json
import pathlib
import sys

payload = json.loads(pathlib.Path(sys.argv[1]).read_text())
images = [bytes(observation["png"]) for observation in payload["observations"]]
if not images or not images[-1]:
    raise SystemExit("built-in agent produced no visual observation")
pathlib.Path(sys.argv[2]).write_bytes(images[-1])
PY
    [[ "$(magick identify -format '%wx%h' "$image")" == "1440x900" ]]
  }

  observe_agent_task() {
    local task="$1"
    local observer="$2"
    local image="$3"
    ctl computer-use observe "$task" 0 0 > "$observer"
    jq -e '
      .task.principal == "coding-agent-1"
      and .task.scope.session == 1
      and .task.state == "stopped"
      and .lease == null
      and (.observations | length) >= 1
      and (.events | map(.event.type) | index("batch_committed")) != null
      and any(.events[]; .event.type == "state_changed" and .event.state == "stopped")
      and (.events | map(.event.type) | index("lease_released")) != null
    ' "$observer" > /dev/null
    extract_last_observation "$observer" "$image"
  }

  assert_single_speedcrunch_window() {
    local output="$1"
    ctl computer-use windows 1 > "$output"
    jq -e '
      .session == 1
      and (
        [.windows[]
          | select(
              ((.application_id // "") | ascii_downcase | contains("speedcrunch"))
              or ((.title // "") | ascii_downcase | contains("speedcrunch"))
            )]
        | length
      ) == 1
      and all(.windows[]; ((.title // "") | ascii_downcase | contains("load session") | not))
    ' "$output" > /dev/null
  }

  FIRST_COMPUTER_TASK="$(wait_for_agent_computer_task \
    "$FIRST_CU_MARKER" "" "$OUT/first-computer-use-tasks.json")"
  observe_agent_task "$FIRST_COMPUTER_TASK" \
    "$OUT/first-computer-use-observer.json" "$OUT/builtin-agent-first.png"
  FIRST_SPEEDCRUNCH_PANE="$(jq -r '.observations[-1].active_window' \
    "$OUT/first-computer-use-observer.json")"
  [[ "$FIRST_SPEEDCRUNCH_PANE" =~ ^[0-9]+$ ]]
  assert_single_speedcrunch_window "$OUT/first-computer-windows.json"
  wait_for_agent_idle

  SECOND_PROMPT="Use the existing SpeedCrunch window from the previous calculation to calculate 25*25. Visually verify that the application shows 625, do not open another application window, leave any automation stopped, and reply with exactly $SECOND_CU_MARKER only after verification."
  submit_agent_prompt "$SECOND_PROMPT"
  SECOND_COMPUTER_TASK="$(wait_for_agent_computer_task \
    "$SECOND_CU_MARKER" "$FIRST_COMPUTER_TASK" "$OUT/second-computer-use-tasks.json")"
  observe_agent_task "$SECOND_COMPUTER_TASK" \
    "$OUT/second-computer-use-observer.json" "$OUT/builtin-agent-second.png"
  SECOND_SPEEDCRUNCH_PANE="$(jq -r '.observations[-1].active_window' \
    "$OUT/second-computer-use-observer.json")"
  [[ "$SECOND_SPEEDCRUNCH_PANE" == "$FIRST_SPEEDCRUNCH_PANE" ]]
  assert_single_speedcrunch_window "$OUT/second-computer-windows.json"
  wait_for_agent_idle

  python3 - "$OUT/first-computer-use-observer.json" "$OUT/builtin-agent-first-initial.png" \
    "$OUT/second-computer-use-observer.json" "$OUT/builtin-agent-second-initial.png" << 'PY'
import json
import pathlib
import sys

for observer, image in ((sys.argv[1], sys.argv[2]), (sys.argv[3], sys.argv[4])):
    payload = json.loads(pathlib.Path(observer).read_text())
    first = bytes(payload["observations"][0]["png"])
    if not first:
        raise SystemExit(f"{observer} has no initial visual observation")
    pathlib.Path(image).write_bytes(first)
PY
  [[ "$(sha256sum "$OUT/builtin-agent-first-initial.png" | cut -d' ' -f1)" != "$(sha256sum "$OUT/builtin-agent-first.png" | cut -d' ' -f1)" ]]
  [[ "$(sha256sum "$OUT/builtin-agent-second-initial.png" | cut -d' ' -f1)" != "$(sha256sum "$OUT/builtin-agent-second.png" | cut -d' ' -f1)" ]]
  [[ "$(sha256sum "$OUT/builtin-agent-first.png" | cut -d' ' -f1)" != "$(sha256sum "$OUT/builtin-agent-second.png" | cut -d' ' -f1)" ]]

  if find "$WORKSPACE" -maxdepth 1 -name '.brood-agent-image-*' -print -quit | rg -q .; then
    echo "ERR coding-agent temporary image was not removed" >&2
    exit 1
  fi
  capture "$OUT/agent-complete.png"
  FIRST_TASK_SEQUENCE="$(jq -r '.task.next_sequence' "$OUT/first-computer-use-observer.json")"
  SECOND_TASK_SEQUENCE="$(jq -r '.task.next_sequence' "$OUT/second-computer-use-observer.json")"
  FIRST_OBSERVATION_SEQUENCES="$(jq -r '[.observations[].sequence] | join(",")' "$OUT/first-computer-use-observer.json")"
  SECOND_OBSERVATION_SEQUENCES="$(jq -r '[.observations[].sequence] | join(",")' "$OUT/second-computer-use-observer.json")"
  cat > "$OUT/proof.md" << EOF
# Built-in agent sequential computer-use proof

- application: one SpeedCrunch window, pane $FIRST_SPEEDCRUNCH_PANE, session 1; launched by the agent from an empty session and reused for the second turn
- agent principal: coding-agent-1
- first task: $FIRST_COMPUTER_TASK, stopped with lease released; sequence $FIRST_TASK_SEQUENCE; observations $FIRST_OBSERVATION_SEQUENCES
- second task: $SECOND_COMPUTER_TASK, stopped with lease released; sequence $SECOND_TASK_SEQUENCE; observations $SECOND_OBSERVATION_SEQUENCES
- automation viewport: 1440x900
- live presentation bounds at capture: 480x686; presentation layout did not control automation geometry
- first workload: the independent built-in agent entered (12345*6789)-987654 and visually verified 82822551
- second workload: the same agent conversation entered 25*25 in the existing window and visually verified 625
- regression invariants: each turn visibly changed SpeedCrunch, the two final frames differ, both tasks stopped with their leases released, and exactly one SpeedCrunch window remained with no unexpected dialog
- input: real Ctrl+Alt+A picker shortcut, pointer selection, prompt typing, and server-side SpeedCrunch keyboard actions
- captures: builtin-agent-first.png, builtin-agent-second.png, both initial frames, and agent-complete.png
EOF
  echo "coding_agent_computer_use=verified first_task=$FIRST_COMPUTER_TASK second_task=$SECOND_COMPUTER_TASK pane=$FIRST_SPEEDCRUNCH_PANE"
  echo "proof=$OUT/proof.md"
  echo "first_automation_capture=$OUT/builtin-agent-first.png"
  echo "second_automation_capture=$OUT/builtin-agent-second.png"
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
    if rg -q "component actor worker failed" "$SERVER_LOG"; then
      echo "ERR coding-agent worker failed; inspect $CLIENT_LOG and $SERVER_LOG" >&2
      exit 1
    fi
    if [[ -f "$WORKSPACE/.brood/agent-actor-proof.txt" ]] &&
      rg -qx "actor-ui-worker-ok" "$WORKSPACE/.brood/agent-actor-proof.txt" &&
      [[ -f "$WORKSPACE/.brood/image-proof.txt" ]] &&
      rg -qx "$IMAGE_EXPECTED" "$WORKSPACE/.brood/image-proof.txt"; then
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
  if rg -q "component actor worker finished" "$SERVER_LOG" &&
    ((idle_updates >= 3)); then
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
if [[ -n "$OFFLINE" ]]; then
  submit_reliability_prompt() {
    local baseline
    baseline="$(rg -c 'component actor worker finished' "$SERVER_LOG" || true)"
    submit_agent_prompt "$1"
    for _ in $(seq 1 300); do
      local current
      current="$(rg -c 'component actor worker finished' "$SERVER_LOG" || true)"
      ((current > baseline)) && return 0
      if rg -q 'component actor worker failed' "$SERVER_LOG"; then
        echo "ERR reliability turn failed" >&2
        return 1
      fi
      sleep 0.1
    done
    echo "ERR reliability turn did not finish" >&2
    return 1
  }
  submit_reliability_prompt /compact
  submit_reliability_prompt "Verify durable agent state"
  rg -qx 'durable-repl-compaction-ok' "$WORKSPACE/.brood/agent-resume-proof.txt"
  jq -e '.compactions == 1 and .resumed == true and .image_colors == ["green", "red"]' "$OUT/provider-evidence.json" > /dev/null
  capture "$OUT/agent-resumed.png"
  submit_agent_prompt "Verify cancellation"
  for _ in $(seq 1 100); do
    [[ -f "$WORKSPACE/.brood/cancel-started" ]] && break
    sleep 0.05
  done
  [[ -f "$WORKSPACE/.brood/cancel-started" ]]
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-keyboard-key" --none 1
  for _ in $(seq 1 100); do
    rg -q 'Turn cancelled' "$CLIENT_LOG" && break
    sleep 0.05
  done
  rg -q 'Turn cancelled' "$CLIENT_LOG"
  sleep 3.2
  [[ ! -e "$WORKSPACE/.brood/cancel-leaked" ]]
  submit_reliability_prompt "Verify cancellation recovery"
  rg -qx 'cancellation-recovery-ok' "$WORKSPACE/.brood/cancel-recovered"
  jq -e '.cancellation_recovered == true' "$OUT/provider-evidence.json" > /dev/null
  capture "$OUT/agent-cancel-recovered.png"
fi
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
    "$CLIENT_LOG" |
    head -1
)"
[[ -n "$INBOX_PANE" ]]
inbox_frame_ready() {
  rg -q "render-content-projection .*${INBOX_PANE}:[0-9]+:320:686:320:686" "$CLIENT_LOG"
}
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
  if ((INBOX_SHORTCUT_COUNT > INBOX_SHORTCUT_BASELINE)) &&
    inbox_frame_ready; then
    break
  fi
  sleep 0.05
done
((INBOX_SHORTCUT_COUNT > INBOX_SHORTCUT_BASELINE))
inbox_frame_ready
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
  "$OUT/agent-complete.png" << 'PY'
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

WORKER_DESCRIPTION="the live headless Brood agent"
[[ -z "$OFFLINE" ]] || WORKER_DESCRIPTION="the real Brood agent with a local Responses fixture, persistent REPL, and compaction across turns"
cat > "$OUT/proof.md" << EOF
# Coding agent actor proof

- input: real text and Enter events through a headless Sway presentation
- UI: Ctrl+Alt+A opened the session agent picker and Enter launched a session-scoped client-realm canvas actor for its selected agent
- agent: one independently addressed session-realm actor selected through the UI
- work: a private actor worker launched $WORKER_DESCRIPTION in the test workspace
- result: .brood/agent-actor-proof.txt contains actor-ui-worker-ok
- image: real compositor clipboard and desktop drag/drop PNGs were attached, transferred to the session actor, validated by $WORKER_DESCRIPTION as $IMAGE_EXPECTED, and removed after the turn
- inbox: agent completion created a global queue item and clicking it focused the originating agent pane
- survival: the focused UI accepted a model change after the completed turn
- captures: agent-idle.png, agent-image-attached.png, agent-images-attached.png, agent-finished-inbox.png, and agent-complete.png
- status: $STATUS
EOF

if [[ -n "$COMPUTER_USE" ]]; then
  cat > "$OUT/proof.md" << EOF
# Built-in agent computer-use proof

- input: a real prompt submitted through the built-in session-scoped coding-agent pane
- binding: the Brood agent received native computer tools fixed to Brood session 1 and principal coding-agent-1, with no DBus or MCP transport
- application: real SpeedCrunch pane $SPEEDCRUNCH_PANE inside that session
- work: the Brood agent started its own task, observed the application as native image input, entered (12345*6789)-987654, visually verified 82822551, and stopped the task
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
