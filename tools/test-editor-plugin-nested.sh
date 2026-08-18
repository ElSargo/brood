#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

# The editor proof owns its bus and compositor. Never attach its backend,
# presentation client, or local Neovim runtime to the caller's desktop.
if [[ "${BROOD_EDITOR_PRIVATE_DBUS:-0}" != 1 ]]; then
  exec env \
    -u brood_session \
    -u BROOD_SURFACE_SESSION_ID \
    -u BROOD_PANE_ID \
    -u DBUS_SESSION_BUS_ADDRESS \
    BROOD_EDITOR_PRIVATE_DBUS=1 \
    dbus-run-session -- "$0" "$@"
fi

for tool in cc git grim jq magick nix pkg-config ps python3 rg sway wtype; do
  if ! command -v "$tool" > /dev/null 2>&1; then
    echo "ERR $tool is required for the editor plugin proof" >&2
    exit 2
  fi
done

OUT="${BROOD_EDITOR_OUT:-$(mktemp -d /tmp/brood-editor.XXXXXX)}"
PACKAGE_LINK="$OUT/package-result.$BASHPID"
RUNTIME_DIR="$(mktemp -d /tmp/brood-editor-runtime.XXXXXX)"
WORKSPACE="$(mktemp -d /tmp/brood-editor-workspace.XXXXXX)"
SERVER_LOG="$OUT/server.log"
CLIENT_LOG="$OUT/client.log"
COMPOSITOR_LOG="$OUT/compositor.log"
SWAY_CONFIG="$OUT/sway.conf"
DATA_HOME="$OUT/data"
CONFIG_HOME="$OUT/config"
DBUS_DESTINATION="org.brood.EditorTest.p$BASHPID"
EDIT_FILE="$WORKSPACE/tracked.txt"
INBOUND_FILE="$WORKSPACE/inbound.txt"
LARGE_FILE="$WORKSPACE/large.txt"
TERMINAL_MARKER="$WORKSPACE/terminal-command-routed"
HOST_CONFIG_MARKER="brood-host-config-$BASHPID"
HOST_PLUGIN_LINE="BROOD_HOST_LUA_PLUGIN_LOADED_$BASHPID"
HOST_CONFIG_DIR="$CONFIG_HOME/nvim"
HOST_PLUGIN_DIR="$HOST_CONFIG_DIR/lua/brood_test_plugin"
mkdir -p "$OUT" "$DATA_HOME" "$HOST_PLUGIN_DIR"
chmod 700 "$RUNTIME_DIR"

# This is the client's host Neovim configuration. A unique user command is
# defined in a pure-Lua plugin under XDG_CONFIG_HOME/nvim/lua, so bytes written
# by that command prove that the local Wasm Neovim loaded host configuration
# and plugin code. A log line or package-presence assertion alone would not
# prove this.
printf '%s\n' \
  'local M = {}' \
  'function M.install()' \
  '  vim.api.nvim_create_user_command("BroodHostPluginProof", function()' \
  "    vim.api.nvim_put({ \"$HOST_PLUGIN_LINE\" }, \"l\", true, true)" \
  '  end, { desc = "Brood host Lua plugin proof" })' \
  'end' \
  'return M' \
  > "$HOST_PLUGIN_DIR/init.lua"
printf '%s\n' \
  "vim.g.brood_config_marker = \"$HOST_CONFIG_MARKER:\" .. (type(vim.brood) == \"table\" and \"bridge\" or \"no-bridge\")" \
  'require("brood_test_plugin").install()' \
  > "$HOST_CONFIG_DIR/init.lua"

git -C "$WORKSPACE" init --quiet
printf '%s\n' \
  'tracked editor baseline' \
  > "$EDIT_FILE"
printf '%s\n' \
  'first inbound line' \
  'second inbound line' \
  'third inbound line' \
  > "$INBOUND_FILE"
python3 - "$LARGE_FILE" <<'PY'
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
with path.open("w", encoding="utf-8", newline="\n") as output:
    for index in range(1400):
        output.write(f"stream-{index:04d} {'x' * 72}\n")
PY
git -C "$WORKSPACE" add tracked.txt inbound.txt large.txt
git -C "$WORKSPACE" \
  -c user.name=Brood \
  -c user.email=brood@example.invalid \
  commit --quiet -m baseline

CLIENT_PID=""
SERVER_PID=""
COMPOSITOR_PID=""
cleanup() {
  local result=$?
  set +e
  terminate_process_bounded "$CLIENT_PID"
  terminate_process_bounded "$SERVER_PID"
  terminate_process_bounded "$COMPOSITOR_PID"
  mkdir -p "$OUT/fixtures"
  [[ -f "$EDIT_FILE" ]] && cp "$EDIT_FILE" "$OUT/fixtures/tracked.txt"
  [[ -f "$INBOUND_FILE" ]] && cp "$INBOUND_FILE" "$OUT/fixtures/inbound.txt"
  [[ -f "$LARGE_FILE" ]] && cp "$LARGE_FILE" "$OUT/fixtures/large.txt"
  [[ -f "$TERMINAL_MARKER" ]] \
    && cp "$TERMINAL_MARKER" "$OUT/fixtures/terminal-command-routed"
  [[ -d "$HOST_CONFIG_DIR" && ! -e "$OUT/fixtures/host-nvim-config" ]] \
    && cp -R "$HOST_CONFIG_DIR" "$OUT/fixtures/host-nvim-config"
  rm -rf -- "$RUNTIME_DIR" "$WORKSPACE"
  if ((result != 0)); then
    echo "client_log=$CLIENT_LOG" >&2
    [[ -f "$CLIENT_LOG" ]] && tail -n 260 "$CLIENT_LOG" >&2
    echo "server_log=$SERVER_LOG" >&2
    [[ -f "$SERVER_LOG" ]] && tail -n 220 "$SERVER_LOG" >&2
    echo "compositor_log=$COMPOSITOR_LOG" >&2
    [[ -f "$COMPOSITOR_LOG" ]] && tail -n 100 "$COMPOSITOR_LOG" >&2
  fi
  return "$result"
}
trap 'echo "ERR command=$BASH_COMMAND line=$LINENO" >&2' ERR
trap cleanup EXIT

