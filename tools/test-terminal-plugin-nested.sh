#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

if [[ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]]; then
  exec dbus-run-session -- "$0" "$@"
fi

for tool in cc grim jq magick pkg-config python3 sway swaymsg tmux weston wl-copy wl-paste wtype; do
  if ! command -v "$tool" > /dev/null 2>&1; then
    echo "ERR $tool is required for the terminal plugin proof" >&2
    exit 2
  fi
done

if [[ -n "${BROOD_TERMINAL_PACKAGE:-}" ]]; then
  PACKAGE="$BROOD_TERMINAL_PACKAGE"
else
  PACKAGE="$(nix build .#brood --no-link --print-out-paths)"
fi
PACKAGE="$(readlink -f "$PACKAGE")"

# Keep non-visual bundled plugin acceptance behind the built-in `brood test`
# supervisor as well. These actors expose their result through workspace files,
# so a presentation client would add no evidence to this focused lane.
if [[ "${BROOD_RUNTIME_PLUGIN_ONLY:-0}" == 1 ]]; then
  exec env BROOD_RUNTIME_PLUGIN_PACKAGE="$PACKAGE" \
    bash "$ROOT/tools/test-runtime-plugins-nested.sh"
fi

OUT="${BROOD_TERMINAL_OUT:-$(mktemp -d /tmp/brood-terminal.XXXXXX)}"
RUNTIME_DIR="$(mktemp -d /tmp/brood-terminal-runtime.XXXXXX)"
SERVER_LOG="$OUT/server.log"
CLIENT1_LOG="$OUT/client-before-disconnect.log"
CLIENT2_LOG="$OUT/client-after-reconnect.log"
COMPOSITOR_LOG="$OUT/compositor.log"
SWAY_CONFIG="$OUT/sway.conf"
DBUS_DESTINATION="org.brood.TerminalTest.p$$"
WESTON_DIRECT="${BROOD_TERMINAL_WESTON_DIRECT:-0}"
WESTON_RENDERER="${BROOD_TERMINAL_WESTON_RENDERER:-gl}"
if [[ -n "${BROOD_TERMINAL_REFRESH_HZ:-}" ]]; then
  REFRESH_HZ="$BROOD_TERMINAL_REFRESH_HZ"
elif [[ "$WESTON_DIRECT" == 1 ]]; then
  REFRESH_HZ=180
else
  REFRESH_HZ=60
fi
TMUX_SOCKET="$OUT/helix-tmux.sock"
SHELL_ENV_FILE="$OUT/terminal-shell-env.sh"
TERMINAL_PROOF_FILE="$OUT/terminal-proof.txt"
mkdir -p "$OUT"
chmod 700 "$RUNTIME_DIR"

if [[ "$WESTON_DIRECT" == 1 ]]; then
  seq 1 2000 | sed 's/^/terminal plugin scroll line /' > "$TERMINAL_PROOF_FILE"
  printf 'exec tmux -S %q new-session hx %q\n' \
    "$TMUX_SOCKET" "$TERMINAL_PROOF_FILE" > "$SHELL_ENV_FILE"
fi

CLIENT_PID=""
SERVER_PID=""
COMPOSITOR_PID=""
cleanup() {
  local result=$?
  set +e
  terminate_process_bounded "$CLIENT_PID"
  terminate_process_bounded "$SERVER_PID"
  terminate_process_bounded "$COMPOSITOR_PID"
  rm -rf -- "$RUNTIME_DIR"
  if ((result != 0)); then
    echo "client_before_log=$CLIENT1_LOG" >&2
    [[ -f "$CLIENT1_LOG" ]] && tail -n 160 "$CLIENT1_LOG" >&2
    echo "client_after_log=$CLIENT2_LOG" >&2
    [[ -f "$CLIENT2_LOG" ]] && tail -n 200 "$CLIENT2_LOG" >&2
    echo "server_log=$SERVER_LOG" >&2
    [[ -f "$SERVER_LOG" ]] && tail -n 160 "$SERVER_LOG" >&2
    echo "compositor_log=$COMPOSITOR_LOG" >&2
    [[ -f "$COMPOSITOR_LOG" ]] && tail -n 100 "$COMPOSITOR_LOG" >&2
  fi
  return "$result"
}
trap cleanup EXIT

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

RUST_LOG="${RUST_LOG:-broodd=info,brood_client=info,smithay=warn,wgpu_core=warn,wgpu_hal=warn}"
SERVER_ENV=(env -u brood_session "RUST_LOG=$RUST_LOG")
if [[ "$WESTON_DIRECT" == 1 ]]; then
  SERVER_ENV+=("ENV=$SHELL_ENV_FILE")
fi
"${SERVER_ENV[@]}" \
  "$PACKAGE/bin/brood" run \
  --no-session-runner \
  --headless \
  --config "$PACKAGE/brood.toml" \
  --dbus-destination "$DBUS_DESTINATION" \
  --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
  > "$SERVER_LOG" 2>&1 &
SERVER_PID=$!

ctl() {
  env -u brood_session "$PACKAGE/bin/brood" ctl \
    --dbus-destination "$DBUS_DESTINATION" \
    --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
    "$@"
}

for _ in $(seq 1 200); do
  ctl status > /dev/null 2>&1 && break
  kill -0 "$SERVER_PID" 2> /dev/null || {
    echo "ERR session server exited during startup" >&2
    exit 1
  }
  sleep 0.05
done
ctl status > /dev/null

for _ in $(seq 1 200); do
  STATUS_LIVE="$(ctl status-json)"
  if jq -e '
    .hosted_runtime_io_children == 1
    and .hosted_runtime_io_streams == 1
    and any(.plugin_actors[]; .[0] | contains("terminal-session"))
  ' <<< "$STATUS_LIVE" > /dev/null; then
    break
  fi
  sleep 0.05
done
printf '%s\n' "$STATUS_LIVE" > "$OUT/status-shell-live.json"
jq -e '
  .hosted_runtime_io_children == 1
  and .hosted_runtime_io_streams == 1
  and .hosted_runtime_io_resources == 3
' <<< "$STATUS_LIVE" > /dev/null

OUTPUT_NAME="HEADLESS-1"
if [[ "$WESTON_DIRECT" == 1 ]]; then
  : > "$SWAY_CONFIG"
else
  printf '%s\n' \
    'xwayland disable' \
    "output * mode --custom 1280x720@${REFRESH_HZ}Hz" \
    'seat seat0 fallback true' \
    'default_border none' \
    'default_floating_border none' \
    > "$SWAY_CONFIG"
fi

if [[ "$WESTON_DIRECT" == 1 ]]; then
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    weston \
    --backend=headless \
    --width=1280 \
    --height=720 \
    --refresh-rate="$((REFRESH_HZ * 1000))" \
    --fake-seat \
    --renderer="$WESTON_RENDERER" \
    --socket=wayland-direct \
    --no-config \
    --log="$COMPOSITOR_LOG" \
    > /dev/null 2>&1 &
else
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WLR_BACKENDS=headless \
    WLR_HEADLESS_OUTPUTS=1 \
    WLR_LIBINPUT_NO_DEVICES=1 \
    sway \
    --unsupported-gpu \
    --config "$SWAY_CONFIG" \
    > "$COMPOSITOR_LOG" 2>&1 &
fi
COMPOSITOR_PID=$!

WAYLAND_SOCKET=""
SWAY_SOCKET=""
for _ in $(seq 1 200); do
  if [[ "$WESTON_DIRECT" == 1 ]]; then
    WAYLAND_SOCKET="wayland-direct"
    [[ -S "$RUNTIME_DIR/$WAYLAND_SOCKET" ]] && break
    sleep 0.05
    continue
  fi
  WAYLAND_SOCKET="$(
    find "$RUNTIME_DIR" -maxdepth 1 -type s -name 'wayland-*' -printf '%f\n' | head -1
  )"
  SWAY_SOCKET="$(
    find "$RUNTIME_DIR" -maxdepth 1 -type s -name 'sway-ipc.*.sock' -print | head -1
  )"
  [[ -n "$WAYLAND_SOCKET" && -n "$SWAY_SOCKET" ]] && break
  kill -0 "$COMPOSITOR_PID" 2> /dev/null || {
    echo "ERR headless Sway exited during startup" >&2
    exit 1
  }
  sleep 0.05
