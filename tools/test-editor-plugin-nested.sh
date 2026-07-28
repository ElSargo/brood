#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# The editor proof owns its bus and compositor. Never attach its service,
# Neovim instance, or presentation client to the caller's desktop session.
if [[ "${BROOD_EDITOR_PRIVATE_DBUS:-0}" != 1 ]]; then
  exec env \
    -u brood_session \
    -u BROOD_SURFACE_SESSION_ID \
    -u BROOD_PANE_ID \
    -u DBUS_SESSION_BUS_ADDRESS \
    BROOD_EDITOR_PRIVATE_DBUS=1 \
    dbus-run-session -- "$0" "$@"
fi

for tool in cc git grim jq magick nix pkg-config python3 rg sway wtype; do
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
TERMINAL_MARKER="$WORKSPACE/terminal-command-routed"
ACTIONS_FILE="$WORKSPACE/editor-actions.vim"
mkdir -p "$OUT" "$DATA_HOME" "$CONFIG_HOME"
chmod 700 "$RUNTIME_DIR"

git -C "$WORKSPACE" init --quiet
printf '%s\n' \
  'tracked editor baseline' \
  > "$EDIT_FILE"
printf '%s\n' \
  'first inbound line' \
  'second inbound line' \
  'third inbound line' \
  > "$INBOUND_FILE"
git -C "$WORKSPACE" add tracked.txt inbound.txt
git -C "$WORKSPACE" \
  -c user.name=Brood \
  -c user.email=brood@example.invalid \
  commit --quiet -m baseline
printf '%s\n' \
  "BroodTerminal touch $TERMINAL_MARKER" \
  'BroodDiff' \
  'BroodAgent Reply with editor-route-ready only; do not run tools or change files' \
  > "$ACTIONS_FILE"

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
  mkdir -p "$OUT/fixtures"
  [[ -f "$EDIT_FILE" ]] && cp "$EDIT_FILE" "$OUT/fixtures/tracked.txt"
  [[ -f "$INBOUND_FILE" ]] && cp "$INBOUND_FILE" "$OUT/fixtures/inbound.txt"
  [[ -f "$ACTIONS_FILE" ]] && cp "$ACTIONS_FILE" "$OUT/fixtures/editor-actions.vim"
  [[ -f "$TERMINAL_MARKER" ]] && cp "$TERMINAL_MARKER" "$OUT/fixtures/terminal-command-routed"
  rm -rf -- "$RUNTIME_DIR" "$WORKSPACE"
  if ((result != 0)); then
    echo "client_log=$CLIENT_LOG" >&2
    [[ -f "$CLIENT_LOG" ]] && tail -n 220 "$CLIENT_LOG" >&2
    echo "server_log=$SERVER_LOG" >&2
    [[ -f "$SERVER_LOG" ]] && tail -n 180 "$SERVER_LOG" >&2
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
[[ -x "$PACKAGE/bin/brood-nvim" ]]
[[ -x "$PACKAGE/bin/nvim" ]]
[[ -s "$PACKAGE/brood.toml" ]]

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

# Linux evdev key 18 is E. The helper adds Ctrl+Alt, and the presentation key
# policy sees XKB keycode 26, exactly as it would from a physical keyboard.
wayland_env "$OUT/virtual-keyboard-key" 18

EDITOR_UI=""
EDITOR_ACTOR=""
EDITOR_METRIC=""
for _ in {1..600}; do
  EDITOR_UI="$(
    sed -n \
      's/.*started presentation WASM actor id=\(neovim-ui[^ ]*\).*/\1/p' \
      "$CLIENT_LOG" \
      | tail -1
  )"
  EDITOR_ACTOR="$(
    ctl actors list 2> /dev/null \
      | sed -n 's/^actor id=\([^ ]*neovim-session\) .*/\1/p' \
      | head -1
  )"
  if [[ -n "$EDITOR_UI" ]]; then
    EDITOR_METRIC="$(
      sed -n \
        "s/^presentation plugin worker metrics actor=${EDITOR_UI} //p" \
        "$CLIENT_LOG" \
        | tail -1
    )"
  fi
  if [[ -n "$EDITOR_UI" && -n "$EDITOR_ACTOR" && -n "$EDITOR_METRIC" ]] \
    && jq -e '.pty_bytes > 100 and .packets_total > 0' \
      <<< "$EDITOR_METRIC" > /dev/null; then
    break
  fi
  kill -0 "$CLIENT_PID" 2> /dev/null || {
    echo "ERR presentation client exited before Neovim rendered" >&2
    exit 1
  }
  sleep 0.05
done
[[ "$EDITOR_UI" == neovim-ui-* ]]
[[ "$EDITOR_ACTOR" == *neovim-session ]]
jq -e '.pty_bytes > 100 and .packets_total > 0' \
  <<< "$EDITOR_METRIC" > /dev/null
rg -q 'sent key-policy actor message actor=brood.client-init' "$CLIENT_LOG"
ctl actors list > "$OUT/actors-editor-created.txt"