if [[ -n "${BROOD_EDITOR_PACKAGE:-}" ]]; then
  PACKAGE="$(readlink -f "$BROOD_EDITOR_PACKAGE")"
else
  nix build .#brood -o "$PACKAGE_LINK"
  PACKAGE="$(readlink -f "$PACKAGE_LINK")"
fi
[[ -x "$PACKAGE/bin/brood" ]]
[[ -x "$PACKAGE/bin/brood-client" ]]
[[ -x "$PACKAGE/bin/brood-sessiond" ]]
[[ -s "$PACKAGE/brood.toml" ]]

CLIENT_PACKAGE="$PACKAGE/packages/org.brood.default-client"
FILES_RELEASE="$PACKAGE/plugins/neovim-files/target/wasm32-unknown-unknown/release"
CLIENT_COMPONENT="$CLIENT_PACKAGE/brood_neovim_client_plugin.component.wasm"
FILES_COMPONENT="$FILES_RELEASE/brood_neovim_files_plugin.component.wasm"
CLIENT_RELEASE_REAL="$(dirname "$(readlink -f "$CLIENT_COMPONENT")")"
WASM_RUNTIME="$CLIENT_PACKAGE/share/brood/neovim-wasm"
[[ -s "$CLIENT_COMPONENT" ]]
[[ -s "$FILES_COMPONENT" ]]
for artifact in runner.mjs brood.lua nvim.js nvim.wasm nvim.data; do
  [[ -s "$WASM_RUNTIME/$artifact" ]]
done

read -r -a WAYLAND_BUILD_FLAGS <<< "$(pkg-config --cflags --libs wayland-client xkbcommon)"
cc \
  -std=c11 \
  -Wall \
  -Wextra \
  -Werror \
  tools/virtual-keyboard-key.c \
  -o "$OUT/virtual-keyboard-key" \
  "${WAYLAND_BUILD_FLAGS[@]}"

RUST_LOG="${RUST_LOG:-broodd=info,brood_client=info,smithay=warn,wgpu_core=warn,wgpu_hal=warn}"
(
  cd "$WORKSPACE"
  exec env \
    -u brood_session \
    -u BROOD_SURFACE_SESSION_ID \
    -u BROOD_PANE_ID \
    XDG_DATA_HOME="$DATA_HOME" \
    XDG_CONFIG_HOME="$CONFIG_HOME" \
    RUST_LOG="$RUST_LOG" \
    "$PACKAGE/bin/brood-sessiond" \
    --headless \
    --config "$PACKAGE/brood.toml" \
    --dbus-destination "$DBUS_DESTINATION" \
    --dbus-address "$DBUS_SESSION_BUS_ADDRESS"
) > "$SERVER_LOG" 2>&1 &
SERVER_PID=$!

ctl() {
  env \
    -u brood_session \
    -u BROOD_SURFACE_SESSION_ID \
    -u BROOD_PANE_ID \
    "$PACKAGE/bin/brood" ctl \
    --dbus-destination "$DBUS_DESTINATION" \
    --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
    "$@"
}

for _ in {1..300}; do
  ctl status > /dev/null 2>&1 && break
  kill -0 "$SERVER_PID" 2> /dev/null || {
    echo "ERR session server exited during editor proof startup" >&2
    exit 1
  }
  sleep 0.05
done
ctl status > /dev/null

printf '%s\n' \
  'xwayland disable' \
  'output * mode --custom 1280x720@60Hz' \
  'seat seat0 fallback true' \
  'default_border none' \
  'default_floating_border none' \
  > "$SWAY_CONFIG"
env \
  XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WLR_BACKENDS=headless \
  WLR_HEADLESS_OUTPUTS=1 \
  WLR_LIBINPUT_NO_DEVICES=1 \
  WLR_RENDERER_ALLOW_SOFTWARE=1 \
  sway \
  --unsupported-gpu \
  --config "$SWAY_CONFIG" \
  > "$COMPOSITOR_LOG" 2>&1 &
COMPOSITOR_PID=$!

WAYLAND_SOCKET=""
SWAY_SOCKET=""
for _ in {1..300}; do
  WAYLAND_SOCKET="$(
    find "$RUNTIME_DIR" -maxdepth 1 -type s -name 'wayland-*' -printf '%f\n' | head -1
  )"
  SWAY_SOCKET="$(
    find "$RUNTIME_DIR" -maxdepth 1 -type s -name 'sway-ipc.*.sock' -print | head -1
  )"
  [[ -n "$WAYLAND_SOCKET" && -n "$SWAY_SOCKET" ]] && break
  kill -0 "$COMPOSITOR_PID" 2> /dev/null || {
    echo "ERR headless Sway exited during editor proof startup" >&2
    exit 1
  }
  sleep 0.05
done
[[ -n "$WAYLAND_SOCKET" && -n "$SWAY_SOCKET" ]]

wayland_env() {
  env \
    XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    SWAYSOCK="$SWAY_SOCKET" \
    "$@"
}

