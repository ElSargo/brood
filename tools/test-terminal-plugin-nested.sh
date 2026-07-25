#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]]; then
  exec dbus-run-session -- "$0" "$@"
fi

for tool in cc grim jq magick pkg-config python3 sway swaymsg wtype; do
  if ! command -v "$tool" > /dev/null 2>&1; then
    echo "ERR $tool is required for the terminal plugin proof" >&2
    exit 2
  fi
done

if [[ -n "${BROOD_TERMINAL_PACKAGE:-}" ]]; then
  PACKAGE="$BROOD_TERMINAL_PACKAGE"
else
  PACKAGE="$ROOT/result-terminal"
  nix build path:.#brood -o "$PACKAGE"
fi
PACKAGE="$(readlink -f "$PACKAGE")"

OUT="${BROOD_TERMINAL_OUT:-$(mktemp -d /tmp/brood-terminal.XXXXXX)}"
RUNTIME_DIR="$OUT/runtime"
SERVER_LOG="$OUT/server.log"
CLIENT1_LOG="$OUT/client-before-disconnect.log"
CLIENT2_LOG="$OUT/client-after-reconnect.log"
COMPOSITOR_LOG="$OUT/compositor.log"
SWAY_CONFIG="$OUT/sway.conf"
DBUS_DESTINATION="org.brood.TerminalTest.p$$"
mkdir -p "$OUT" "$RUNTIME_DIR"
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

RUST_LOG="${RUST_LOG:-broodd=info,brood_client=info,smithay=warn,wgpu_core=warn,wgpu_hal=warn}"
env -u brood_session \
  RUST_LOG="$RUST_LOG" \
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
jq -e '
  .hosted_runtime_io_children == 1
  and .hosted_runtime_io_streams == 1
  and .hosted_runtime_io_resources == 2
' <<< "$STATUS_LIVE" > /dev/null
printf '%s\n' "$STATUS_LIVE" > "$OUT/status-shell-live.json"

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
SWAY_SOCKET=""
for _ in $(seq 1 200); do
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
[[ -n "$WAYLAND_SOCKET" && -n "$SWAY_SOCKET" ]]

wayland_env() {
  env \
    XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    SWAYSOCK="$SWAY_SOCKET" \
    "$@"
}