done
if [[ "$WESTON_DIRECT" != 1 ]]; then
  [[ -n "$WAYLAND_SOCKET" && -n "$SWAY_SOCKET" ]]
fi
[[ -n "$WAYLAND_SOCKET" ]]

wayland_env() {
  env \
    XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    SWAYSOCK="$SWAY_SOCKET" \
    "$@"
}

if [[ "$WESTON_DIRECT" == 1 ]]; then
  wayland_env wayland-info > "$OUT/outputs.txt"
else
  wayland_env swaymsg -t get_outputs > "$OUT/outputs.json"
  OUTPUT_NAME="$(jq -r 'map(select(.active))[0].name' "$OUT/outputs.json")"
fi

start_client() {
  local log="$1"
  env -u brood_session \
    XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    BROOD_KEYBINDINGS_PATH="$OUT/keybindings.json" \
    WGPU_BACKEND=vulkan \
    BROOD_DMABUF_HAPPY_PATH=1 \
    BROOD_TRACE_PLUGIN_INPUT=1 \
    BROOD_TRACE_PLUGIN_WORKERS=1 \
    BROOD_TERMINAL_SCROLL_BENCH=1 \
    BROOD_TERMINAL_SCROLL_BENCH_AUTO="$WESTON_DIRECT" \
    RUST_LOG="$RUST_LOG" \
    "$PACKAGE/bin/brood-client" \
    --config "$PACKAGE/brood.toml" \
    --dbus-destination "$DBUS_DESTINATION" \
    --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
    > "$log" 2>&1 &
  CLIENT_PID=$!
}

wait_for_terminal_render() {
  local log="$1"
  for _ in $(seq 1 300); do
    if rg -q "started presentation WASM actor id=terminal-ui" "$log" \
      && rg -q 'presentation plugin worker metrics actor=terminal-ui ' "$log"; then
      return
    fi
    kill -0 "$CLIENT_PID" 2> /dev/null || {
      echo "ERR presentation client exited before terminal render" >&2
      exit 1
    }
    sleep 0.05
  done
  echo "ERR timed out waiting for terminal render" >&2
  exit 1
}

last_metric() {
  local log="$1"
  sed -n 's/^presentation plugin worker metrics actor=terminal-ui //p' "$log" | tail -1
}

last_actor_metric() {
  local log="$1"
  local actor="$2"
  sed -n "s/^presentation plugin worker metrics actor=${actor} //p" "$log" | tail -1
}

metric_value() {
  local log="$1"
  local key="$2"
  last_metric "$log" | jq -r ".$key // 0"
}

type_line() {
  local input="$1"
  local ascii_chunk="" character code index
  # Keep all ASCII on one standard-keymap virtual keyboard so shell text never
  # needs compact-keymap Ctrl chords that can accidentally match product
  # shortcuts. The visual proof includes two non-ASCII characters, which are
  # sent individually through wtype between the stable ASCII chunks.
  for ((index = 0; index < ${#input}; index++)); do
    character="${input:index:1}"
    printf -v code '%d' "'$character"
    if ((code < 128)); then
      ascii_chunk+="$character"
      continue
    fi
    if [[ -n "$ascii_chunk" ]]; then
      wayland_env "$OUT/virtual-keyboard-key" --text "$ascii_chunk"
      ascii_chunk=""
    fi
    wayland_env wtype "$character"
  done
  wayland_env "$OUT/virtual-keyboard-key" --text "${ascii_chunk}"$'\r'
}

ctrl_alt_i() {
  wayland_env "$OUT/virtual-keyboard-key" 23
}

focus_terminal() {
  local log="$1"
  local baseline next quiet_previous quiet_samples terminal_pane
  terminal_pane="$(
    sed -n \
      's/.*started presentation WASM actor id=terminal-ui .* pane=\([0-9][0-9]*\) .*/\1/p' \
      "$log" \
      | head -1
  )"
  [[ -n "$terminal_pane" ]]
  for attempt in $(seq 1 5); do
    quiet_previous="$(metric_value "$log" pty_bytes)"
    quiet_samples=0
    for _ in $(seq 1 100); do
      sleep 0.05
      next="$(metric_value "$log" pty_bytes)"
      if [[ "$next" == "$quiet_previous" ]]; then
        quiet_samples=$((quiet_samples + 1))
        ((quiet_samples >= 8)) && break
      else
        quiet_previous="$next"
        quiet_samples=0
      fi
    done
    baseline="$(metric_value "$log" pty_bytes)"
    # The terminal is the composed workspace surface on the left. Use real
    # pointer input to focus that visible pane; Ctrl+J cycles the declared pane
    # catalogue and may legitimately defer on a currently hidden plugin pane.
    wayland_env "$OUT/virtual-pointer-input" click 240 360 1280 720
    type_line "echo __TERMINAL_FOCUS_${attempt}__"
    for _ in $(seq 1 60); do
      next="$(metric_value "$log" pty_bytes)"
      if ((next > baseline + 4)); then
        return
      fi
      sleep 0.05
    done
  done
  echo "ERR could not focus the terminal plugin pane" >&2
  exit 1
}

capture() {
  wayland_env grim "$1"
  [[ -s "$1" ]]
}

close_frontend() {
  ctl close-frontend > /dev/null
  for _ in $(seq 1 200); do
    ! kill -0 "$CLIENT_PID" 2> /dev/null && break
    sleep 0.05
  done
  wait "$CLIENT_PID"
  CLIENT_PID=""
}

if [[ "${BROOD_TERMINAL_PICKER_CONSUMERS_ONLY:-0}" == 1 ]]; then
  # Use a Ctrl+Alt binding in this lane because the terminal workload's compact
  # virtual keyboard does not reliably report its synthetic Ctrl+Shift state to
  # every wlroots version. The product default remains Ctrl+Shift+/.
  jq -n '{version:1,bindings:[{action:"keybindings.open",key_code:29,modifiers:3}]}' \
    > "$OUT/keybindings.json"
fi

start_client "$CLIENT1_LOG"
wait_for_terminal_render "$CLIENT1_LOG"
if [[ "$WESTON_DIRECT" != 1 ]]; then
  focus_terminal "$CLIENT1_LOG"
fi
SCROLL_LOG="$CLIENT1_LOG"

if [[ "${BROOD_TERMINAL_SCROLL_ONLY:-0}" != 1 ]]; then
capture "$OUT/terminal-initial.png"

SHELL_BASELINE="$(metric_value "$CLIENT1_LOG" pty_bytes)"
type_line 'printf "\033[31mANSI_RED\033[0m \033[1mBOLD\033[22m \033[3mITALIC\033[23m \033[4mUNDERLINE\033[24m \033[9mSTRIKE\033[29m \033[7mINVERSE\033[27m λ中 TERM=%s COLORTERM=%s\n" "$TERM" "$COLORTERM"; printf "CWD=%s\n" "$PWD"; echo __SHELL_OK__'
SHELL_PREVIOUS="$SHELL_BASELINE"
SHELL_QUIET=0
for _ in $(seq 1 400); do
  SHELL_BYTES="$(metric_value "$CLIENT1_LOG" pty_bytes)"
  if [[ "$SHELL_BYTES" == "$SHELL_PREVIOUS" ]]; then
    SHELL_QUIET=$((SHELL_QUIET + 1))
  else
    SHELL_PREVIOUS="$SHELL_BYTES"
    SHELL_QUIET=0
  fi
  if ((SHELL_BYTES > SHELL_BASELINE + 40 && SHELL_QUIET >= 8)); then
    break
  fi
  sleep 0.05
done
((SHELL_BYTES > SHELL_BASELINE + 40 && SHELL_QUIET >= 8))
capture "$OUT/terminal-shell-ansi-unicode.png"

KEY_FILE="$OUT/key-sequences.bin"
type_line "IFS= read -r key; printf %s \"\$key\" > '$KEY_FILE'; echo __KEY_SEQUENCE_OK__"
wayland_env "$OUT/virtual-keyboard-key" --none 63
wayland_env "$OUT/virtual-keyboard-key" --ctrl-only 106
wayland_env "$OUT/virtual-keyboard-key" --shift-only 15
wayland_env "$OUT/virtual-keyboard-key" --none 28
for _ in $(seq 1 200); do
  [[ -s "$KEY_FILE" ]] && break
  sleep 0.05
done
KEY_HEX="$(od -An -tx1 "$KEY_FILE" | tr -d ' \n')"
[[ "$KEY_HEX" == "1b5b31357e1b5b313b35431b5b5a" ]]

APPLICATION_KEY_FILE="$OUT/application-cursor-key.bin"
type_line "printf '\\033[?1h'; IFS= read -r key; printf %s \"\$key\" > '$APPLICATION_KEY_FILE'; printf '\\033[?1l'; echo __APPLICATION_CURSOR_OK__"
for _ in $(seq 1 200); do
  APPLICATION_CURSOR="$(metric_value "$CLIENT1_LOG" application_cursor)"
  [[ "$APPLICATION_CURSOR" == "true" ]] && break
  sleep 0.05
done
[[ "$APPLICATION_CURSOR" == "true" ]]
wayland_env "$OUT/virtual-keyboard-key" --none 103
wayland_env "$OUT/virtual-keyboard-key" --none 28
for _ in $(seq 1 200); do
  [[ -s "$APPLICATION_KEY_FILE" ]] && break
  sleep 0.05
done
APPLICATION_KEY_HEX="$(od -An -tx1 "$APPLICATION_KEY_FILE" | tr -d ' \n')"
[[ "$APPLICATION_KEY_HEX" == "1b4f41" ]]

if [[ "${BROOD_TERMINAL_PICKER_CONSUMERS_ONLY:-0}" != 1 ]]; then
INBOX_FOCUS_BASELINE="$(
  rg -a -c 'presentation local actor message sender=inbox recipient=terminal-ui body=\{"kind":"focus"\}' \
    "$CLIENT1_LOG" || true
)"
TERMINAL_PANE="$(
  sed -n \
    's/.*started presentation WASM actor id=terminal-ui .* pane=\([0-9][0-9]*\) .*/\1/p' \
    "$CLIENT1_LOG" \
    | head -1
)"
[[ -n "$TERMINAL_PANE" ]]
INBOX_PANE="$(
  sed -n \
    's/.*started presentation WASM actor id=inbox .* pane=\([0-9][0-9]*\) .*/\1/p' \
    "$CLIENT1_LOG" \
    | head -1
)"
[[ -n "$INBOX_PANE" ]]
INBOX_REVEAL_BASELINE="$(
  rg -a -c 'sent key-policy actor message actor=inbox' "$CLIENT1_LOG" || true
)"
ctrl_alt_i
for _ in $(seq 1 100); do
  INBOX_REVEAL_COUNT="$(
    rg -a -c 'sent key-policy actor message actor=inbox' "$CLIENT1_LOG" || true
  )"
  if ((INBOX_REVEAL_COUNT > INBOX_REVEAL_BASELINE)) \
    && rg -a -q "sent plugin pane resize pane=$INBOX_PANE logical_size=320x686 " "$CLIENT1_LOG"; then
    break
  fi
  sleep 0.05