env \
  -u brood_session \
  -u BROOD_SURFACE_SESSION_ID \
  -u BROOD_PANE_ID \
  XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  SWAYSOCK="$SWAY_SOCKET" \
  XDG_DATA_HOME="$DATA_HOME" \
  XDG_CONFIG_HOME="$CONFIG_HOME" \
  WGPU_BACKEND=vulkan \
  BROOD_DMABUF_HAPPY_PATH=1 \
  BROOD_TRACE_PLUGIN_GRAPHICS=1 \
  BROOD_TRACE_PLUGIN_INPUT=1 \
  BROOD_TRACE_PLUGIN_WORKERS=1 \
  RUST_LOG="$RUST_LOG" \
  "$PACKAGE/bin/brood-client" \
  --config "$PACKAGE/brood.toml" \
  --dbus-destination "$DBUS_DESTINATION" \
  --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
  > "$CLIENT_LOG" 2>&1 &
CLIENT_PID=$!

for _ in {1..400}; do
  if rg -q 'started presentation WASM actor id=terminal-ui' "$CLIENT_LOG" \
    && rg -q 'accepted plugin draw packet' "$CLIENT_LOG"; then
    break
  fi
  kill -0 "$CLIENT_PID" 2> /dev/null || {
    echo "ERR presentation client exited before its initial pane rendered" >&2
    exit 1
  }
  sleep 0.05
done
rg -q 'started presentation WASM actor id=terminal-ui' "$CLIENT_LOG"
rg -q 'accepted plugin draw packet' "$CLIENT_LOG"

capture() {
  wayland_env grim "$1"
  [[ -s "$1" ]]
}

press_plain_key() {
  wayland_env "$OUT/virtual-keyboard-key" --none "$1"
}

press_editor_shortcut() {
  # Linux evdev key 18 is E. The helper adds Ctrl+Alt, and the presentation
  # key policy sees the same event sequence as a physical keyboard.
  wayland_env "$OUT/virtual-keyboard-key" 18
}

focus_editor() {
  local before
  local after
  before="$(
    rg -c "focused local plugin pane=$EDITOR_PANE" "$CLIENT_LOG" 2> /dev/null \
      || true
  )"
  before="${before:-0}"
  press_editor_shortcut
  for _ in {1..200}; do
    after="$(
      rg -c "focused local plugin pane=$EDITOR_PANE" "$CLIENT_LOG" 2> /dev/null \
        || true
    )"
    after="${after:-0}"
    ((after > before)) && return
    sleep 0.05
  done
  echo "ERR editor pane did not regain focus" >&2
  return 1
}

nvim_command() {
  wayland_env wtype -s 5 ":$1" -s 20 -k Return
}

count_route_kind() {
  local sender=$1
  local recipient=$2
  local kind=$3
  local file=$4
  local count
  count="$(
    rg -c \
      "sender=$sender recipient=$recipient body=.*\"kind\":\"$kind\"" \
      "$file" \
      2> /dev/null \
      || true
  )"
  printf '%s\n' "${count:-0}"
}

route_has_kind() {
  local sender=$1
  local recipient=$2
  local kind=$3
  local file=$4
  rg -q \
    "sender=$sender recipient=$recipient body=.*\"kind\":\"$kind\"" \
    "$file"
}

local_open_completed() {
  local sender=$1
  local path=$2
  local file=$3
  rg -a -F \
    "presentation local actor message sender=$sender recipient=brood.client-init body=" \
    "$file" \
    | rg -F '"kind":"editor-opened"' \
    | rg -F -q "\"path\":\"$path\""
}

actor_pane() {
  local actor=$1
  local file=$2
  sed -n \
    "s/.*started presentation WASM actor id=$actor kind=[^ ]* generation=[^ ]* pane=\\([0-9][0-9]*\\) .*/\\1/p" \
    "$file" \
    | tail -1
}

draw_sequence() {
  local pane=$1
  local file=$2
  local sequence
  sequence="$(
    sed -n \
      "s/.*accepted plugin draw packet pane=$pane sequence=\\([0-9][0-9]*\\) .*/\\1/p" \
      "$file" \
      | tail -1
  )"
  printf '%s\n' "${sequence:-0}"
}

terminal_snapshot_rendered() {
  local actor=$1
  local pane=$2
  local file=$3
  awk \
    -v start="recipient=$actor terminal_snapshot_bytes=" \
    -v draw="accepted plugin draw packet pane=$pane " \
    '
      index($0, start) { snapshot = 1 }
      snapshot && index($0, draw) { found = 1; exit }
      END { exit !found }
    ' \
    "$file"
}

actor_list() {
  ctl actors list 2> /dev/null
}

DRAW_PACKETS_BEFORE="$(rg -c 'accepted plugin draw packet' "$CLIENT_LOG" || true)"
press_editor_shortcut

EDITOR_UI=""
EDITOR_PANE=""
EDITOR_FILES_ACTOR=""
SESSION_INIT_ACTOR=""
for _ in {1..1200}; do
  EDITOR_UI="$(
    sed -n \
      's/.*started presentation WASM actor id=\(neovim-ui[^ ]*\).*/\1/p' \
      "$CLIENT_LOG" \
      | tail -1
  )"
  if [[ "$EDITOR_UI" == neovim-ui-* ]]; then
    EDITOR_PANE="$(actor_pane "$EDITOR_UI" "$CLIENT_LOG")"
  fi
  ACTORS="$(actor_list || true)"
  EDITOR_FILES_ACTOR="$(
    sed -n 's/^actor id=\([^ ]*\.neovim-files\) .*/\1/p' <<< "$ACTORS" | head -1
  )"
  SESSION_INIT_ACTOR="$(
    sed -n 's/^actor id=\(brood\.session-init\.[0-9][0-9]*\) .*/\1/p' \
      <<< "$ACTORS" \
      | head -1
  )"
  DRAW_PACKETS_AFTER="$(rg -c 'accepted plugin draw packet' "$CLIENT_LOG" || true)"
  if [[ "$EDITOR_UI" == neovim-ui-* \
    && "$EDITOR_PANE" =~ ^[0-9]+$ \
    && "$EDITOR_FILES_ACTOR" == *.neovim-files \
    && "$SESSION_INIT_ACTOR" == brood.session-init.* ]] \
    && ((DRAW_PACKETS_AFTER > DRAW_PACKETS_BEFORE)) \
    && route_has_kind neovim-files "$EDITOR_UI" editor-client-authorized "$CLIENT_LOG" \
    && route_has_kind "$EDITOR_UI" init editor-files-ready "$CLIENT_LOG"; then
    break
  fi
  kill -0 "$CLIENT_PID" 2> /dev/null || {
    echo "ERR presentation client exited before local Wasm Neovim was ready" >&2
    exit 1
  }
  sleep 0.05