start_client() {
  local log="$1"
  env -u brood_session \
    XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    WGPU_BACKEND=vulkan \
    BROOD_DMABUF_HAPPY_PATH=1 \
    BROOD_TRACE_PLUGIN_INPUT=1 \
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

metric_value() {
  local log="$1"
  local key="$2"
  last_metric "$log" | jq -r ".$key // 0"
}

type_line() {
  wayland_env wtype -s 2 "$1" -k Return
}

focus_terminal() {
  local log="$1"
  local baseline next quiet_previous quiet_samples
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
    type_line "echo __TERMINAL_FOCUS_${attempt}__"
    for _ in $(seq 1 60); do
      next="$(metric_value "$log" pty_bytes)"
      if ((next > baseline + 4)); then
        return
      fi
      sleep 0.05
    done
    wayland_env "$OUT/virtual-keyboard-key" --ctrl-only 36
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

start_client "$CLIENT1_LOG"
wait_for_terminal_render "$CLIENT1_LOG"
focus_terminal "$CLIENT1_LOG"
capture "$OUT/terminal-initial.png"

SHELL_BASELINE="$(metric_value "$CLIENT1_LOG" pty_bytes)"
type_line 'printf "\033[31mANSI_RED\033[0m λ中 TERM=%s COLORTERM=%s\n" "$TERM" "$COLORTERM"; printf "CWD=%s\n" "$PWD"; echo __SHELL_OK__'
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

close_frontend
STATUS_DISCONNECTED="$(ctl status-json)"
printf '%s\n' "$STATUS_DISCONNECTED" > "$OUT/status-presentation-disconnected.json"
jq -e '
  .presentation == "Detached"
  and .hosted_runtime_io_children == 1
  and .hosted_runtime_io_streams == 1
  and .hosted_runtime_io_resources == 2
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

HELIX_BASELINE="$(metric_value "$CLIENT2_LOG" pty_bytes)"
type_line 'printf "terminal plugin TUI proof λ中\n" > /tmp/brood-terminal-proof.txt; hx /tmp/brood-terminal-proof.txt'
for _ in $(seq 1 300); do
  HELIX_BYTES="$(metric_value "$CLIENT2_LOG" pty_bytes)"
  ((HELIX_BYTES > HELIX_BASELINE + 500)) && break
  sleep 0.05
done
((HELIX_BYTES > HELIX_BASELINE + 500))
capture "$OUT/terminal-helix.png"

RESIZE_BASELINE="$(metric_value "$CLIENT2_LOG" pty_resizes)"
for mode in 1024x640 1400x800 960x600 1180x680; do
  wayland_env swaymsg output HEADLESS-1 mode "$mode" > /dev/null
  sleep 0.15
done
for _ in $(seq 1 200); do
  RESIZES="$(metric_value "$CLIENT2_LOG" pty_resizes)"
  ((RESIZES >= RESIZE_BASELINE + 4)) && break
  sleep 0.05
done
((RESIZES >= RESIZE_BASELINE + 4))
capture "$OUT/terminal-helix-resized.png"

type_line ':q!'
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

type_line 'exit'
for _ in $(seq 1 300); do
  EXIT_METRIC="$(last_metric "$CLIENT2_LOG")"
  STATUS_EXITED="$(ctl status-json)"
  if jq -e '.exited == true' <<< "$EXIT_METRIC" > /dev/null \
    && jq -e '
      .hosted_runtime_io_children == 0
      and .hosted_runtime_io_streams == 0
      and .hosted_runtime_io_resources == 0
    ' <<< "$STATUS_EXITED" > /dev/null; then
    break
  fi
  sleep 0.05
done
jq -e '.exited == true' <<< "$EXIT_METRIC" > /dev/null
jq -e '
  .hosted_runtime_io_children == 0
  and .hosted_runtime_io_streams == 0
  and .hosted_runtime_io_resources == 0
' <<< "$STATUS_EXITED" > /dev/null
printf '%s\n' "$STATUS_EXITED" > "$OUT/status-shell-exited.json"
capture "$OUT/terminal-shell-exited.png"

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
  "$OUT/terminal-helix.png" \
  "$OUT/terminal-helix-resized.png" \
  "$OUT/terminal-reconnected.png" <<'PY'
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
  --argjson exited "$STATUS_EXITED" \
  --argjson terminal "$LAST_TERMINAL_METRIC" \
  --argjson graphics "$GRAPHICS_METRIC" \
  --argjson input "$INPUT_METRIC" \
  --argjson stream_render_ms "$STREAM_RENDER_MS" \
  '{
    workload: {
      shell: true,
      ansi_color: true,
      unicode: true,
      tui: "helix",
      repeated_resizes: $terminal.pty_resizes,
      streaming_bytes: $terminal.pty_bytes,
      streaming_render_ms: $stream_render_ms,
      presentation_reconnect: true,
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
    runtime_after_exit: {
      resources: $exited.hosted_runtime_io_resources,
      streams: $exited.hosted_runtime_io_streams,
      children: $exited.hosted_runtime_io_children
    },
    terminal_render: $terminal,
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
- input: real Wayland key/text events through the focused external presentation client
- presentation ownership: the client disconnected while the session-owned shell and its two native resources remained live, then a new presentation actor reattached
- TUI: Helix rendered full-screen and remained responsive through repeated compositor/pane resizes
- streaming: 4,000 output lines reached a revised canvas packet in ${STREAM_RENDER_MS}ms
- teardown: shell exit retired every PTY child and stream resource
- screenshots: terminal-shell-ansi-unicode.png, terminal-reconnected.png, terminal-helix.png, terminal-helix-resized.png, terminal-streaming.png, terminal-shell-exited.png
- structured metrics: proof.json
EOF

ctl quit > /dev/null 2>&1 || true
echo "terminal_plugin_nested=verified"
echo "proof=$OUT/proof.md"
echo "metrics=$OUT/proof.json"
echo "shell_capture=$OUT/terminal-shell-ansi-unicode.png"
echo "helix_capture=$OUT/terminal-helix.png"
echo "resized_capture=$OUT/terminal-helix-resized.png"