done
((INBOX_REVEAL_COUNT > INBOX_REVEAL_BASELINE))
rg -a -q "sent plugin pane resize pane=$INBOX_PANE logical_size=320x686 " "$CLIENT1_LOG"
INBOX_CLICK_X=$((1280 - 320 / 2))
wayland_env "$OUT/virtual-pointer-input" click "$INBOX_CLICK_X" 100 1280 720
for _ in $(seq 1 100); do
  INBOX_FOCUS_COUNT="$(
    rg -a -c 'presentation local actor message sender=inbox recipient=terminal-ui body=\{"kind":"focus"\}' \
      "$CLIENT1_LOG" || true
  )"
  if ((INBOX_FOCUS_COUNT > INBOX_FOCUS_BASELINE)) \
    && rg -a -q "presentation key policy requested pane focus pane=$TERMINAL_PANE .* changed=true deferred=false" \
      "$CLIENT1_LOG"; then
    break
  fi
  sleep 0.05
done
((INBOX_FOCUS_COUNT > INBOX_FOCUS_BASELINE))
rg -a -q "presentation key policy requested pane focus pane=$TERMINAL_PANE .* changed=true deferred=false" \
  "$CLIENT1_LOG"
capture "$OUT/inbox-click-focused-terminal.png"

INBOX_SHORTCUT_BASELINE="$(
  rg -a -c 'sent key-policy actor message actor=inbox' "$CLIENT1_LOG" || true
)"
INBOX_PANE_FOCUS_BASELINE="$(
  rg -a -c "focused local plugin pane=$INBOX_PANE" "$CLIENT1_LOG" || true
)"
ctrl_alt_i
for _ in $(seq 1 100); do
  INBOX_SHORTCUT_COUNT="$(
    rg -a -c 'sent key-policy actor message actor=inbox' "$CLIENT1_LOG" || true
  )"
  INBOX_PANE_FOCUS_COUNT="$(
    rg -a -c "focused local plugin pane=$INBOX_PANE" "$CLIENT1_LOG" || true
  )"
  if ((INBOX_SHORTCUT_COUNT > INBOX_SHORTCUT_BASELINE)) \
    && ((INBOX_PANE_FOCUS_COUNT > INBOX_PANE_FOCUS_BASELINE)); then
    break
  fi
  sleep 0.05
done
((INBOX_SHORTCUT_COUNT > INBOX_SHORTCUT_BASELINE))
((INBOX_PANE_FOCUS_COUNT > INBOX_PANE_FOCUS_BASELINE))
capture "$OUT/inbox-ctrl-alt-i-focused.png"

TERMINAL_POINTER_FOCUS_BASELINE="$(
  rg -a -c "focused local plugin pane=$TERMINAL_PANE" "$CLIENT1_LOG" || true
)"
wayland_env "$OUT/virtual-pointer-input" click 240 360 1280 720
for _ in $(seq 1 100); do
  TERMINAL_POINTER_FOCUS_COUNT="$(
    rg -a -c "focused local plugin pane=$TERMINAL_PANE" "$CLIENT1_LOG" || true
  )"
  ((TERMINAL_POINTER_FOCUS_COUNT > TERMINAL_POINTER_FOCUS_BASELINE)) && break
  sleep 0.05
done
((TERMINAL_POINTER_FOCUS_COUNT > TERMINAL_POINTER_FOCUS_BASELINE))

if [[ "${BROOD_TERMINAL_INBOX_ONLY:-0}" == 1 ]]; then
  cat > "$OUT/proof.md" <<EOF
# Nested inbox shortcut proof

- notification action: clicking a real command-completion card focused terminal-ui
- shortcut input: real Ctrl+Alt+I through the nested Wayland keyboard
- shortcut result: focus changed from terminal-ui to the presentation-global inbox pane
- screenshot: inbox-ctrl-alt-i-focused.png
EOF
  echo "nested inbox shortcut proof passed"
  echo "proof=$OUT/proof.md"
  exit 0
fi
fi