done
[[ "$EDITOR_UI" == neovim-ui-* ]]
[[ "$EDITOR_PANE" =~ ^[0-9]+$ ]]
[[ "$EDITOR_FILES_ACTOR" == *.neovim-files ]]
[[ "$SESSION_INIT_ACTOR" == brood.session-init.* ]]
route_has_kind neovim-files "$EDITOR_UI" editor-client-authorized "$CLIENT_LOG"
route_has_kind "$EDITOR_UI" init editor-files-ready "$CLIENT_LOG"
actor_list > "$OUT/actors-editor-created.txt"

# The Emscripten/Node process must be a child of the presentation client and
# run from the packaged client actor directory. The backend must not own a
# native Neovim subprocess.
RUNNER_PID=""
for _ in {1..600}; do
  ps -eo pid=,ppid=,args= > "$OUT/processes.txt"
  RUNNER_PID="$(
    awk -v parent="$CLIENT_PID" \
      '$2 == parent && $0 ~ /node .*share\/brood\/neovim-wasm\/runner\.mjs/ { print $1; exit }' \
      "$OUT/processes.txt"
  )"
  [[ -n "$RUNNER_PID" ]] && break
  sleep 0.05
done
[[ -n "$RUNNER_PID" ]]
RUNNER_CWD="$(readlink -f "/proc/$RUNNER_PID/cwd")"
[[ "$RUNNER_CWD" == "$CLIENT_RELEASE_REAL" ]]
awk -v runner="$RUNNER_PID" -v client="$CLIENT_PID" -v server="$SERVER_PID" \
  '$1 == runner || $1 == client || $1 == server' \
  "$OUT/processes.txt" \
  > "$OUT/editor-process-placement.txt"
python3 - "$SERVER_PID" "$OUT/processes.txt" \
  > "$OUT/backend-descendants.txt" <<'PY'
import pathlib
import sys

root = int(sys.argv[1])
records = {}
children = {}
for raw in pathlib.Path(sys.argv[2]).read_text().splitlines():
    fields = raw.strip().split(maxsplit=2)
    if len(fields) < 2:
        continue
    pid = int(fields[0])
    parent = int(fields[1])
    records[pid] = raw
    children.setdefault(parent, []).append(pid)

pending = list(children.get(root, ()))
seen = set()
while pending:
    pid = pending.pop()
    if pid in seen:
        continue
    seen.add(pid)
    print(records[pid])
    pending.extend(children.get(pid, ()))
PY
if rg -q '(^|[/ ])nvim( |$)|brood-nvim' "$OUT/backend-descendants.txt"; then
  echo "ERR backend unexpectedly owns a native Neovim process" >&2
  exit 1
fi

sleep 0.4
capture "$OUT/editor-created.png"

# Open a workspace-relative path by addressing the session init actor. The
# init actor forwards the open to the client, which fetches bytes and revision
# chunks from the backend neovim-files actor.
OPEN_REQUESTS_BEFORE="$(
  count_route_kind "$EDITOR_UI" neovim-files open "$CLIENT_LOG"
)"
OPEN_REPLIES_BEFORE="$(
  count_route_kind neovim-files "$EDITOR_UI" open "$CLIENT_LOG"
)"
EDITOR_DRAW_BEFORE="$(draw_sequence "$EDITOR_PANE" "$CLIENT_LOG")"
EDIT_OPEN_BODY="$(
  jq -cn \
    '{kind:"open-editor", path:"tracked.txt", line:1, column:1}'
)"
ctl actors send "$SESSION_INIT_ACTOR" "$EDIT_OPEN_BODY" \
  | tee "$OUT/editor-open-send.txt"
rg -q 'queued=true' "$OUT/editor-open-send.txt"
for _ in {1..600}; do
  OPEN_REQUESTS_AFTER="$(
    count_route_kind "$EDITOR_UI" neovim-files open "$CLIENT_LOG"
  )"
  OPEN_REPLIES_AFTER="$(
    count_route_kind neovim-files "$EDITOR_UI" open "$CLIENT_LOG"
  )"
  EDITOR_DRAW_AFTER="$(draw_sequence "$EDITOR_PANE" "$CLIENT_LOG")"
  if ((OPEN_REQUESTS_AFTER > OPEN_REQUESTS_BEFORE \
    && OPEN_REPLIES_AFTER > OPEN_REPLIES_BEFORE \
    && EDITOR_DRAW_AFTER > EDITOR_DRAW_BEFORE)) \
    && local_open_completed "$EDITOR_UI" tracked.txt "$CLIENT_LOG"; then
    break
  fi
  kill -0 "$CLIENT_PID" 2> /dev/null || {
    echo "ERR presentation client exited while opening the remote file" >&2
    exit 1
  }
  sleep 0.05
done
((OPEN_REQUESTS_AFTER > OPEN_REQUESTS_BEFORE))
((OPEN_REPLIES_AFTER > OPEN_REPLIES_BEFORE))
((EDITOR_DRAW_AFTER > EDITOR_DRAW_BEFORE))
local_open_completed "$EDITOR_UI" tracked.txt "$CLIENT_LOG"
sleep 0.1
capture "$OUT/editor-opened.png"