capture() {
  wayland_env grim "$1"
  [[ -s "$1" ]]
}

press_plain_key() {
  wayland_env "$OUT/virtual-keyboard-key" --none "$1"
}

nvim_command() {
  wayland_env wtype -s 5 ":$1" -s 20 -k Return
}

sleep 0.4
capture "$OUT/editor-created.png"

# Open a tracked fixture, then use ordinary Normal/Insert mode keys and :write.
nvim_command "edit $EDIT_FILE"
sleep 0.3
wayland_env wtype -s 5 'GoBROOD_EDITOR_REAL_KEYS'
press_plain_key 1
nvim_command write
for _ in {1..300}; do
  [[ "$(tail -n 1 "$EDIT_FILE")" == 'BROOD_EDITOR_REAL_KEYS' ]] && break
  sleep 0.05
done
[[ "$(tail -n 1 "$EDIT_FILE")" == 'BROOD_EDITOR_REAL_KEYS' ]]
capture "$OUT/editor-edited.png"

# Address the durable session actor through the public actor control surface.
# Wait for its remote-control worker rather than a PTY redraw: opening a buffer
# may be visually equivalent and therefore emit no new terminal bytes. The
# requested line/column is then proved by an actual Normal-mode append.
INBOUND_BODY="$(
  jq -cn \
    --arg path "$INBOUND_FILE" \
    '{kind:"editor-open", path:$path, line:2, column:3}'
)"
EDITOR_OPEN_WORKER_PATTERN='component actor worker finished actor=.*neovim-session worker=([3-9]|[1-9][0-9]+)$'
EDITOR_OPEN_WORKERS_BEFORE="$(rg -c "$EDITOR_OPEN_WORKER_PATTERN" "$SERVER_LOG" || true)"
ctl actors send "$EDITOR_ACTOR" "$INBOUND_BODY" \
  | tee "$OUT/editor-open-send.txt"
rg -q 'queued=true' "$OUT/editor-open-send.txt"

for _ in {1..400}; do
  EDITOR_OPEN_WORKERS_AFTER="$(rg -c "$EDITOR_OPEN_WORKER_PATTERN" "$SERVER_LOG" || true)"
  ((EDITOR_OPEN_WORKERS_AFTER > EDITOR_OPEN_WORKERS_BEFORE)) && break
  sleep 0.05
done
((EDITOR_OPEN_WORKERS_AFTER > EDITOR_OPEN_WORKERS_BEFORE))
sleep 0.3
wayland_env wtype -s 5 'a__BROOD_INBOUND__'
press_plain_key 1
nvim_command write
for _ in {1..300}; do
  [[ "$(sed -n '2p' "$INBOUND_FILE")" == 'sec__BROOD_INBOUND__ond inbound line' ]] && break
  sleep 0.05
done
[[ "$(sed -n '2p' "$INBOUND_FILE")" == 'sec__BROOD_INBOUND__ond inbound line' ]]
capture "$OUT/editor-inbound-open.png"
ctl actors list > "$OUT/actors-after-inbound.txt"

# Source all product commands in one Neovim turn so the asynchronous pane
# creations cannot steal focus between them.
nvim_command "source $ACTIONS_FILE"
for _ in {1..600}; do
  if [[ -f "$TERMINAL_MARKER" ]] \
    && rg -q \
      'backend actor message .*sender=.*neovim-session .*recipient=brood.client-init .*body=.*"action":"terminal"' \
      "$CLIENT_LOG" \
    && rg -q \
      'backend actor message .*sender=.*neovim-session .*recipient=brood.client-init .*body=.*"action":"diff"' \
      "$CLIENT_LOG" \
    && rg -q \
      'backend actor message .*sender=.*neovim-session .*recipient=brood.client-init .*body=.*"action":"agent"' \
      "$CLIENT_LOG" \
    && rg -q 'started presentation WASM actor id=diff-viewer' "$CLIENT_LOG" \
    && rg -q 'started presentation WASM actor id=coding-agent-ui-' "$CLIENT_LOG"; then
    break
  fi
  kill -0 "$CLIENT_PID" 2> /dev/null || {
    echo "ERR presentation client exited during editor message routing" >&2
    exit 1
  }
  sleep 0.05
done
[[ -f "$TERMINAL_MARKER" ]]
rg -q \
  'backend actor message .*sender=.*neovim-session .*recipient=brood.client-init .*body=.*"action":"terminal"' \
  "$CLIENT_LOG"
rg -q \
  'backend actor message .*sender=.*neovim-session .*recipient=brood.client-init .*body=.*"action":"diff"' \
  "$CLIENT_LOG"
rg -q \
  'backend actor message .*sender=.*neovim-session .*recipient=brood.client-init .*body=.*"action":"agent"' \
  "$CLIENT_LOG"
rg -q 'backend actor message .*body=.*"kind":"editor-action"' "$CLIENT_LOG"
rg -q 'started presentation WASM actor id=diff-viewer' "$CLIENT_LOG"
rg -q 'started presentation WASM actor id=coding-agent-ui-' "$CLIENT_LOG"
rg -q 'terminal-session-2' "$SERVER_LOG" "$CLIENT_LOG"