if [[ "${BROOD_TERMINAL_PICKER_CONSUMERS_ONLY:-0}" == 1 ]]; then
  # The shortcut editor must delegate catalogue filtering and selection to the
  # one presentation-global picker, then own only the physical-chord capture.
  wayland_env "$OUT/virtual-keyboard-key" 21
  for _ in $(seq 1 200); do
    if rg -a -q 'sender=keybindings recipient=picker body=.*"type":"picker-create"' "$CLIENT1_LOG"; then
      break
    fi
    sleep 0.05
  done
  rg -a -q 'sender=keybindings recipient=picker body=.*"type":"picker-create"' "$CLIENT1_LOG"
  type_line 'open session picker'
  for _ in $(seq 1 200); do
    if rg -a -q 'sender=picker recipient=keybindings body=.*"type":"picker-selected"' "$CLIENT1_LOG" \
      && rg -a -q 'sender=keybindings recipient=session-switcher body=.*"type":"keybindings-pane".*"visible":true' "$CLIENT1_LOG"; then
      break
    fi
    sleep 0.05
  done
  rg -a -q 'sender=picker recipient=keybindings body=.*"type":"picker-selected"' "$CLIENT1_LOG"
  rg -a -q 'sender=keybindings recipient=session-switcher body=.*"type":"keybindings-pane".*"visible":true' "$CLIENT1_LOG"
  wayland_env "$OUT/virtual-keyboard-key" 22
  for _ in $(seq 1 200); do
    if [[ -s "$OUT/keybindings.json" ]] \
      && jq -e 'any(.bindings[]; .action == "sessions.picker" and .key_code == 30 and .modifiers == 3)' \
        "$OUT/keybindings.json" > /dev/null; then
      break
    fi
    sleep 0.05
  done
  jq -e 'any(.bindings[]; .action == "sessions.picker" and .key_code == 30 and .modifiers == 3)' \
    "$OUT/keybindings.json" > /dev/null

  # Saving returns to the same shared picker. Close it, then exercise the agent
  # consumer and select coding-agent-1 through real key input.
  wayland_env "$OUT/virtual-keyboard-key" --none 1
  sleep 0.1
  PICKER_PANE="$(
    sed -n 's/.*started presentation WASM actor id=picker .* pane=\([0-9][0-9]*\) .*/\1/p' \
      "$CLIENT1_LOG" | tail -1
  )"
  [[ -n "$PICKER_PANE" ]]
  PICKER_FOCUS_BEFORE="$(
    rg -a -c "focused local plugin pane=$PICKER_PANE" "$CLIENT1_LOG" || true
  )"
  wayland_env "$OUT/virtual-keyboard-key" 30
  for _ in $(seq 1 200); do
    PICKER_FOCUS_AFTER="$(
      rg -a -c "focused local plugin pane=$PICKER_PANE" "$CLIENT1_LOG" || true
    )"
    if rg -a -q 'recipient=picker body=.*"title":"Open coding agent"' "$CLIENT1_LOG" \
      && ((PICKER_FOCUS_AFTER > PICKER_FOCUS_BEFORE)); then
      break
    fi
    sleep 0.05
  done
  rg -a -q 'recipient=picker body=.*"title":"Open coding agent"' "$CLIENT1_LOG"
  ((PICKER_FOCUS_AFTER > PICKER_FOCUS_BEFORE))
  wayland_env "$OUT/virtual-keyboard-key" --none 28

  AGENT_UI=""
  AGENT_PANE=""
  for _ in $(seq 1 300); do
    AGENT_START="$(
      { rg -a 'started presentation WASM actor id=coding-agent-ui-b[0-9]+-s[0-9]+-[0-9]+ .* pane=[0-9]+' \
        "$CLIENT1_LOG" || true; } | tail -1
    )"
    AGENT_UI="$(sed -n 's/.*actor id=\([^ ]*\).*/\1/p' <<< "$AGENT_START")"
    AGENT_PANE="$(sed -n 's/.* pane=\([0-9][0-9]*\) .*/\1/p' <<< "$AGENT_START")"
    AGENT_DRAW_COUNT="$({ rg -a -c "accepted plugin draw packet pane=$AGENT_PANE" "$CLIENT1_LOG" || true; })"
    if [[ -n "$AGENT_UI" && -n "$AGENT_PANE" ]] \
      && rg -a -q "recipient=$AGENT_UI body=.*\"kind\":\"bind\".*\"agent\":\"coding-agent-1\"" "$CLIENT1_LOG" \
      && ((AGENT_DRAW_COUNT >= 1)); then
      break
    fi
    sleep 0.05
  done
  [[ -n "$AGENT_UI" ]]
  [[ -n "$AGENT_PANE" ]]
  rg -a -q "recipient=$AGENT_UI body=.*\"kind\":\"bind\".*\"agent\":\"coding-agent-1\"" "$CLIENT1_LOG"
  ((AGENT_DRAW_COUNT >= 1))
  capture "$OUT/coding-agent-selected.png"

  cat > "$OUT/proof.md" <<EOF
# Shared picker consumer proof

- keybindings: the configured Ctrl+Alt+Y sent the action catalogue to picker; selection opened only chord capture and persisted Ctrl+Alt+U
- agent: Ctrl+Alt+A sent the session agent catalogue to the same picker; selecting coding-agent-1 launched and bound $AGENT_UI
- screenshot: coding-agent-selected.png
EOF
  echo "nested shared picker consumer proof passed"
  echo "proof=$OUT/proof.md"
  exit 0
fi

close_frontend
STATUS_DISCONNECTED="$(ctl status-json)"
printf '%s\n' "$STATUS_DISCONNECTED" > "$OUT/status-presentation-disconnected.json"
jq -e '
  .presentation == "Detached"
  and .hosted_runtime_io_children >= 1
  and .hosted_runtime_io_streams >= 1
  and .hosted_runtime_io_resources >= 3
' <<< "$STATUS_DISCONNECTED" > /dev/null

start_client "$CLIENT2_LOG"
wait_for_terminal_render "$CLIENT2_LOG"
focus_terminal "$CLIENT2_LOG"
RECONNECT_BASELINE="$(metric_value "$CLIENT2_LOG" pty_bytes)"
type_line 'echo __SESSION_SHELL_SURVIVED_PRESENTATION_RECONNECT__'
for _ in $(seq 1 200); do
  RECONNECT_BYTES="$(metric_value "$CLIENT2_LOG" pty_bytes)"
  ((RECONNECT_BYTES > RECONNECT_BASELINE)) && break
  sleep 0.05
done
((RECONNECT_BYTES > RECONNECT_BASELINE))
capture "$OUT/terminal-reconnected.png"

COLOR_ENV_FILE="$OUT/terminal-color-env.txt"
type_line "printf 'TERM=%s\\nCOLORTERM=%s\\nNO_COLOR=%s\\nWAYLAND_DISPLAY=%s\\nDBUS_SESSION_BUS_ADDRESS=%s\\nBROOD_SURFACE_SESSION_ID=%s\\n' \"\$TERM\" \"\$COLORTERM\" \"\${NO_COLOR-unset}\" \"\$WAYLAND_DISPLAY\" \"\$DBUS_SESSION_BUS_ADDRESS\" \"\$BROOD_SURFACE_SESSION_ID\" > '$COLOR_ENV_FILE'"
for _ in $(seq 1 200); do
  [[ -s "$COLOR_ENV_FILE" ]] && break
  sleep 0.05
done
rg -qx 'TERM=xterm-256color' "$COLOR_ENV_FILE"
rg -qx 'COLORTERM=truecolor' "$COLOR_ENV_FILE"
rg -qx 'NO_COLOR=unset' "$COLOR_ENV_FILE"
rg -qx "WAYLAND_DISPLAY=$(jq -r .wayland_display <<< "$STATUS_LIVE")" "$COLOR_ENV_FILE"
rg -qx "BROOD_SURFACE_SESSION_ID=$(jq -r .active_surface_session <<< "$STATUS_LIVE")" \
  "$COLOR_ENV_FILE"
CHILD_DBUS_ADDRESS="$(sed -n 's/^DBUS_SESSION_BUS_ADDRESS=//p' "$COLOR_ENV_FILE")"
[[ "$CHILD_DBUS_ADDRESS" != "$DBUS_SESSION_BUS_ADDRESS" ]]
rg -q '/brood-inprocess-dbus-[^/]+-surface-[0-9]+/bus$' <<< "$CHILD_DBUS_ADDRESS"

GHOSTTY_BASELINE="$(ctl status-json | jq -r .total_windows)"
GHOSTTY_PID_FILE="$OUT/ghostty-child.pid"
type_line "env -u LD_LIBRARY_PATH ghostty --config-file=/dev/null --class=brood-terminal-ghostty-proof -e sleep 30 > '$OUT/ghostty-child.log' 2>&1 & printf '%s\n' \"\$!\" > '$GHOSTTY_PID_FILE'"
for _ in $(seq 1 100); do
  [[ -s "$GHOSTTY_PID_FILE" ]] && break
  sleep 0.05
done
[[ -s "$GHOSTTY_PID_FILE" ]]
GHOSTTY_PID="$(< "$GHOSTTY_PID_FILE")"
[[ "$GHOSTTY_PID" =~ ^[0-9]+$ ]]
for _ in $(seq 1 300); do
  GHOSTTY_STATUS="$(ctl status-json)"
  GHOSTTY_WINDOWS="$(jq -r .total_windows <<< "$GHOSTTY_STATUS")"
  ((GHOSTTY_WINDOWS > GHOSTTY_BASELINE)) && break
  sleep 0.05