# The unique command exists only if the host config's Lua plugin loaded inside
# Wasm Neovim. Ordinary Normal/Insert-mode keys then make a second edit.
# A physical Backspace removes the trailing sentinel rather than inserting a
# control character. :write must carry both changes through the
# revision-checked backend actor.
nvim_command BroodHostPluginProof
sleep 0.2
wayland_env wtype -s 5 'GoBROOD_EDITOR_REAL_KEYSX'
press_plain_key 14
press_plain_key 1
SAVES_BEFORE="$(
  count_route_kind neovim-files "$EDITOR_UI" save-complete "$CLIENT_LOG"
)"
nvim_command write
for _ in {1..600}; do
  if rg -F -q "$HOST_PLUGIN_LINE" "$EDIT_FILE" \
    && [[ "$(tail -n 1 "$EDIT_FILE")" == 'BROOD_EDITOR_REAL_KEYS' ]] \
    && (("$(count_route_kind neovim-files "$EDITOR_UI" save-complete "$CLIENT_LOG")" > SAVES_BEFORE)); then
    break
  fi
  sleep 0.05
done
rg -F -q "$HOST_PLUGIN_LINE" "$EDIT_FILE"
[[ "$(tail -n 1 "$EDIT_FILE")" == 'BROOD_EDITOR_REAL_KEYS' ]]
route_has_kind "$EDITOR_UI" neovim-files save-begin "$CLIENT_LOG"
(("$(count_route_kind neovim-files "$EDITOR_UI" save-complete "$CLIENT_LOG")" > SAVES_BEFORE))
capture "$OUT/editor-edited.png"

# A second addressed open includes one-based line and column. The subsequent
# append and backend save prove the requested cursor location in actual bytes.
OPEN_REQUESTS_BEFORE="$(
  count_route_kind "$EDITOR_UI" neovim-files open "$CLIENT_LOG"
)"
OPEN_REPLIES_BEFORE="$(
  count_route_kind neovim-files "$EDITOR_UI" open "$CLIENT_LOG"
)"
EDITOR_DRAW_BEFORE="$(draw_sequence "$EDITOR_PANE" "$CLIENT_LOG")"
INBOUND_BODY="$(
  jq -cn \
    '{kind:"open-editor", path:"inbound.txt", line:2, column:3}'
)"
ctl actors send "$SESSION_INIT_ACTOR" "$INBOUND_BODY" \
  | tee "$OUT/editor-inbound-send.txt"
rg -q 'queued=true' "$OUT/editor-inbound-send.txt"
for _ in {1..600}; do
  OPEN_REQUESTS_AFTER="$(
    count_route_kind "$EDITOR_UI" neovim-files open "$CLIENT_LOG"
  )"
  OPEN_REPLIES_AFTER="$(
    count_route_kind neovim-files "$EDITOR_UI" open "$CLIENT_LOG"
  )"
  EDITOR_DRAW_AFTER="$(draw_sequence "$EDITOR_PANE" "$CLIENT_LOG")"
  if ((OPEN_REQUESTS_AFTER > OPEN_REQUESTS_BEFORE \
    && OPEN_REPLIES_AFTER > OPEN_REPLIES_BEFORE \
    && EDITOR_DRAW_AFTER > EDITOR_DRAW_BEFORE)) \
    && local_open_completed "$EDITOR_UI" inbound.txt "$CLIENT_LOG"; then
    break
  fi
  sleep 0.05
done
((OPEN_REQUESTS_AFTER > OPEN_REQUESTS_BEFORE))
((OPEN_REPLIES_AFTER > OPEN_REPLIES_BEFORE))
((EDITOR_DRAW_AFTER > EDITOR_DRAW_BEFORE))
local_open_completed "$EDITOR_UI" inbound.txt "$CLIENT_LOG"
sleep 0.1
wayland_env wtype -s 5 'a__BROOD_INBOUND__'
press_plain_key 1
SAVES_BEFORE="$(
  count_route_kind neovim-files "$EDITOR_UI" save-complete "$CLIENT_LOG"
)"
nvim_command write
for _ in {1..600}; do
  if [[ "$(sed -n '2p' "$INBOUND_FILE")" == 'sec__BROOD_INBOUND__ond inbound line' ]] \
    && (("$(count_route_kind neovim-files "$EDITOR_UI" save-complete "$CLIENT_LOG")" > SAVES_BEFORE)); then
    break
  fi
  sleep 0.05
done
[[ "$(sed -n '2p' "$INBOUND_FILE")" == 'sec__BROOD_INBOUND__ond inbound line' ]]
(("$(count_route_kind neovim-files "$EDITOR_UI" save-complete "$CLIENT_LOG")" > SAVES_BEFORE))
capture "$OUT/editor-inbound-open.png"
actor_list > "$OUT/actors-after-inbound.txt"

# A file larger than 64 runner chunks proves that one coalesced actor
# notification drains a multi-record upload across bounded worker turns. Its
# subsequent write exercises the streamed save path in the opposite direction.
OPEN_REQUESTS_BEFORE="$(
  count_route_kind "$EDITOR_UI" neovim-files open "$CLIENT_LOG"
)"
OPEN_REPLIES_BEFORE="$(
  count_route_kind neovim-files "$EDITOR_UI" open "$CLIENT_LOG"
)"
EDITOR_DRAW_BEFORE="$(draw_sequence "$EDITOR_PANE" "$CLIENT_LOG")"
LARGE_BODY="$(
  jq -cn \
    '{kind:"open-editor", path:"large.txt", line:1, column:1}'
)"
ctl actors send "$SESSION_INIT_ACTOR" "$LARGE_BODY" \
  | tee "$OUT/editor-large-send.txt"