sleep 0.5
capture "$OUT/editor-actions.png"
rg -a \
  'neovim-session|editor-action|diff-viewer|terminal-session-2|coding-agent-ui-' \
  "$SERVER_LOG" "$CLIENT_LOG" \
  > "$OUT/editor-routes.log"

STATUS="$(ctl status)"
STATUS_JSON="$(ctl status-json)"
rg -q 'presentation=Attached' <<< "$STATUS"
ctl actors list > "$OUT/actors-final.txt"
rg -q 'neovim-session' "$OUT/actors-final.txt"
rg -q 'terminal-session-2' "$OUT/actors-final.txt"

python3 - \
  "$OUT/editor-created.png" \
  "$OUT/editor-edited.png" \
  "$OUT/editor-inbound-open.png" \
  "$OUT/editor-actions.png" <<'PY'
import pathlib
import subprocess
import sys

for value in sys.argv[1:]:
    path = pathlib.Path(value)
    data = path.read_bytes()
    if not data.startswith(b"\x89PNG\r\n\x1a\n") or len(data) < 4096:
        raise SystemExit(f"invalid editor graphical proof: {path}")
    colors = int(
        subprocess.check_output(
            ["magick", str(path), "-format", "%k", "info:"], text=True
        )
    )
    if colors < 40:
        raise SystemExit(f"editor proof has too little rendered content: {path} colors={colors}")

for first, second, minimum in [
    (sys.argv[1], sys.argv[2], 0.001),
    (sys.argv[2], sys.argv[3], 0.001),
    (sys.argv[3], sys.argv[4], 0.002),
]:
    metric = subprocess.run(
        ["magick", "compare", "-metric", "RMSE", first, second, "null:"],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    ).stderr
    normalized = float(metric.rsplit("(", 1)[1].rstrip(")\n"))
    if normalized < minimum:
        raise SystemExit(
            f"editor captures are unexpectedly similar: {first} {second} RMSE={normalized}"
        )
    print(f"editor_capture_rmse={normalized:.6f}")
PY

EDITOR_METRIC="$(
  sed -n \
    "s/^presentation plugin worker metrics actor=${EDITOR_UI} //p" \
    "$CLIENT_LOG" \
    | tail -1
)"
jq -n \
  --argjson status "$STATUS_JSON" \
  --argjson editor "$EDITOR_METRIC" \
  --arg editor_actor "$EDITOR_ACTOR" \
  --arg editor_ui "$EDITOR_UI" \
  --arg edit_file "$EDIT_FILE" \
  --arg inbound_file "$INBOUND_FILE" \
  '{
    editor_actor: $editor_actor,
    editor_ui: $editor_ui,
    workload: {
      ctrl_alt_e: true,
      vim_normal_insert_write: true,
      edited_file: $edit_file,
      inbound_actor_message: {
        kind: "editor-open",
        file: $inbound_file,
        line: 2,
        column: 3
      },
      outbound_routes: ["diff", "terminal", "agent"],
      terminal_command_marker: true
    },
    render: $editor,
    status: $status
  }' > "$OUT/proof.json"

cat > "$OUT/proof.md" <<EOF
# Brood Neovim editor proof

- package: the Nix-built Brood package supplied brood, brood-client, brood-nvim, nvim, and the packaged actor Components
- isolation: private DBus session and headless Sway; no presentation window was created on the host desktop
- launch: a real Ctrl+Alt+E event created $EDITOR_ACTOR and rendered $EDITOR_UI
- editing: Normal/Insert-mode keys and :write added BROOD_EDITOR_REAL_KEYS to tracked.txt
- inbound messaging: actors send delivered editor-open with line 2, column 3; the subsequent Vim append produced sec__BROOD_INBOUND__ond inbound line
- outbound messaging: :BroodTerminal ran a command in terminal-session-2, :BroodDiff opened the diff-viewer actor, and :BroodAgent routed a bounded harmless prompt into a coding-agent pane
- screenshots: editor-created.png, editor-edited.png, editor-inbound-open.png, editor-actions.png
- structured evidence: proof.json, editor-routes.log, actors-editor-created.txt, actors-after-inbound.txt, actors-final.txt
- status: $STATUS
EOF

mkdir -p "$OUT/fixtures"
cp "$EDIT_FILE" "$OUT/fixtures/tracked.txt"
cp "$INBOUND_FILE" "$OUT/fixtures/inbound.txt"
cp "$ACTIONS_FILE" "$OUT/fixtures/editor-actions.vim"
cp "$TERMINAL_MARKER" "$OUT/fixtures/terminal-command-routed"

ctl quit > /dev/null 2>&1 || true
echo "editor_plugin_nested=verified"
echo "proof=$OUT/proof.md"
echo "metrics=$OUT/proof.json"
echo "created=$OUT/editor-created.png"
echo "edited=$OUT/editor-edited.png"
echo "inbound=$OUT/editor-inbound-open.png"
echo "actions=$OUT/editor-actions.png"