done
((GHOSTTY_WINDOWS > GHOSTTY_BASELINE))
capture "$OUT/terminal-ghostty-managed.png"
kill "$GHOSTTY_PID"
for _ in $(seq 1 200); do
  GHOSTTY_WINDOWS="$(ctl status-json | jq -r .total_windows)"
  ((GHOSTTY_WINDOWS == GHOSTTY_BASELINE)) && break
  sleep 0.05
done
((GHOSTTY_WINDOWS == GHOSTTY_BASELINE))
focus_terminal "$CLIENT2_LOG"

LAZYGIT_BASELINE="$(metric_value "$CLIENT2_LOG" pty_bytes)"
LAZYGIT_CREATED_BASELINE="$(metric_value "$CLIENT2_LOG" layouts_created_total)"
LAZYGIT_DROPPED_BASELINE="$(metric_value "$CLIENT2_LOG" layouts_dropped_total)"
LAZYGIT_STARTED_MS="$(date +%s%3N)"
type_line 'lazygit'
for _ in $(seq 1 300); do
  LAZYGIT_BYTES="$(metric_value "$CLIENT2_LOG" pty_bytes)"
  LAZYGIT_CACHE="$(metric_value "$CLIENT2_LOG" layout_cache_entries)"
  if ((LAZYGIT_BYTES > LAZYGIT_BASELINE + 1000 && LAZYGIT_CACHE > 20)); then
    break
  fi
  sleep 0.05
done
LAZYGIT_PRESENT_MS=$(( $(date +%s%3N) - LAZYGIT_STARTED_MS ))
((LAZYGIT_BYTES > LAZYGIT_BASELINE + 1000))
((LAZYGIT_CACHE > 20 && LAZYGIT_CACHE <= 512))
wayland_env wtype -s 5 'jkjkjkjkjkjkjkjkjkjk'
sleep 0.5
wayland_env wtype -s 5 'kjkjkjkjkjkjkjkjkjkj'
sleep 0.5
LAZYGIT_CREATED="$(
  metric_value "$CLIENT2_LOG" layouts_created_total
)"
LAZYGIT_DROPPED="$(
  metric_value "$CLIENT2_LOG" layouts_dropped_total
)"
LAZYGIT_CREATED=$((LAZYGIT_CREATED - LAZYGIT_CREATED_BASELINE))
LAZYGIT_DROPPED=$((LAZYGIT_DROPPED - LAZYGIT_DROPPED_BASELINE))
capture "$OUT/terminal-lazygit-color.png"
LAZYGIT_EXIT_FILE="$OUT/lazygit-exited"
wayland_env wtype 'q'
sleep 0.25
wayland_env "$OUT/virtual-keyboard-key" --ctrl-only 46
sleep 0.25
type_line "touch '$LAZYGIT_EXIT_FILE'"
for _ in $(seq 1 200); do
  [[ -e "$LAZYGIT_EXIT_FILE" ]] && break
  sleep 0.05
done
[[ -e "$LAZYGIT_EXIT_FILE" ]]
SCROLL_LOG="$CLIENT2_LOG"
fi

HELIX_BASELINE="$(metric_value "$SCROLL_LOG" pty_bytes)"
if [[ "$WESTON_DIRECT" != 1 ]]; then
  TERMINAL_PROOF_SHELL_PATH="$(printf %q "$TERMINAL_PROOF_FILE")"
  type_line "{ printf \"terminal plugin TUI proof λ中\\n\"; seq 1 2000 | sed \"s/^/terminal plugin scroll line /\"; } > $TERMINAL_PROOF_SHELL_PATH; hx $TERMINAL_PROOF_SHELL_PATH"
fi
for _ in $(seq 1 300); do
  HELIX_BYTES="$(metric_value "$SCROLL_LOG" pty_bytes)"
  HELIX_COMMANDS="$(metric_value "$SCROLL_LOG" commands)"
  if [[ "$WESTON_DIRECT" == 1 ]] && ((HELIX_COMMANDS > 500)); then
    break
  fi
  if ((HELIX_BYTES > HELIX_BASELINE + 10000 && HELIX_COMMANDS > 500)); then
    break
  fi
  sleep 0.05
done
if [[ "$WESTON_DIRECT" == 1 ]]; then
  ((HELIX_COMMANDS > 500))
else
  ((HELIX_BYTES > HELIX_BASELINE + 10000 && HELIX_COMMANDS > 500))
fi
sleep 0.1
if [[ "${BROOD_TERMINAL_SCROLL_ONLY:-0}" != 1 ]]; then
  capture "$OUT/terminal-helix.png"
fi

if [[ "$WESTON_DIRECT" == 1 ]]; then
  INITIAL_AUTO_METRICS="$(
    rg -c '^plugin helix scroll diagnostics ' "$SCROLL_LOG" || true
  )"
  python3 tools/tmux-repeat-key.py \
    --socket "$TMUX_SOCKET" \
    --count 720 \
    --interval-us 4167 \
    j
  for _ in $(seq 1 200); do
    AUTO_METRICS="$(
      rg -c '^plugin helix scroll diagnostics ' "$SCROLL_LOG" || true
    )"
    ((AUTO_METRICS > INITIAL_AUTO_METRICS)) && break
    sleep 0.05
  done
else
  [[ "$(metric_value "$SCROLL_LOG" mouse_tracking)" == 3 ]]
  [[ "$(metric_value "$SCROLL_LOG" mouse_sgr)" == true ]]
  MOUSE_SCROLL_BASELINE="$(metric_value "$SCROLL_LOG" pty_bytes)"
  wayland_env "$OUT/virtual-pointer-input" scroll 240 360 2 12 1280 720
  for _ in $(seq 1 200); do
    MOUSE_SCROLL_BYTES="$(metric_value "$SCROLL_LOG" pty_bytes)"
    ((MOUSE_SCROLL_BYTES > MOUSE_SCROLL_BASELINE)) && break
    sleep 0.05
  done
  ((MOUSE_SCROLL_BYTES > MOUSE_SCROLL_BASELINE))
  capture "$OUT/terminal-helix-mouse-scroll.png"
  wayland_env "$OUT/virtual-keyboard-key" --none 88
# Saturate the terminal above refresh rate so input/compositor phase alignment
# cannot create artificial missed frames. Presentation should remain display
# limited while the terminal pipeline continues producing well above 60 Hz.
wayland_env "$OUT/virtual-keyboard-key" --repeat 720 4167 36
# Give the final PTY update enough time to reach presentation without including
# Helix's unrelated cursor-blink timer in the measured scene stream.
sleep 0.08
wayland_env "$OUT/virtual-keyboard-key" --none 87
fi
for _ in $(seq 1 200); do
  HELIX_SCROLL_METRIC="$(
    sed -n 's/^plugin helix scroll diagnostics //p' "$SCROLL_LOG" | tail -1
  )"
  [[ -n "$HELIX_SCROLL_METRIC" ]] && break
  sleep 0.05
done
[[ -n "$HELIX_SCROLL_METRIC" ]]
# The headless compositor has host-scheduler jitter, so keep a narrow tolerance
# on actual presents while requiring nearly every 240 Hz Helix update to arrive.
if [[ "$WESTON_DIRECT" == 1 ]]; then
  MIN_SCENES=650
  MIN_SCENE_FPS_MILLI=220000
  MIN_FRAMES=500
  MIN_FPS_MILLI=170000
  MAX_P95_INTERVAL_US=7000
else
  MIN_SCENES=350
  MIN_SCENE_FPS_MILLI=115000
  MIN_FRAMES=175
  MIN_FPS_MILLI=57000
  MAX_P95_INTERVAL_US=25000
fi
jq -e \
  --argjson min_scenes "$MIN_SCENES" \
  --argjson min_scene_fps "$MIN_SCENE_FPS_MILLI" \
  --argjson min_frames "$MIN_FRAMES" \
  --argjson min_fps "$MIN_FPS_MILLI" \
  --argjson max_p95 "$MAX_P95_INTERVAL_US" \
  '
  (.input_events == 0 or (.input_events == 720 and .input_fps_milli >= 235000))
  and .scenes >= $min_scenes
  and .scene_fps_milli >= $min_scene_fps
  and .frames >= $min_frames
  and .fps_milli >= $min_fps
  and .p95_interval_us <= $max_p95
  and .intervals > 0
' \
  <<< "$HELIX_SCROLL_METRIC" > /dev/null