rg -q 'queued=true' "$OUT/editor-large-send.txt"
for _ in {1..800}; do
  OPEN_REQUESTS_AFTER="$(
    count_route_kind "$EDITOR_UI" neovim-files open "$CLIENT_LOG"
  )"
  OPEN_REPLIES_AFTER="$(
    count_route_kind neovim-files "$EDITOR_UI" open "$CLIENT_LOG"
  )"
  EDITOR_DRAW_AFTER="$(draw_sequence "$EDITOR_PANE" "$CLIENT_LOG")"
  if ((OPEN_REQUESTS_AFTER > OPEN_REQUESTS_BEFORE \
    && OPEN_REPLIES_AFTER > OPEN_REPLIES_BEFORE \
    && EDITOR_DRAW_AFTER > EDITOR_DRAW_BEFORE)) \
    && local_open_completed "$EDITOR_UI" large.txt "$CLIENT_LOG"; then
    break
  fi
  sleep 0.05
done
((OPEN_REQUESTS_AFTER > OPEN_REQUESTS_BEFORE))
((OPEN_REPLIES_AFTER > OPEN_REPLIES_BEFORE))
((EDITOR_DRAW_AFTER > EDITOR_DRAW_BEFORE))
local_open_completed "$EDITOR_UI" large.txt "$CLIENT_LOG"
(("$(wc -c < "$LARGE_FILE")" > 64 * 1024))
wayland_env wtype -s 5 'iZ'
press_plain_key 1
SAVES_BEFORE="$(
  count_route_kind neovim-files "$EDITOR_UI" save-complete "$CLIENT_LOG"
)"
nvim_command write
for _ in {1..800}; do
  if [[ "$(head -c 1 "$LARGE_FILE")" == Z ]] \
    && (("$(count_route_kind neovim-files "$EDITOR_UI" save-complete "$CLIENT_LOG")" > SAVES_BEFORE)); then
    break
  fi
  sleep 0.05
done
[[ "$(head -c 1 "$LARGE_FILE")" == Z ]]
(("$(count_route_kind neovim-files "$EDITOR_UI" save-complete "$CLIENT_LOG")" > SAVES_BEFORE))
capture "$OUT/editor-large-stream.png"

# Exercise the typed outbound routes through actual Neovim commands. The
# terminal action creates a backend terminal actor and leaves a filesystem
# marker. The diff action starts the preferred-session diff-viewer and renders
# the real workspace diff. Ctrl+Alt+E focuses the existing local editor between
# panes.
DYNAMIC_TERMINALS_BEFORE="$(
  actor_list | rg -c '\.terminal-session-[0-9]+ ' || true
)"
nvim_command "BroodTerminal touch terminal-command-routed"
TERMINAL_ACTION_UI=""
TERMINAL_ACTION_PANE=""
for _ in {1..800}; do
  DYNAMIC_TERMINALS_AFTER="$(
    actor_list | rg -c '\.terminal-session-[0-9]+ ' || true
  )"
  TERMINAL_ACTION_UI="$(
    sed -n \
      's/.*started presentation WASM actor id=\(terminal-ui-b[^ ]*-s[^ ]*-[0-9][0-9]*\).*/\1/p' \
      "$CLIENT_LOG" \
      | tail -1
  )"
  if [[ -n "$TERMINAL_ACTION_UI" ]]; then
    TERMINAL_ACTION_PANE="$(actor_pane "$TERMINAL_ACTION_UI" "$CLIENT_LOG")"
  fi
  if [[ -f "$TERMINAL_MARKER" && "$TERMINAL_ACTION_PANE" =~ ^[0-9]+$ ]] \
    && terminal_snapshot_rendered \
      "$TERMINAL_ACTION_UI" "$TERMINAL_ACTION_PANE" "$CLIENT_LOG" \
    && ((DYNAMIC_TERMINALS_AFTER > DYNAMIC_TERMINALS_BEFORE)); then
    break
  fi
  sleep 0.05
done
[[ -f "$TERMINAL_MARKER" ]]
[[ -n "$TERMINAL_ACTION_UI" ]]
[[ "$TERMINAL_ACTION_PANE" =~ ^[0-9]+$ ]]
terminal_snapshot_rendered \
  "$TERMINAL_ACTION_UI" "$TERMINAL_ACTION_PANE" "$CLIENT_LOG"
((DYNAMIC_TERMINALS_AFTER > DYNAMIC_TERMINALS_BEFORE))
rg -F -q '"command":"touch terminal-command-routed"' "$CLIENT_LOG"
sleep 0.4
capture "$OUT/editor-terminal-action.png"

focus_editor
nvim_command BroodDiff
DIFF_ACTOR="diff-viewer-sb1-s1"
DIFF_PANE=""
INITIAL_DIFF_SEQUENCE=""
POPULATED_DIFF_SEQUENCE=""
for _ in {1..800}; do
  if [[ -z "$DIFF_PANE" ]] \
    && rg -a -q "started presentation WASM actor id=$DIFF_ACTOR" "$CLIENT_LOG"; then
    DIFF_PANE="$(actor_pane "$DIFF_ACTOR" "$CLIENT_LOG")"
  fi
  if [[ "$DIFF_PANE" =~ ^[0-9]+$ && -z "$INITIAL_DIFF_SEQUENCE" ]]; then
    INITIAL_DIFF_SEQUENCE="$(
      sed -n "/started presentation WASM actor id=$DIFF_ACTOR/,\$p" "$CLIENT_LOG" \
        | sed -n 's/.*accepted plugin draw packet pane='"$DIFF_PANE"' sequence=\([0-9][0-9]*\).*/\1/p' \
        | head -1
    )"
  fi
  if [[ -n "$INITIAL_DIFF_SEQUENCE" ]] \
    && rg -a -q "presentation actor worker finished actor=$DIFF_ACTOR" "$CLIENT_LOG"; then
    POPULATED_DIFF_SEQUENCE="$(
      sed -n "/presentation actor worker finished actor=$DIFF_ACTOR/,\$p" "$CLIENT_LOG" \
        | sed -n 's/.*accepted plugin draw packet pane='"$DIFF_PANE"' sequence=\([0-9][0-9]*\) commands=\([2-9][0-9]\|[1-9][0-9][0-9][0-9]*\).*/\1/p' \
        | tail -1
    )"
  fi
  if [[ -n "$POPULATED_DIFF_SEQUENCE" ]] \
    && ((POPULATED_DIFF_SEQUENCE > INITIAL_DIFF_SEQUENCE)); then
    break
  fi
  sleep 0.05