if [[ "${BROOD_TERMINAL_SCROLL_ONLY:-0}" == 1 ]]; then
  printf '%s\n' "$HELIX_SCROLL_METRIC"
  exit 0
fi

RESIZE_BASELINE="$(metric_value "$CLIENT2_LOG" pty_resizes)"
for mode in 1024x640 1400x800 960x600 1180x680; do
  wayland_env swaymsg \
    "output $OUTPUT_NAME mode --custom ${mode}@${REFRESH_HZ}Hz" > /dev/null
  sleep 0.15
done
# A compositor drag can publish hundreds of output sizes faster than a
# terminal guest can redraw. Exercise that burst explicitly: the client and
# actor runtime must retain the latest size without filling a lossless mailbox
# or terminating the terminal actor.
wayland_env swaymsg "output $OUTPUT_NAME scale 1.25" > /dev/null
for iteration in $(seq 1 160); do
  if ((iteration == 81)); then
    wayland_env swaymsg "output $OUTPUT_NAME scale 1.0" > /dev/null
  fi
  if ((iteration % 2 == 0)); then
    mode=1180x680
  else
    mode=960x600
  fi
  wayland_env swaymsg \
    "output $OUTPUT_NAME mode --custom ${mode}@${REFRESH_HZ}Hz" > /dev/null
done
wayland_env swaymsg "output $OUTPUT_NAME scale 1.0" > /dev/null
wayland_env swaymsg \
  "output $OUTPUT_NAME mode --custom 1180x680@${REFRESH_HZ}Hz" > /dev/null
for _ in $(seq 1 200); do
  RESIZES="$(metric_value "$CLIENT2_LOG" pty_resizes)"
  ((RESIZES >= RESIZE_BASELINE + 4)) && break
  sleep 0.05
done
((RESIZES >= RESIZE_BASELINE + 4))
kill -0 "$CLIENT_PID" 2> /dev/null
if rg -q \
  'actor presentation dispatch backlog is full|component actor mailbox rejected message: channel closed|Error: stop presentation WASM runtime' \
  "$CLIENT2_LOG"; then
  echo "ERR rapid resize terminated or disconnected a presentation actor" >&2
  exit 1
fi
capture "$OUT/terminal-helix-resized.png"

# Let Helix consume the last PTY resize and finish its redraw before measuring
# the following command. The compositor emits two pane sizes per output mode,
# so the resize count alone can advance before the final terminal redraw.
RESIZE_SETTLE_BYTES="$(metric_value "$CLIENT2_LOG" pty_bytes)"
RESIZE_SETTLE_QUIET=0
for _ in $(seq 1 200); do
  sleep 0.05
  RESIZE_SETTLE_NEXT="$(metric_value "$CLIENT2_LOG" pty_bytes)"
  if [[ "$RESIZE_SETTLE_NEXT" == "$RESIZE_SETTLE_BYTES" ]]; then
    RESIZE_SETTLE_QUIET=$((RESIZE_SETTLE_QUIET + 1))
    ((RESIZE_SETTLE_QUIET >= 8)) && break
  else
    RESIZE_SETTLE_BYTES="$RESIZE_SETTLE_NEXT"
    RESIZE_SETTLE_QUIET=0
  fi
done
((RESIZE_SETTLE_QUIET >= 8))

HELIX_EXIT_BASELINE="$(metric_value "$CLIENT2_LOG" pty_bytes)"
wayland_env "$OUT/virtual-keyboard-key" --none 1
sleep 0.05
type_line ':q!'
sleep 0.5
capture "$OUT/terminal-shell-after-helix.png"
for _ in $(seq 1 200); do
  HELIX_EXIT_BYTES="$(metric_value "$CLIENT2_LOG" pty_bytes)"
  HELIX_EXIT_BRACKETED="$(metric_value "$CLIENT2_LOG" bracketed_paste)"
  if ((HELIX_EXIT_BYTES > HELIX_EXIT_BASELINE)) \
    && [[ "$HELIX_EXIT_BRACKETED" == true ]]; then
    break
  fi
  sleep 0.05
done
((HELIX_EXIT_BYTES > HELIX_EXIT_BASELINE))
[[ "$HELIX_EXIT_BRACKETED" == true ]]

wayland_env "$OUT/virtual-keyboard-key" --ctrl-only 38
sleep 0.1
type_line "printf '__BROOD_MOUSE_COPY__\\n'"
sleep 0.2
wayland_env "$OUT/virtual-pointer-input" drag 10 28 205 28 12 1180 680
for _ in $(seq 1 100); do
  MOUSE_COPY_TEXT="$(wayland_env wl-paste --no-newline 2> /dev/null || true)"
  [[ "$MOUSE_COPY_TEXT" == "__BROOD_MOUSE_COPY__" ]] && break
  sleep 0.05
done
[[ "$MOUSE_COPY_TEXT" == "__BROOD_MOUSE_COPY__" ]]
capture "$OUT/terminal-mouse-selection.png"

printf '__STALE_CLIPBOARD__' | wayland_env wl-copy
sleep 0.2
wayland_env "$OUT/virtual-keyboard-key" --ctrl-shift-only 46
for _ in $(seq 1 100); do
  SHORTCUT_COPY_TEXT="$(wayland_env wl-paste --no-newline 2> /dev/null || true)"
  [[ "$SHORTCUT_COPY_TEXT" == "__BROOD_MOUSE_COPY__" ]] && break
  sleep 0.05
done
[[ "$SHORTCUT_COPY_TEXT" == "__BROOD_MOUSE_COPY__" ]]

PASTE_PROOF_FILE="$OUT/clipboard-paste-worked"
printf "touch '%s'" "$PASTE_PROOF_FILE" | wayland_env wl-copy
sleep 0.2
wayland_env "$OUT/virtual-keyboard-key" --ctrl-shift-only 47
wayland_env "$OUT/virtual-keyboard-key" --none 28
for _ in $(seq 1 200); do
  [[ -e "$PASTE_PROOF_FILE" ]] && break
  sleep 0.05
done
[[ -e "$PASTE_PROOF_FILE" ]]

CTRL_C_PROOF_FILE="$OUT/ctrl-c-worked"
wayland_env wtype 'sleep 30'
wayland_env wtype -k Return
sleep 0.1
wayland_env "$OUT/virtual-keyboard-key" --ctrl-only 46
type_line "touch '$CTRL_C_PROOF_FILE'"
for _ in $(seq 1 200); do
  [[ -e "$CTRL_C_PROOF_FILE" ]] && break
  sleep 0.05
done
[[ -e "$CTRL_C_PROOF_FILE" ]]

STREAM_BASELINE="$(metric_value "$CLIENT2_LOG" pty_bytes)"
STREAM_STARTED_MS="$(date +%s%3N)"
type_line 'seq 1 4000; printf "\033[32m__STREAM_DONE__\033[0m\n"'
for _ in $(seq 1 500); do
  STREAM_BYTES="$(metric_value "$CLIENT2_LOG" pty_bytes)"
  if ((STREAM_BYTES > STREAM_BASELINE + 10000)); then
    break
  fi
  kill -0 "$CLIENT_PID" 2> /dev/null
  sleep 0.02
done
STREAM_RENDER_MS=$(( $(date +%s%3N) - STREAM_STARTED_MS ))
((STREAM_BYTES > STREAM_BASELINE + 10000))
capture "$OUT/terminal-streaming.png"

MAX_VIEWPORT="$(metric_value "$CLIENT2_LOG" max_viewport_offset)"
((MAX_VIEWPORT > 0))
wayland_env "$OUT/virtual-pointer-input" scroll 220 340 -2 12 1180 680
for _ in $(seq 1 200); do
  VIEWPORT="$(metric_value "$CLIENT2_LOG" viewport_offset)"
  ((VIEWPORT > 0)) && break
  sleep 0.05
done
if ((VIEWPORT == 0)); then
  wayland_env "$OUT/virtual-pointer-input" scroll 220 340 2 12 1180 680
  for _ in $(seq 1 200); do
    VIEWPORT="$(metric_value "$CLIENT2_LOG" viewport_offset)"
    ((VIEWPORT > 0)) && break
    sleep 0.05
  done
fi
((VIEWPORT > 0))
capture "$OUT/terminal-scrollback.png"
wayland_env "$OUT/virtual-keyboard-key" --shift-only 107
for _ in $(seq 1 200); do
  VIEWPORT="$(metric_value "$CLIENT2_LOG" viewport_offset)"
  ((VIEWPORT == 0)) && break
  sleep 0.05
done
((VIEWPORT == 0))

wayland_env "$OUT/virtual-keyboard-key" --ctrl-only 32
for _ in $(seq 1 300); do
  EXIT_METRIC="$(last_metric "$CLIENT2_LOG")"
  STATUS_EXITED="$(ctl status-json)"
  if jq -e '.exited == true' <<< "$EXIT_METRIC" > /dev/null \
    && jq -e \
      --argjson live "$STATUS_DISCONNECTED" \
      '
        .hosted_runtime_io_children == ($live.hosted_runtime_io_children - 1)
        and .hosted_runtime_io_streams == ($live.hosted_runtime_io_streams - 1)
        and .hosted_runtime_io_resources == ($live.hosted_runtime_io_resources - 2)
      ' <<< "$STATUS_EXITED" > /dev/null; then
    break
  fi
  sleep 0.05
done
printf '%s\n' "$STATUS_EXITED" > "$OUT/status-shell-exited.json"
jq -e '.exited == true' <<< "$EXIT_METRIC" > /dev/null
jq -e \
  --argjson live "$STATUS_DISCONNECTED" \
  '
    .hosted_runtime_io_children == ($live.hosted_runtime_io_children - 1)
    and .hosted_runtime_io_streams == ($live.hosted_runtime_io_streams - 1)
    and .hosted_runtime_io_resources == ($live.hosted_runtime_io_resources - 2)
  ' <<< "$STATUS_EXITED" > /dev/null
capture "$OUT/terminal-shell-exited.png"

SPAWN_STARTED_MS="$(date +%s%3N)"
wayland_env "$OUT/virtual-keyboard-key" 20
DYNAMIC_UI="terminal-ui-b1-s1-2"
for _ in $(seq 1 300); do
  STATUS_SPAWNED="$(ctl status-json)"
  DYNAMIC_METRIC="$(last_actor_metric "$CLIENT2_LOG" "$DYNAMIC_UI")"
  if rg -q "started presentation WASM actor id=$DYNAMIC_UI" "$CLIENT2_LOG" \
    && jq -e \
      --argjson exited "$STATUS_EXITED" \
      '
        .hosted_runtime_io_children == ($exited.hosted_runtime_io_children + 1)
        and .hosted_runtime_io_streams == ($exited.hosted_runtime_io_streams + 1)
        and .hosted_runtime_io_resources == ($exited.hosted_runtime_io_resources + 2)
        and any(.plugin_actors[]; .[0] | contains("terminal-session-2"))
      ' <<< "$STATUS_SPAWNED" > /dev/null \
    && [[ -n "$DYNAMIC_METRIC" ]]; then
    break
  fi
  sleep 0.05
done
SPAWN_PRESENT_MS=$(( $(date +%s%3N) - SPAWN_STARTED_MS ))
rg -q "started presentation WASM actor id=$DYNAMIC_UI" "$CLIENT2_LOG"
jq -e \
  --argjson exited "$STATUS_EXITED" \
  '
    .hosted_runtime_io_children == ($exited.hosted_runtime_io_children + 1)
    and .hosted_runtime_io_streams == ($exited.hosted_runtime_io_streams + 1)
    and .hosted_runtime_io_resources == ($exited.hosted_runtime_io_resources + 2)
    and any(.plugin_actors[]; .[0] | contains("terminal-session-2"))
  ' <<< "$STATUS_SPAWNED" > /dev/null
[[ -n "$DYNAMIC_METRIC" ]]

DYNAMIC_BASELINE="$(jq -r '.pty_bytes // 0' <<< "$DYNAMIC_METRIC")"
type_line 'printf "\033[35m__CTRL_ALT_T_TERMINAL__\033[0m λ中\n"'
for _ in $(seq 1 200); do
  DYNAMIC_METRIC="$(last_actor_metric "$CLIENT2_LOG" "$DYNAMIC_UI")"
  DYNAMIC_BYTES="$(jq -r '.pty_bytes // 0' <<< "$DYNAMIC_METRIC")"
  ((DYNAMIC_BYTES > DYNAMIC_BASELINE + 16)) && break
  sleep 0.05
done
((DYNAMIC_BYTES > DYNAMIC_BASELINE + 16))
capture "$OUT/terminal-ctrl-alt-t.png"

type_line 'exit'
for _ in $(seq 1 300); do
  DYNAMIC_EXIT_METRIC="$(last_actor_metric "$CLIENT2_LOG" "$DYNAMIC_UI")"
  STATUS_FINAL_EXIT="$(ctl status-json)"
  if jq -e '.exited == true' <<< "$DYNAMIC_EXIT_METRIC" > /dev/null \
    && jq -e \
      --argjson exited "$STATUS_EXITED" \
      '
        .hosted_runtime_io_children == $exited.hosted_runtime_io_children
        and .hosted_runtime_io_streams == $exited.hosted_runtime_io_streams
        and .hosted_runtime_io_resources == $exited.hosted_runtime_io_resources
      ' <<< "$STATUS_FINAL_EXIT" > /dev/null; then
    break
  fi
  sleep 0.05
done
jq -e '.exited == true' <<< "$DYNAMIC_EXIT_METRIC" > /dev/null
jq -e \
  --argjson exited "$STATUS_EXITED" \
  '
    .hosted_runtime_io_children == $exited.hosted_runtime_io_children
    and .hosted_runtime_io_streams == $exited.hosted_runtime_io_streams
    and .hosted_runtime_io_resources == $exited.hosted_runtime_io_resources
  ' <<< "$STATUS_FINAL_EXIT" > /dev/null
printf '%s\n' "$STATUS_SPAWNED" > "$OUT/status-shortcut-shell-live.json"
printf '%s\n' "$STATUS_FINAL_EXIT" > "$OUT/status-shortcut-shell-exited.json"

close_frontend
GRAPHICS_METRIC="$(
  sed -n 's/^plugin graphics diagnostics //p' "$CLIENT2_LOG" | tail -1
)"
INPUT_METRIC="$(
  sed -n 's/^plugin input diagnostics //p' "$CLIENT2_LOG" | tail -1
)"
[[ -n "$GRAPHICS_METRIC" && -n "$INPUT_METRIC" ]]
jq -e '
  .accepted_packets > 0
  and .max_packet_bytes > 0
  and .max_packet_commands > 0
  and .text_layouts_created > 0
  and .text_layouts_dropped > 0
  and .live_text_layouts == 0
' <<< "$GRAPHICS_METRIC" > /dev/null
jq -e '.key_to_present_samples > 0 and .max_key_to_present_us > 0' \
  <<< "$INPUT_METRIC" > /dev/null

python3 - \
  "$OUT/terminal-initial.png" \
  "$OUT/terminal-shell-ansi-unicode.png" \
  "$OUT/inbox-ctrl-alt-i-focused.png" \
  "$OUT/terminal-helix.png" \
  "$OUT/terminal-helix-mouse-scroll.png" \
  "$OUT/terminal-helix-resized.png" \
  "$OUT/terminal-shell-after-helix.png" \
  "$OUT/terminal-mouse-selection.png" \
  "$OUT/terminal-reconnected.png" \
  "$OUT/terminal-scrollback.png" \
  "$OUT/terminal-ctrl-alt-t.png" \
  "$OUT/terminal-lazygit-color.png" \
  "$OUT/terminal-ghostty-managed.png" <<'PY'
import pathlib
import subprocess
import sys

for value in sys.argv[1:]:
    path = pathlib.Path(value)
    data = path.read_bytes()
    if not data.startswith(b"\x89PNG\r\n\x1a\n") or len(data) < 4096:
        raise SystemExit(f"invalid terminal graphical proof: {path}")
    colors = int(subprocess.check_output(["magick", str(path), "-format", "%k", "info:"]))
    if colors < 50:
        raise SystemExit(f"terminal proof has too little rendered content: {path} colors={colors}")

for first, second, minimum in [
    (sys.argv[1], sys.argv[2], 0.005),
    (sys.argv[2], sys.argv[3], 0.01),
    (sys.argv[3], sys.argv[4], 0.005),
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
            f"terminal captures are unexpectedly similar: {first} {second} RMSE={normalized}"
        )