done
rg -a -q "started presentation WASM actor id=$DIFF_ACTOR" "$CLIENT_LOG"
[[ "$DIFF_PANE" =~ ^[0-9]+$ ]]
rg -a -q "presentation actor worker finished actor=$DIFF_ACTOR" "$CLIENT_LOG"
[[ -n "$INITIAL_DIFF_SEQUENCE" && -n "$POPULATED_DIFF_SEQUENCE" ]]
((POPULATED_DIFF_SEQUENCE > INITIAL_DIFF_SEQUENCE))
sleep 0.8
capture "$OUT/editor-diff-action.png"

focus_editor
AGENT_PROMPT='Reply with editor-route-ready only; do not change files'
nvim_command "BroodAgent $AGENT_PROMPT"
AGENT_UI=""
AGENT_PANE=""
for _ in {1..800}; do
  AGENT_UI="$(
    sed -n \
      's/.*started presentation WASM actor id=\(coding-agent-ui-[^ ]*\).*/\1/p' \
      "$CLIENT_LOG" \
      | tail -1
  )"
  if [[ -n "$AGENT_UI" ]]; then
    AGENT_PANE="$(actor_pane "$AGENT_UI" "$CLIENT_LOG")"
  fi
  if [[ "$AGENT_PANE" =~ ^[0-9]+$ ]] \
    && (("$(draw_sequence "$AGENT_PANE" "$CLIENT_LOG")" > 0)) \
    && rg -F -q "\"prompt\":\"$AGENT_PROMPT\"" "$CLIENT_LOG"; then
    break
  fi
  kill -0 "$CLIENT_PID" 2> /dev/null || {
    echo "ERR presentation client exited during editor action routing" >&2
    exit 1
  }
  sleep 0.05
done
[[ -n "$AGENT_UI" ]]
[[ "$AGENT_PANE" =~ ^[0-9]+$ ]]
(("$(draw_sequence "$AGENT_PANE" "$CLIENT_LOG")" > 0))
rg -F -q "\"prompt\":\"$AGENT_PROMPT\"" "$CLIENT_LOG"
sleep 0.5
capture "$OUT/editor-agent-action.png"

STATUS="$(ctl status)"
STATUS_JSON="$(ctl status-json)"
rg -q 'presentation=Attached' <<< "$STATUS"
actor_list > "$OUT/actors-final.txt"
rg -q '\.neovim-files ' "$OUT/actors-final.txt"
rg -q '\.terminal-session-[0-9]+ ' "$OUT/actors-final.txt"
if rg -q 'neovim-session' "$OUT/actors-final.txt" "$OUT/editor-process-placement.txt"; then
  echo "ERR obsolete backend-native neovim-session is still active" >&2
  exit 1
fi

rg -a \
  'neovim-ui|neovim-files|editor-action|create-terminal|git --no-pager diff|coding-agent-ui|coding-agent-1' \
  "$SERVER_LOG" "$CLIENT_LOG" \
  > "$OUT/editor-routes.log"

python3 - \
  "$OUT/editor-created.png" \
  "$OUT/editor-opened.png" \
  "$OUT/editor-edited.png" \
  "$OUT/editor-inbound-open.png" \
  "$OUT/editor-large-stream.png" \
  "$OUT/editor-terminal-action.png" \
  "$OUT/editor-diff-action.png" \
  "$OUT/editor-agent-action.png" \
  > "$OUT/screenshot-metrics.json" <<'PY'
import json
import pathlib
import subprocess
import sys

paths = [pathlib.Path(value) for value in sys.argv[1:]]
captures = []
for path in paths:
    data = path.read_bytes()
    if not data.startswith(b"\x89PNG\r\n\x1a\n") or len(data) < 4096:
        raise SystemExit(f"invalid editor graphical proof: {path}")
    identify = subprocess.check_output(
        ["magick", str(path), "-format", "%wx%h %k", "info:"], text=True
    ).split()
    dimensions = identify[0]
    colors = int(identify[1])
    if dimensions != "1280x720":
        raise SystemExit(f"unexpected editor capture dimensions: {path} {dimensions}")
    if colors < 40:
        raise SystemExit(
            f"editor proof has too little rendered content: {path} colors={colors}"
        )
    captures.append(
        {
            "file": path.name,
            "bytes": len(data),
            "dimensions": dimensions,
            "colors": colors,
        }
    )

comparisons = []
for first, second in [
    (paths[1], paths[2]),
    (paths[2], paths[3]),
    (paths[3], paths[4]),
    (paths[4], paths[5]),
    (paths[5], paths[6]),
    (paths[6], paths[7]),
]:
    metric = subprocess.run(
        ["magick", "compare", "-metric", "RMSE", first, second, "null:"],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    ).stderr
    normalized = float(metric.rsplit("(", 1)[1].rstrip(")\n"))
    if normalized < 0.0005:
        raise SystemExit(
            f"editor captures are unexpectedly similar: {first} {second} RMSE={normalized}"
        )
    comparisons.append(
        {
            "first": first.name,
            "second": second.name,
            "rmse": normalized,
        }
    )

print(json.dumps({"captures": captures, "comparisons": comparisons}, indent=2))
PY