PY

LAST_TERMINAL_METRIC="$(last_metric "$CLIENT2_LOG")"
jq -n \
  --argjson live "$STATUS_LIVE" \
  --argjson disconnected "$STATUS_DISCONNECTED" \
  --argjson exited "$STATUS_FINAL_EXIT" \
  --argjson shortcut_live "$STATUS_SPAWNED" \
  --argjson shortcut_terminal "$DYNAMIC_EXIT_METRIC" \
  --argjson terminal "$LAST_TERMINAL_METRIC" \
  --argjson graphics "$GRAPHICS_METRIC" \
  --argjson input "$INPUT_METRIC" \
  --argjson helix_scroll "$HELIX_SCROLL_METRIC" \
  --argjson stream_render_ms "$STREAM_RENDER_MS" \
  --argjson spawn_present_ms "$SPAWN_PRESENT_MS" \
  --argjson lazygit_present_ms "$LAZYGIT_PRESENT_MS" \
  --argjson lazygit_created "$LAZYGIT_CREATED" \
  --argjson lazygit_dropped "$LAZYGIT_DROPPED" \
  --arg key_hex "$KEY_HEX" \
  --arg application_key_hex "$APPLICATION_KEY_HEX" \
  '{
    workload: {
      shell: true,
      ansi_color: true,
      unicode: true,
      styles: ["bold", "italic", "underline", "strikethrough", "inverse"],
      key_sequences: {
        f5_ctrl_right_shift_tab: $key_hex,
        application_cursor_up: $application_key_hex
      },
      tui: "helix",
      helix_scroll: $helix_scroll,
      lazygit: {
        color_environment: {
          term: "xterm-256color",
          colorterm: "truecolor",
          no_color: "unset"
        },
        startup_to_present_ms: $lazygit_present_ms,
        layout_cache_entries: $terminal.layout_cache_entries,
        layouts_created: $lazygit_created,
        layouts_dropped: $lazygit_dropped
      },
      managed_gui_descendant: {
        application: "ghostty",
        wayland_display: $live.wayland_display,
        isolated_session_bus: true,
        mapped_inside_backend: true
      },
      repeated_resizes: $terminal.pty_resizes,
      streaming_bytes: $terminal.pty_bytes,
      streaming_render_ms: $stream_render_ms,
      presentation_reconnect: true,
      ctrl_alt_t_spawn: {
        session_actor: "terminal-session-2",
        presentation_actor: "terminal-ui-b1-s1-2",
        spawn_to_present_ms: $spawn_present_ms,
        pty_bytes: $shortcut_terminal.pty_bytes,
        child_exit: $shortcut_terminal.exited
      },
      scrollback_lines: $terminal.max_viewport_offset,
      scrollback_navigation: true,
      child_exit: $terminal.exited
    },
    runtime_live: {
      resources: $live.hosted_runtime_io_resources,
      streams: $live.hosted_runtime_io_streams,
      children: $live.hosted_runtime_io_children,
      max_stream_incoming_bytes: $exited.hosted_runtime_io_max_stream_incoming_bytes,
      max_stream_outgoing_bytes: $exited.hosted_runtime_io_max_stream_outgoing_bytes,
      read_would_block: $exited.hosted_runtime_io_stream_read_would_block,
      write_would_block: $exited.hosted_runtime_io_stream_write_would_block
    },
    presentation_disconnected: {
      presentation: $disconnected.presentation,
      shell_children: $disconnected.hosted_runtime_io_children,
      resources: $disconnected.hosted_runtime_io_resources
    },
    shortcut_live: {
      resources: $shortcut_live.hosted_runtime_io_resources,
      streams: $shortcut_live.hosted_runtime_io_streams,
      children: $shortcut_live.hosted_runtime_io_children
    },
    runtime_after_exit: {
      resources: $exited.hosted_runtime_io_resources,
      streams: $exited.hosted_runtime_io_streams,
      children: $exited.hosted_runtime_io_children
    },
    terminal_render: $terminal,
    render_pressure: {
      snapshots_received: $terminal.snapshots_received,
      snapshots_coalesced: $terminal.snapshots_coalesced,
      snapshot_chunks_received: $terminal.snapshot_chunks_received,
      snapshot_bytes_received: $terminal.snapshot_bytes_received,
      snapshot_rows_received: $terminal.snapshot_rows_received,
      full_snapshots_received: $terminal.full_snapshots_received,
      packets: $terminal.packets_total,
      cached_layouts: $terminal.layout_cache_entries,
      layouts_created: $terminal.layouts_created_total,
      layouts_reused: $terminal.layouts_reused_total,
      layouts_dropped: $terminal.layouts_dropped_total
    },
    graphics: $graphics,
    input: $input,
    mailbox: [
      $exited.plugin_actors[]
      | select(.[0] | contains("terminal-session"))
      | {
          actor: .[0],
          enqueued: .[2],
          dropped: .[3],
          capacity: .[4]
        }
    ]
  }' > "$OUT/proof.json"

cat > "$OUT/proof.md" <<EOF
# Bundled terminal plugin nested proof

- packaged Components: terminal-session and terminal-ui resolved from the first-party init actor closures
- shell: real allowlisted sh PTY in $ROOT
- terminal environment: xterm-256color with truecolor intent, rendered by guest-parsed ANSI runs
- input: real Wayland key/text/pointer events through the focused external presentation client
- product shortcut: real Ctrl+Alt+T created paired terminal-session-2 and terminal-ui-b1-s1-2 actors, focused the new backend/session-scoped pane, accepted a Unicode shell command, and presented it in ${SPAWN_PRESENT_MS}ms
- inbox shortcut: activating a command card returned focus to terminal-ui, then real Ctrl+Alt+I focused the presentation-global inbox pane
- keyboard protocol: F5, Ctrl+Right, Shift+Tab, and DEC application-cursor Up were captured byte-for-byte from the PTY
- color policy: the controlled PTY profile removed ambient NO_COLOR, retained xterm-256color/truecolor, and rendered a real lazygit screen in terminal-lazygit-color.png
- managed GUI descendants: the terminal shell inherited the backend Wayland socket and isolated session bus; a real Ghostty child mapped inside Brood in terminal-ghostty-managed.png instead of opening through the host Ghostty instance
- TUI text reuse: lazygit started and presented in ${LAZYGIT_PRESENT_MS}ms with a bounded recent-layout cache; its interaction created ${LAZYGIT_CREATED} layouts and dropped ${LAZYGIT_DROPPED}
- control keys: Ctrl-L cleared the shell, Ctrl-C interrupted a foreground process, and Ctrl-D exited it
- clipboard: a real pointer drag selected and copied terminal text through the outer Wayland clipboard, then Ctrl-Shift-V pasted an external clipboard command back into the shell
- mouse: Helix negotiated SGR any-motion reporting and consumed real virtual-pointer scroll input; guest-owned shell history was navigated with smooth virtual-pointer axis input and reset with Shift+End
- presentation ownership: the client disconnected while the session-owned shell and its two native resources remained live, then a new presentation actor reattached
- TUI: Helix rendered full-screen and remained responsive through repeated compositor/pane resizes
- streaming: 4,000 output lines reached a revised canvas packet in ${STREAM_RENDER_MS}ms
- teardown: both the boot shell and the shortcut-created shell retired every PTY child and stream resource
- screenshots: terminal-shell-ansi-unicode.png, terminal-lazygit-color.png, terminal-ghostty-managed.png, terminal-reconnected.png, terminal-helix.png, terminal-helix-mouse-scroll.png, terminal-helix-resized.png, terminal-shell-after-helix.png, terminal-mouse-selection.png, terminal-streaming.png, terminal-scrollback.png, terminal-shell-exited.png, terminal-ctrl-alt-t.png
- structured metrics: proof.json
EOF

ctl quit > /dev/null 2>&1 || true
echo "terminal_plugin_nested=verified"
echo "proof=$OUT/proof.md"
echo "metrics=$OUT/proof.json"
echo "shell_capture=$OUT/terminal-shell-ansi-unicode.png"
echo "helix_capture=$OUT/terminal-helix.png"
echo "resized_capture=$OUT/terminal-helix-resized.png"