OPEN_REQUESTS="$(count_route_kind "$EDITOR_UI" neovim-files open "$CLIENT_LOG")"
OPEN_REPLIES="$(count_route_kind neovim-files "$EDITOR_UI" open "$CLIENT_LOG")"
SAVE_BEGINS="$(count_route_kind "$EDITOR_UI" neovim-files save-begin "$CLIENT_LOG")"
SAVE_COMPLETES="$(count_route_kind neovim-files "$EDITOR_UI" save-complete "$CLIENT_LOG")"
DRAW_PACKETS="$(rg -c 'accepted plugin draw packet' "$CLIENT_LOG" || true)"
LARGE_BYTES="$(wc -c < "$LARGE_FILE")"
SCREENSHOT_METRICS="$(< "$OUT/screenshot-metrics.json")"
jq -n \
  --argjson status "$STATUS_JSON" \
  --argjson screenshots "$SCREENSHOT_METRICS" \
  --arg editor_files_actor "$EDITOR_FILES_ACTOR" \
  --arg editor_ui "$EDITOR_UI" \
  --arg runner_cwd "$RUNNER_CWD" \
  --arg host_config_marker "$HOST_CONFIG_MARKER" \
  --arg host_plugin_line "$HOST_PLUGIN_LINE" \
  --argjson open_requests "$OPEN_REQUESTS" \
  --argjson open_replies "$OPEN_REPLIES" \
  --argjson save_begins "$SAVE_BEGINS" \
  --argjson save_completes "$SAVE_COMPLETES" \
  --argjson large_bytes "$LARGE_BYTES" \
  --argjson draw_packets "$DRAW_PACKETS" \
  '{
    placement: {
      client_actor: $editor_ui,
      backend_file_actor: $editor_files_actor,
      local_runner_cwd: $runner_cwd,
      backend_native_neovim: false
    },
    workload: {
      ctrl_alt_e: true,
      host_config_marker: $host_config_marker,
      host_lua_plugin_effect: $host_plugin_line,
      normal_insert_write: true,
      remote_file_protocol: {
        open_requests: $open_requests,
        open_replies: $open_replies,
        save_begins: $save_begins,
        save_completes: $save_completes
      },
      inbound_actor_message: {
        kind: "open-editor",
        path: "inbound.txt",
        line: 2,
        column: 3
      },
      chunked_remote_file: {
        path: "large.txt",
        bytes: $large_bytes,
        saved_prefix: "Z"
      },
      outbound_routes: {
        terminal_command_marker: true,
        preferred_session_diff_viewer: true,
        agent_prompt: true
      }
    },
    render: {
      draw_packets: $draw_packets,
      screenshots: $screenshots
    },
    status: $status
  }' > "$OUT/proof.json"

{
  printf '%s\n' \
    '# Brood client-local Wasm Neovim proof' \
    '' \
    "- package: $CLIENT_COMPONENT is the presentation actor, $FILES_COMPONENT is the backend file actor, and the packaged runtime contains nvim.js/nvim.wasm/nvim.data" \
    '- isolation: private DBus session and headless Sway; no presentation window was created on the host desktop' \
    "- placement: client PID $CLIENT_PID owns Node/Wasm runner PID $RUNNER_PID in $RUNNER_CWD; backend PID $SERVER_PID owns $EDITOR_FILES_ACTOR and no native Neovim process" \
    "- host config: the temporary host XDG config loaded its pure-Lua plugin; :BroodHostPluginProof inserted $HOST_PLUGIN_LINE into the remote buffer" \
    "- remote editing: workspace-relative open/read and revision-checked save messages crossed between $EDITOR_UI and $EDITOR_FILES_ACTOR; tracked.txt and inbound.txt contain the resulting bytes" \
    '- inbound messaging: an addressed open-editor selected inbound.txt at line 2, column 3; the real key append produced sec__BROOD_INBOUND__ond inbound line' \
    "- streaming: large.txt crossed more than 64 client-to-runner chunks and its Z-prefixed save returned through the backend file actor" \
    '- outbound messaging: :BroodTerminal executed a backend command, :BroodDiff opened the preferred-session diff-viewer and populated its canvas, and :BroodAgent routed a bounded prompt into a coding-agent pane' \
    '- graphical proof: editor-created.png, editor-opened.png, editor-edited.png, editor-inbound-open.png, editor-large-stream.png, editor-terminal-action.png, editor-diff-action.png, editor-agent-action.png' \
    '- structured evidence: proof.json, screenshot-metrics.json, editor-routes.log, editor-process-placement.txt, actors-editor-created.txt, actors-after-inbound.txt, actors-final.txt' \
    '- scope: this built-in nested proof validates the client/backend actor boundary in one private offscreen process topology; cross-host transport remains covered by the separate SSH test path' \
    "- status: $STATUS"
} > "$OUT/proof.md"

mkdir -p "$OUT/fixtures"
cp "$EDIT_FILE" "$OUT/fixtures/tracked.txt"
cp "$INBOUND_FILE" "$OUT/fixtures/inbound.txt"
cp "$LARGE_FILE" "$OUT/fixtures/large.txt"
cp "$TERMINAL_MARKER" "$OUT/fixtures/terminal-command-routed"
cp -R "$HOST_CONFIG_DIR" "$OUT/fixtures/host-nvim-config"

ctl quit > /dev/null 2>&1 || true
echo "editor_plugin_nested=verified"
echo "proof=$OUT/proof.md"
echo "metrics=$OUT/proof.json"
echo "created=$OUT/editor-created.png"
echo "edited=$OUT/editor-edited.png"
echo "inbound=$OUT/editor-inbound-open.png"
echo "stream=$OUT/editor-large-stream.png"
echo "terminal=$OUT/editor-terminal-action.png"
echo "diff=$OUT/editor-diff-action.png"
echo "agent=$OUT/editor-agent-action.png"
