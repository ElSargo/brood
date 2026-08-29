#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

for tool in cargo cc grim jq magick pkg-config python3 rg rustc sha256sum sway swaymsg; do
  if ! command -v "$tool" > /dev/null 2>&1; then
    echo "ERR $tool is required for the synchronized resize proof" >&2
    exit 2
  fi
done

OUT="${BROOD_RESIZE_OUT:-$(mktemp -d /tmp/brood-resize-proof.XXXXXX)}"
RUNTIME_DIR="$(mktemp -d /tmp/brood-resize-runtime.XXXXXX)"
FRAMES_DIR="$OUT/frames"
CLIENT_LOG_NORMAL="$OUT/client-normal.log"
CLIENT_LOG_DEGRADED="$OUT/client-degraded.log"
CLIENT_LOG="$CLIENT_LOG_NORMAL"
COMPOSITOR_LOG="$OUT/compositor.log"
POINTER_LOG="$OUT/pointer.log"
POINTER_PROTOCOL_LOG="$OUT/pointer-protocol.log"
POINTER_STDERR_LOG="$OUT/pointer-stderr.log"
BURST_INPUT="$OUT/burst-input.jsonl"
BURST_METRICS="$OUT/burst-metrics.json"
LIFECYCLE_METRICS="$OUT/interaction-lifecycle.json"
FRAME_MANIFEST="$OUT/frame-manifest.jsonl"
RESIZE_EVENTS="$OUT/resize-events.jsonl"
METRICS="$OUT/metrics.json"
PROOF="$OUT/proof.json"
ANALYSIS_LOG="$OUT/analysis-errors.log"
SWAY_CONFIG="$OUT/sway.conf"
OUTPUTS_JSON="$OUT/outputs.json"
mkdir -p "$FRAMES_DIR"
chmod 700 "$RUNTIME_DIR"
: > "$FRAME_MANIFEST"
: > "$POINTER_PROTOCOL_LOG"
: > "$POINTER_STDERR_LOG"
: > "$BURST_INPUT"
: > "$ANALYSIS_LOG"

CLIENT_PID=""
COMPOSITOR_PID=""
POINTER_PID=""
POINTER_READ_FD=""
POINTER_WRITE_FD=""
POINTER_PRESSED=0
WAYLAND_SOCKET=""
SWAY_SOCKET=""

cleanup() {
  local result=$?
  set +e
  if [[ -n "$POINTER_WRITE_FD" && -n "$POINTER_PID" ]] \
    && kill -0 "$POINTER_PID" 2> /dev/null; then
    if ((POINTER_PRESSED == 1)); then
      printf 'cancel\n' >&"$POINTER_WRITE_FD"
    else
      printf 'quit\n' >&"$POINTER_WRITE_FD"
    fi
  fi
  terminate_process_bounded "$POINTER_PID"
  terminate_process_bounded "$CLIENT_PID"
  terminate_process_bounded "$COMPOSITOR_PID"
  rm -rf -- "$RUNTIME_DIR"
  cat "$POINTER_PROTOCOL_LOG" "$POINTER_STDERR_LOG" > "$POINTER_LOG" 2> /dev/null || true
  if ((result != 0)); then
    {
      echo "resize_proof=failed"
      echo "exit_status=$result"
      echo "artifacts=$OUT"
      echo "frames=$(find "$FRAMES_DIR" -maxdepth 1 -name 'frame-*.png' -type f | wc -l)"
      echo "last_resize_events:"
      rg 'resize-present ' "$CLIENT_LOG_NORMAL" "$CLIENT_LOG_DEGRADED" \
        2> /dev/null | tail -n 30 || true
      echo "analysis_errors:"
      tail -n 30 "$ANALYSIS_LOG" 2> /dev/null || true
      echo "pointer_protocol:"
      tail -n 30 "$POINTER_LOG" 2> /dev/null || true
      echo "burst_input:"
      tail -n 30 "$BURST_INPUT" 2> /dev/null || true
      echo "burst_metrics:"
      tail -n 80 "$BURST_METRICS" 2> /dev/null || true
    } > "$OUT/failure-summary.txt"
    echo "resize_artifacts=$OUT" >&2
    echo "client_normal_log=$CLIENT_LOG_NORMAL" >&2
    tail -n 160 "$CLIENT_LOG_NORMAL" >&2 2> /dev/null || true
    echo "client_degraded_log=$CLIENT_LOG_DEGRADED" >&2
    tail -n 160 "$CLIENT_LOG_DEGRADED" >&2 2> /dev/null || true
    echo "compositor_log=$COMPOSITOR_LOG" >&2
    tail -n 100 "$COMPOSITOR_LOG" >&2 2> /dev/null || true
    echo "failure_summary=$OUT/failure-summary.txt" >&2
  fi
  return "$result"
}
trap 'echo "ERR command=$BASH_COMMAND line=$LINENO" >&2' ERR
trap cleanup EXIT

CLIENT_BIN="${BROOD_RESIZE_CLIENT:-$ROOT/target/debug/brood-client}"
if [[ -z "${BROOD_RESIZE_CLIENT:-}" ]]; then
  cargo build -p brood-client --bin brood-client --locked
fi

WORKSPACE_MODULE="${BROOD_RESIZE_WORKSPACE_MODULE:-$ROOT/plugins/default-workspace/target/wasm32-unknown-unknown/release/brood_default_workspace_plugin.wasm}"
if [[ -z "${BROOD_RESIZE_WORKSPACE_MODULE:-}" ]]; then
  cargo build \
    --manifest-path plugins/default-workspace/Cargo.toml \
    --target wasm32-unknown-unknown \
    --release \
    --locked
fi

RESIZE_PROOF_COMPONENT="${BROOD_RESIZE_PROOF_COMPONENT:-$ROOT/plugins/resize-proof/target/wasm32-unknown-unknown/release/brood_resize_proof_plugin.component.wasm}"
if [[ -z "${BROOD_RESIZE_PROOF_COMPONENT:-}" ]]; then
  RESIZE_PROOF_CORE="$ROOT/plugins/resize-proof/target/wasm32-unknown-unknown/release/brood_resize_proof_plugin.wasm"
  cargo build \
    --manifest-path plugins/resize-proof/Cargo.toml \
    --target wasm32-unknown-unknown \
    --release \
    --locked
  cargo run --quiet -p brood-wasm --example componentize -- \
    "$RESIZE_PROOF_CORE" \
    "$RESIZE_PROOF_COMPONENT"
fi

[[ -x "$CLIENT_BIN" ]] || {
  echo "ERR resize proof client executable is missing: $CLIENT_BIN" >&2
  exit 2
}
[[ -s "$RESIZE_PROOF_COMPONENT" ]] || {
  echo "ERR resize proof component is missing: $RESIZE_PROOF_COMPONENT" >&2
  exit 2
}
[[ -s "$WORKSPACE_MODULE" ]] || {
  echo "ERR default-workspace module is missing: $WORKSPACE_MODULE" >&2
  exit 2
}
CLIENT_BIN="$(readlink -f "$CLIENT_BIN")"
WORKSPACE_MODULE="$(readlink -f "$WORKSPACE_MODULE")"
RESIZE_PROOF_COMPONENT="$(readlink -f "$RESIZE_PROOF_COMPONENT")"
rustc \
  --edition=2024 \
  --crate-type=cdylib \
  --target wasm32-unknown-unknown \
  -C opt-level=s \
  -C panic=abort \
  tools/resize-session-switcher-shim.rs \
  -o "$OUT/resize-session-switcher-shim.wasm"

read -r -a RESIZE_WAYLAND_FLAGS <<< "$(pkg-config --cflags --libs wayland-client xkbcommon)"
cc \
  -std=c11 \
  -Wall \
  -Wextra \
  -Werror \
  tools/virtual-pointer-input.c \
  -o "$OUT/virtual-pointer-input" \
  "${RESIZE_WAYLAND_FLAGS[@]}"
RUST_LOG="${RUST_LOG:-brood_client=info,smithay=warn,wgpu_core=warn,wgpu_hal=warn}"

printf '%s\n' \
  'xwayland disable' \
  'output * mode --custom 1280x720@60Hz' \
  'output * scale 1.25' \
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

for _ in $(seq 1 240); do
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
[[ -n "$WAYLAND_SOCKET" && -n "$SWAY_SOCKET" ]] || {
  echo "ERR timed out waiting for headless Sway sockets" >&2
  exit 1
}

wayland_env() {
  env \
    XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    SWAYSOCK="$SWAY_SOCKET" \
    "$@"
}

wayland_env swaymsg -t get_outputs -r > "$OUTPUTS_JSON"
OUTPUT_NAME="$(jq -r 'map(select(.active))[0].name // empty' "$OUTPUTS_JSON")"
OUTPUT_SCALE="$(jq -r 'map(select(.active))[0].scale // 1' "$OUTPUTS_JSON")"
LOGICAL_WIDTH="$(jq -r 'map(select(.active))[0].rect.width // 0' "$OUTPUTS_JSON")"
LOGICAL_HEIGHT="$(jq -r 'map(select(.active))[0].rect.height // 0' "$OUTPUTS_JSON")"
MODE_WIDTH="$(jq -r 'map(select(.active))[0].current_mode.width // 0' "$OUTPUTS_JSON")"
MODE_HEIGHT="$(jq -r 'map(select(.active))[0].current_mode.height // 0' "$OUTPUTS_JSON")"
[[ -n "$OUTPUT_NAME" && "$LOGICAL_WIDTH" -gt 0 && "$LOGICAL_HEIGHT" -gt 0 ]] || {
  echo "ERR could not resolve the active headless Sway output" >&2
  exit 1
}
EXPECTED_PANE_LOGICAL_WIDTH=$((LOGICAL_WIDTH / 2))
EXPECTED_OUTPUT_SCALE="$(printf '%.2f' "$OUTPUT_SCALE")"
FRACTIONAL_SCALE_SUPPORTED="$(python3 - "$OUTPUT_SCALE" <<'PY'
import sys

print("true" if abs(float(sys.argv[1]) - 1.25) < 0.001 else "false")
PY
)"

TRACE_CURSOR=0
TRACE_LINE_NO=0
TRACE_TEXT=""
CAPTURE_LANE="normal"

start_proof_client() {
  local lane="$1"
  local second_actor="$2"
  local first_actor="${3:-resize-proof-fast}"
  local second_mode="${second_actor#resize-proof-}"
  [[ "$second_mode" != stall ]] || second_mode=stall-once
  local startup_native_line=""
  CLIENT_LOG="$CLIENT_LOG_NORMAL"
  if [[ "$lane" == degraded ]]; then
    CLIENT_LOG="$CLIENT_LOG_DEGRADED"
  fi
  : > "$CLIENT_LOG"
  TRACE_CURSOR=0
  TRACE_LINE_NO=0
  TRACE_TEXT=""
  CAPTURE_LANE="$lane"
  ACK_ROUNDTRIP_MS=0
  env -u brood_session -u BROOD_SURFACE_SESSION_ID -u BROOD_PANE_ID \
    XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    BROOD_KEYBINDINGS_PATH="$OUT/keybindings.json" \
    BROOD_TRACE_PLUGIN_GRAPHICS=1 \
    BROOD_TRACE_PLUGIN_INPUT=1 \
    BROOD_TRACE_RESIZE=1 \
    WGPU_BACKEND=vulkan \
    BROOD_DMABUF_HAPPY_PATH=1 \
    RUST_LOG="$RUST_LOG" \
    "$CLIENT_BIN" \
    --no-server \
    --no-init \
    --wasm-actor "$first_actor=$RESIZE_PROOF_COMPONENT" \
    --wasm-actor "$second_actor=$RESIZE_PROOF_COMPONENT" \
    --wasm-actor "session-switcher=$OUT/resize-session-switcher-shim.wasm" \
    --wasm-key-policy workspace "$WORKSPACE_MODULE" \
    > "$CLIENT_LOG" 2>&1 &
  CLIENT_PID=$!

  for _ in $(seq 1 600); do
    startup_native_line="$(python3 "$ROOT/tools/check-resize-startup.py" \
      "$CLIENT_LOG" \
      --width "$EXPECTED_PANE_LOGICAL_WIDTH" \
      --height "$LOGICAL_HEIGHT" \
      --first-actor "$first_actor" \
      --first-mode fast \
      --second-actor "$second_actor" \
      --second-mode "$second_mode")"
    if rg -q 'registered pointer-policy provider=workspace .*path=[^[:space:]]+$' "$CLIENT_LOG" \
      && rg -q 'bound pointer-policy provider=workspace .*actor=workspace .*actor_generation=[0-9]+$' "$CLIENT_LOG" \
      && rg -q "started presentation WASM actor id=$first_actor .*path=[^[:space:]]+$" "$CLIENT_LOG" \
      && rg -q "started presentation WASM actor id=$second_actor .*path=[^[:space:]]+$" "$CLIENT_LOG" \
      && rg -q 'started presentation WASM actor id=session-switcher .*path=[^[:space:]]+$' "$CLIENT_LOG" \
      && rg -q "settled presentation window resize .*to=${MODE_WIDTH}x${MODE_HEIGHT} .*elapsed_ms=[0-9]+$" "$CLIENT_LOG" \
      && [[ -n "$startup_native_line" ]] \
      && rg -q 'resize-present .*event_sequence=0 .*divider_x=[0-9]+ .*phase=(settled|promoted) .*superseded_epochs=[0-9]+$' "$CLIENT_LOG"; then
      return 0
    fi
    kill -0 "$CLIENT_PID" 2> /dev/null || {
      echo "ERR $lane presentation client exited before both proof panes rendered" >&2
      return 1
    }
    sleep 0.025
  done
  echo "ERR timed out waiting for $lane fast/$second_actor proof panes and divider" >&2
  tail -n 160 "$CLIENT_LOG" >&2 || true
  return 1
}

stop_proof_client() {
  local stopped_pid="$CLIENT_PID"
  terminate_process_bounded "$CLIENT_PID"
  CLIENT_PID=""
  for _ in $(seq 1 200); do
    if ! wayland_env swaymsg -t get_tree -r \
      | jq -e --argjson pid "$stopped_pid" \
        '.. | objects | select(.pid? == $pid)' > /dev/null; then
      return 0
    fi
    sleep 0.025
  done
  echo "ERR proof client window remained after process termination" >&2
  return 1
}

start_proof_client normal resize-proof-slow

wait_resize_trace() {
  local event_sequence="$1"
  local phases="$2"
  local require_divider="$3"
  local result=""
  for _ in $(seq 1 500); do
    result="$(awk \
      -v start="$TRACE_CURSOR" \
      -v wanted_sequence="$event_sequence" \
      -v wanted_phases="$phases" \
      -v require_divider="$require_divider" '
        NR <= start || index($0, "resize-present ") == 0 \
          || $0 !~ / superseded_epochs=[0-9]+$/ { next }
        {
          sequence = "";
          phase = "";
          divider = -1;
          for (field_index = 1; field_index <= NF; field_index += 1) {
            split($field_index, pair, "=");
            if (pair[1] == "event_sequence") sequence = pair[2];
            if (pair[1] == "phase") phase = pair[2];
            if (pair[1] == "divider_x") divider = pair[2] + 0;
          }
          if (sequence == wanted_sequence && phase ~ ("^(" wanted_phases ")$") && (!require_divider || divider >= 0)) {
            print NR "\t" substr($0, index($0, "resize-present "));
            exit;
          }
        }
      ' "$CLIENT_LOG")"
    if [[ -n "$result" ]]; then
      TRACE_LINE_NO="${result%%$'\t'*}"
      TRACE_TEXT="${result#*$'\t'}"
      TRACE_CURSOR="$TRACE_LINE_NO"
      return 0
    fi
    kill -0 "$CLIENT_PID" 2> /dev/null || {
      echo "ERR presentation client exited waiting for resize-present sequence=$event_sequence phases=$phases" >&2
      return 1
    }
    sleep 0.01
  done
  echo "ERR timed out waiting for resize-present event_sequence=$event_sequence phases=$phases after_line=$TRACE_CURSOR" >&2
  rg 'resize-present ' "$CLIENT_LOG" | tail -n 30 >&2 || true
  return 1
}

SYNCHRONIZED_LOG_LINE=0
wait_synchronized_layout_log() {
  local event_sequence="$1"
  local outcome="$2"
  local after_line="$3"
  local result=""
  for _ in $(seq 1 500); do
    result="$(awk \
      -v start="$after_line" \
      -v wanted_sequence="$event_sequence" \
      -v wanted_outcome="$outcome" '
        NR <= start { next }
        index($0, wanted_outcome " synchronized layout actor=workspace ") == 0 { next }
        wanted_outcome == "accepted" && $0 !~ / target_rects=\[[^]]*\]$/ { next }
        wanted_outcome != "accepted" && $0 !~ / duration_us=[0-9]+$/ { next }
        index($0, "event_sequence=" wanted_sequence " ") != 0 {
          print NR
          exit
        }
      ' "$CLIENT_LOG")"
    if [[ -n "$result" ]]; then
      SYNCHRONIZED_LOG_LINE="$result"
      return 0
    fi
    kill -0 "$CLIENT_PID" 2> /dev/null || {
      echo "ERR presentation client exited waiting for $outcome synchronized layout event_sequence=$event_sequence" >&2
      return 1
    }
    sleep 0.01
  done
  echo "ERR timed out waiting for $outcome synchronized layout event_sequence=$event_sequence after_line=$after_line" >&2
  rg 'synchronized layout actor=workspace ' "$CLIENT_LOG" | tail -n 30 >&2 || true
  return 1
}

FRAME_INDEX=0
capture_frame() {
  local label="$1"
  local ack_sequence="$2"
  local ack_action="$3"
  local ack_x="$4"
  local ack_y="$5"
  local frame_number frame_path frame_relative digest captured_ms
  printf -v frame_number '%04d' "$FRAME_INDEX"
  frame_relative="frames/frame-${frame_number}.png"
  frame_path="$OUT/$frame_relative"
  # Reduce PNG work after the compositor supplies the output buffer. The
  # proof actors provide the observable trace-to-capture interval.
  wayland_env grim -l 0 -o "$OUTPUT_NAME" "$frame_path"
  [[ -s "$frame_path" ]]
  digest="$(sha256sum "$frame_path" | awk '{print $1}')"
  captured_ms="$(date +%s%3N)"
  python3 - \
    "$FRAME_INDEX" \
    "$label" \
    "$frame_relative" \
    "$digest" \
    "$captured_ms" \
    "$ack_sequence" \
    "$ack_action" \
    "$ack_x" \
    "$ack_y" \
    "$ACK_ROUNDTRIP_MS" \
    "$TRACE_LINE_NO" \
    "$TRACE_TEXT" \
    "$OUTPUT_SCALE" \
    "$CAPTURE_LANE" \
    >> "$FRAME_MANIFEST" <<'PY'
import json
import re
import sys

pattern = re.compile(
    r"^resize-present interaction=(?P<interaction>\d+) "
    r"event_sequence=(?P<event_sequence>\d+) epoch=(?P<epoch>\d+) "
    r"divider_x=(?P<divider_x>-?\d+) "
    r"composition_size=(?P<composition_width>\d+)x(?P<composition_height>\d+) "
    r"phase=(?P<phase>settled|projected|promoted|degraded) "
    r"wait_ready=(?P<wait_ready>\d+)/(?P<wait_total>\d+) "
    r"staged_count=(?P<staged_count>\d+) "
    r"managed_expectations=(?P<managed_expectations>\d+) "
    r"managed_staged_resources=(?P<managed_staged_resources>\d+) "
    r"degraded=(?P<degraded>true|false) "
    r"superseded_epochs=(?P<superseded_epochs>\d+)$"
)
match = pattern.match(sys.argv[12])
if match is None:
    raise SystemExit(f"invalid resize-present trace: {sys.argv[12]!r}")
trace = match.groupdict()
record = {
    "frame": int(sys.argv[1]),
    "lane": sys.argv[14],
    "label": sys.argv[2],
    "file": sys.argv[3],
    "sha256": sys.argv[4],
    "captured_unix_ms": int(sys.argv[5]),
    "pointer_ack": {
        "sequence": int(sys.argv[6]),
        "action": sys.argv[7],
        "x": int(sys.argv[8]),
        "y": int(sys.argv[9]),
        "roundtrip_ms": int(sys.argv[10]),
    },
    "trace_log_line": int(sys.argv[11]),
    "output_scale": float(sys.argv[13]),
    "trace": {
        "interaction": int(trace["interaction"]),
        "event_sequence": int(trace["event_sequence"]),
        "epoch": int(trace["epoch"]),
        "divider_x": int(trace["divider_x"]),
        "phase": trace["phase"],
        "wait_ready": int(trace["wait_ready"]),
        "wait_total": int(trace["wait_total"]),
        "staged_count": int(trace["staged_count"]),
        "degraded": trace["degraded"] == "true",
        "superseded_epochs": int(trace["superseded_epochs"]),
    },
}
print(json.dumps(record, sort_keys=True))
PY
  FRAME_INDEX=$((FRAME_INDEX + 1))
}

wait_resize_trace 0 'settled|promoted' 1
capture_frame initial-settled 0 initial -1 -1
INITIAL_DIVIDER_X="$(sed -n 's/.* divider_x=\(-\{0,1\}[0-9][0-9]*\) .*/\1/p' <<< "$TRACE_TEXT")"
[[ -n "$INITIAL_DIVIDER_X" && "$INITIAL_DIVIDER_X" -ge 0 ]]

read -r INITIAL_POINTER_X POINTER_Y < <(
  python3 - "$INITIAL_DIVIDER_X" "$OUTPUT_SCALE" "$LOGICAL_WIDTH" "$LOGICAL_HEIGHT" <<'PY'
import sys

divider = int(sys.argv[1])
scale = max(float(sys.argv[2]), 0.001)
width = int(sys.argv[3])
height = int(sys.argv[4])
x = min(max(round(divider / scale), 1), width - 1)
y = min(max(height // 2, 1), height - 1)
print(x, y)
PY
)

ACK_SEQUENCE=0
ACK_ACTION=""
ACK_X=0
ACK_Y=0
ACK_ROUNDTRIP_MS=0
start_pointer_sequence() {
  coproc RESIZE_POINTER {
    wayland_env \
      "$OUT/virtual-pointer-input" \
      sequence "$LOGICAL_WIDTH" "$LOGICAL_HEIGHT" \
      2>> "$POINTER_STDERR_LOG"
  }
  POINTER_PID="$RESIZE_POINTER_PID"
  POINTER_READ_FD="${RESIZE_POINTER[0]}"
  POINTER_WRITE_FD="${RESIZE_POINTER[1]}"
  if ! IFS= read -r -t 5 -u "$POINTER_READ_FD" POINTER_READY; then
    echo "ERR persistent virtual pointer did not become ready" >&2
    return 1
  fi
  printf 'lane=%s %s\n' "$CAPTURE_LANE" "$POINTER_READY" >> "$POINTER_PROTOCOL_LOG"
  [[ "$POINTER_READY" == "ready extent_x=$LOGICAL_WIDTH extent_y=$LOGICAL_HEIGHT" ]]
  ACK_SEQUENCE=0
  ACK_ACTION=""
  ACK_X=0
  ACK_Y=0
  ACK_ROUNDTRIP_MS=0
  POINTER_PRESSED=0
}

send_pointer_command() {
  local command="$1"
  local expected_action="$2"
  local acknowledgement
  printf '%s\n' "$command" >&"$POINTER_WRITE_FD"
  if ! IFS= read -r -t 5 -u "$POINTER_READ_FD" acknowledgement; then
    echo "ERR virtual pointer did not acknowledge command=$command" >&2
    return 1
  fi
  printf 'lane=%s %s\n' "$CAPTURE_LANE" "$acknowledgement" >> "$POINTER_PROTOCOL_LOG"
  if [[ ! "$acknowledgement" =~ ^ack[[:space:]]sequence=([0-9]+)[[:space:]]action=([^[:space:]]+)[[:space:]]x=([0-9]+)[[:space:]]y=([0-9]+)[[:space:]]roundtrip_ms=([0-9]+)$ ]]; then
    echo "ERR malformed virtual pointer acknowledgement: $acknowledgement" >&2
    return 1
  fi
  ACK_SEQUENCE="${BASH_REMATCH[1]}"
  ACK_ACTION="${BASH_REMATCH[2]}"
  ACK_X="${BASH_REMATCH[3]}"
  ACK_Y="${BASH_REMATCH[4]}"
  ACK_ROUNDTRIP_MS="${BASH_REMATCH[5]}"
  [[ "$ACK_ACTION" == "$expected_action" ]] || {
    echo "ERR virtual pointer acknowledged action=$ACK_ACTION expected=$expected_action" >&2
    return 1
  }
  if [[ "$expected_action" == press ]]; then
    POINTER_PRESSED=1
  elif [[ "$expected_action" == release ]]; then
    POINTER_PRESSED=0
  fi
}

BURST_INPUT_INDEX=0
record_burst_ack() {
  local phase="$1"
  printf \
    '{"index":%d,"phase":"%s","sequence":%d,"action":"%s","x":%d,"y":%d,"roundtrip_ms":%d}\n' \
    "$BURST_INPUT_INDEX" \
    "$phase" \
    "$ACK_SEQUENCE" \
    "$ACK_ACTION" \
    "$ACK_X" \
    "$ACK_Y" \
    "$ACK_ROUNDTRIP_MS" \
    >> "$BURST_INPUT"
  BURST_INPUT_INDEX=$((BURST_INPUT_INDEX + 1))
}

finish_pointer_sequence() {
  printf 'quit\n' >&"$POINTER_WRITE_FD"
  if ! IFS= read -r -t 5 -u "$POINTER_READ_FD" POINTER_DONE; then
    echo "ERR persistent virtual pointer did not acknowledge quit" >&2
    return 1
  fi
  printf 'lane=%s %s\n' "$CAPTURE_LANE" "$POINTER_DONE" >> "$POINTER_PROTOCOL_LOG"
  [[ "$POINTER_DONE" == "done sequence=$ACK_SEQUENCE reason=quit" ]]
  wait "$POINTER_PID"
  POINTER_PID=""
  POINTER_READ_FD=""
  POINTER_WRITE_FD=""
}

start_pointer_sequence

send_pointer_command "motion $INITIAL_POINTER_X $POINTER_Y" motion
wait_resize_trace "$ACK_SEQUENCE" settled 1
capture_frame hover-settled "$ACK_SEQUENCE" "$ACK_ACTION" "$ACK_X" "$ACK_Y"

send_pointer_command press press
wait_resize_trace "$ACK_SEQUENCE" settled 1
capture_frame press-settled "$ACK_SEQUENCE" "$ACK_ACTION" "$ACK_X" "$ACK_Y"

mapfile -t DRAG_POINTER_XS < <(
  python3 - "$INITIAL_POINTER_X" "$LOGICAL_WIDTH" <<'PY'
import sys

start = int(sys.argv[1])
width = int(sys.argv[2])
margin = max(64, width // 10)
right_room = width - margin - start
left_room = start - margin
if right_room >= 100:
    end = start + min(160, right_room)
elif left_room >= 100:
    end = start - min(160, left_room)
else:
    raise SystemExit(f"divider has no safe drag range: start={start} width={width}")
values = []
for step in range(1, 17):
    value = round(start + (end - start) * step / 16)
    if not values or value != values[-1]:
        values.append(value)
if len(values) != 16:
    raise SystemExit(f"drag coordinates are not unique: {values}")
print("\n".join(str(value) for value in values))
PY
)
[[ "${#DRAG_POINTER_XS[@]}" -eq 16 ]]

MOTION_INDEX=0
for pointer_x in "${DRAG_POINTER_XS[@]}"; do
  MOTION_INDEX=$((MOTION_INDEX + 1))
  printf -v MOTION_LABEL '%02d' "$MOTION_INDEX"
  send_pointer_command "motion $pointer_x $POINTER_Y" motion
  wait_resize_trace "$ACK_SEQUENCE" projected 1
  capture_frame \
    "motion-${MOTION_LABEL}-projected" \
    "$ACK_SEQUENCE" "$ACK_ACTION" "$ACK_X" "$ACK_Y"
  wait_resize_trace "$ACK_SEQUENCE" 'promoted|degraded' 1
  capture_frame \
    "motion-${MOTION_LABEL}-final" \
    "$ACK_SEQUENCE" "$ACK_ACTION" "$ACK_X" "$ACK_Y"
done

send_pointer_command release release
wait_resize_trace "$ACK_SEQUENCE" projected 1
capture_frame release-projected "$ACK_SEQUENCE" "$ACK_ACTION" "$ACK_X" "$ACK_Y"
wait_resize_trace "$ACK_SEQUENCE" 'promoted|degraded' 1
capture_frame release-final "$ACK_SEQUENCE" "$ACK_ACTION" "$ACK_X" "$ACK_Y"
FRAME_LOCKED_RELEASE_SEQUENCE="$ACK_SEQUENCE"
FRAME_LOCKED_PROMOTED_LINE="$TRACE_LINE_NO"
wait_synchronized_layout_log \
  "$FRAME_LOCKED_RELEASE_SEQUENCE" \
  completed \
  "$FRAME_LOCKED_PROMOTED_LINE"

BURST_START_POINTER_X="${DRAG_POINTER_XS[$((${#DRAG_POINTER_XS[@]} - 1))]}"
BURST_START_DIVIDER_X="$(sed -n 's/.* divider_x=\(-\{0,1\}[0-9][0-9]*\) .*/\1/p' <<< "$TRACE_TEXT")"
[[ -n "$BURST_START_DIVIDER_X" && "$BURST_START_DIVIDER_X" -ge 0 ]]
mapfile -t BURST_POINTER_XS < <(
  python3 - "$BURST_START_POINTER_X" "$INITIAL_POINTER_X" <<'PY'
import sys

start = int(sys.argv[1])
anchor = int(sys.argv[2])
end = start + round((anchor - start) / 2)
values = [round(start + (end - start) * step / 8) for step in range(1, 9)]
if len(set(values)) != 8 or end == start:
    raise SystemExit(f"burst coordinates are not unique: start={start} end={end} values={values}")
print("\n".join(str(value) for value in values))
PY
)
[[ "${#BURST_POINTER_XS[@]}" -eq 8 ]]

# Establish a fresh pointer capture, then admit exactly the first motion.  Once
# that transaction is active, the remaining captured motions and final release
# are sent back-to-back with only the virtual-pointer roundtrip ACK between
# them. The 90 ms slow proof pane keeps the first request in flight. One
# obsolete cycle plus the coalesced final cycle costs at most 180 ms, leaving
# 70 ms of the fixed 250 ms transaction budget for host hint and preparation
# work while keeping the supersession path deterministic.
send_pointer_command press press
record_burst_ack capture-press
wait_resize_trace "$ACK_SEQUENCE" settled 1
BURST_TRACE_START_LINE="$TRACE_LINE_NO"

BURST_MOTION_INDEX=1
send_pointer_command "motion ${BURST_POINTER_XS[0]} $POINTER_Y" motion
record_burst_ack motion-01
BURST_FIRST_MOTION_SEQUENCE="$ACK_SEQUENCE"
wait_synchronized_layout_log "$BURST_FIRST_MOTION_SEQUENCE" accepted "$BURST_TRACE_START_LINE"
BURST_FIRST_ACCEPTED_LINE="$SYNCHRONIZED_LOG_LINE"

for pointer_x in "${BURST_POINTER_XS[@]:1}"; do
  BURST_MOTION_INDEX=$((BURST_MOTION_INDEX + 1))
  printf -v BURST_MOTION_LABEL '%02d' "$BURST_MOTION_INDEX"
  send_pointer_command "motion $pointer_x $POINTER_Y" motion
  record_burst_ack "motion-$BURST_MOTION_LABEL"
done
send_pointer_command release release
record_burst_ack release
BURST_RELEASE_SEQUENCE="$ACK_SEQUENCE"

wait_resize_trace "$BURST_RELEASE_SEQUENCE" projected 1
BURST_PROJECTED_TRACE="$TRACE_TEXT"
BURST_PROJECTED_LINE="$TRACE_LINE_NO"
wait_resize_trace "$BURST_RELEASE_SEQUENCE" promoted 1
BURST_PROMOTED_TRACE="$TRACE_TEXT"
BURST_PROMOTED_LINE="$TRACE_LINE_NO"
wait_synchronized_layout_log "$BURST_RELEASE_SEQUENCE" completed "$BURST_TRACE_START_LINE"
BURST_COMPLETED_LINE="$SYNCHRONIZED_LOG_LINE"
kill -0 "$CLIENT_PID"

finish_pointer_sequence

stop_proof_client
if ! python3 - \
  "$CLIENT_LOG_NORMAL" \
  "$BURST_INPUT" \
  "$BURST_TRACE_START_LINE" \
  "$BURST_FIRST_ACCEPTED_LINE" \
  "$BURST_COMPLETED_LINE" \
  "$BURST_START_POINTER_X" \
  "$BURST_START_DIVIDER_X" \
  "$OUTPUT_SCALE" \
  "$BURST_PROJECTED_LINE" \
  "$BURST_PROJECTED_TRACE" \
  "$BURST_PROMOTED_LINE" \
  "$BURST_PROMOTED_TRACE" \
  > "$BURST_METRICS" 2>> "$ANALYSIS_LOG" <<'PY'
import json
import pathlib
import re
import statistics
import sys

log_path = pathlib.Path(sys.argv[1])
input_path = pathlib.Path(sys.argv[2])
start_line = int(sys.argv[3])
first_accepted_line = int(sys.argv[4])
completed_line = int(sys.argv[5])
start_pointer_x = int(sys.argv[6])
start_divider_x = int(sys.argv[7])
scale = float(sys.argv[8])
projected_line = int(sys.argv[9])
projected_text = sys.argv[10]
promoted_line = int(sys.argv[11])
promoted_text = sys.argv[12]

trace_pattern = re.compile(
    r"^resize-present interaction=(?P<interaction>\d+) "
    r"event_sequence=(?P<event_sequence>\d+) epoch=(?P<epoch>\d+) "
    r"divider_x=(?P<divider_x>-?\d+) "
    r"composition_size=(?P<composition_width>\d+)x(?P<composition_height>\d+) "
    r"phase=(?P<phase>settled|projected|promoted|degraded) "
    r"wait_ready=(?P<wait_ready>\d+)/(?P<wait_total>\d+) "
    r"staged_count=(?P<staged_count>\d+) "
    r"managed_expectations=(?P<managed_expectations>\d+) "
    r"managed_staged_resources=(?P<managed_staged_resources>\d+) "
    r"degraded=(?P<degraded>true|false) "
    r"superseded_epochs=(?P<superseded_epochs>\d+)$"
)
accepted_pattern = re.compile(
    r"accepted synchronized layout actor=workspace request=(?P<request>\d+) "
    r"interaction=(?P<interaction>\d+) event_sequence=(?P<event_sequence>\d+) "
    r"native_resize_revision=(?P<native_resize_revision>\d+) epoch=(?P<epoch>\d+) "
)
terminal_pattern = re.compile(
    r"(?P<outcome>completed|degraded) synchronized layout actor=workspace "
    r"request=(?P<request>\d+) .* event_sequence=(?P<event_sequence>\d+) "
)

def parse_trace(text):
    match = trace_pattern.fullmatch(text)
    if match is None:
        raise SystemExit(f"invalid burst resize-present trace: {text!r}")
    fields = match.groupdict()
    return {
        "interaction": int(fields["interaction"]),
        "event_sequence": int(fields["event_sequence"]),
        "epoch": int(fields["epoch"]),
        "divider_x": int(fields["divider_x"]),
        "phase": fields["phase"],
        "wait_ready": int(fields["wait_ready"]),
        "wait_total": int(fields["wait_total"]),
        "staged_count": int(fields["staged_count"]),
        "degraded": fields["degraded"] == "true",
        "superseded_epochs": int(fields["superseded_epochs"]),
    }

inputs = [json.loads(line) for line in input_path.read_text().splitlines() if line]
expected_actions = ["press"] + ["motion"] * 8 + ["release"]
actions = [event["action"] for event in inputs]
sequences = [event["sequence"] for event in inputs]
motions = [event for event in inputs if event["action"] == "motion"]
if actions != expected_actions:
    raise SystemExit(f"unexpected burst input actions: {actions}")
if not all(second == first + 1 for first, second in zip(sequences, sequences[1:])):
    raise SystemExit(f"burst pointer ACKs are not ordered and consecutive: {sequences}")
if len({event["x"] for event in motions}) != 8:
    raise SystemExit(f"burst captured motion coordinates are not unique: {motions}")
roundtrips = [event["roundtrip_ms"] for event in inputs]
if any(value < 0 or value >= 5_000 for value in roundtrips):
    raise SystemExit(f"invalid burst pointer roundtrip durations: {roundtrips}")

projected = parse_trace(projected_text)
promoted = parse_trace(promoted_text)
release = inputs[-1]
if not (
    projected["event_sequence"] == release["sequence"]
    and promoted["event_sequence"] == release["sequence"]
    and projected["phase"] == "projected"
    and promoted["phase"] == "promoted"
    and not projected["degraded"]
    and not promoted["degraded"]
    and promoted["staged_count"] == 0
    and projected["divider_x"] == promoted["divider_x"]
    and projected["interaction"] != 0
    and projected["interaction"] == promoted["interaction"]
):
    raise SystemExit(
        f"burst release did not become the exact durable target: "
        f"release={release} projected={projected} promoted={promoted}"
    )

expected_release_divider = start_divider_x + (release["x"] - start_pointer_x) * scale
release_coordinate_error = abs(promoted["divider_x"] - expected_release_divider)
if release_coordinate_error > 1.01:
    raise SystemExit(
        f"burst durable divider misses released pointer coordinate: "
        f"start_pointer={start_pointer_x} start_divider={start_divider_x} "
        f"release_x={release['x']} scale={scale} expected={expected_release_divider} "
        f"actual={promoted['divider_x']}"
    )

lines = log_path.read_text(encoding="utf-8", errors="replace").splitlines()
accepted = []
terminals = {}
traces = []
for line_number, line in enumerate(lines, 1):
    if line_number <= start_line or line_number > completed_line:
        continue
    accepted_match = accepted_pattern.search(line)
    if accepted_match is not None:
        fields = accepted_match.groupdict()
        accepted.append({
            "line": line_number,
            "request": int(fields["request"]),
            "interaction": int(fields["interaction"]),
            "event_sequence": int(fields["event_sequence"]),
            "epoch": int(fields["epoch"]),
        })
    terminal_match = terminal_pattern.search(line)
    if terminal_match is not None:
        fields = terminal_match.groupdict()
        terminals[int(fields["request"])] = {
            "line": line_number,
            "outcome": fields["outcome"],
            "event_sequence": int(fields["event_sequence"]),
        }
    trace_offset = line.find("resize-present ")
    if trace_offset >= 0:
        trace = parse_trace(line[trace_offset:])
        trace["line"] = line_number
        traces.append(trace)

if not accepted or accepted[0]["line"] != first_accepted_line:
    raise SystemExit(
        f"burst first admitted request does not match synchronization marker: "
        f"expected_line={first_accepted_line} accepted={accepted}"
    )
superseded = []
for current, following in zip(accepted, accepted[1:]):
    terminal = terminals.get(current["request"])
    if terminal is None or following["line"] < terminal["line"]:
        superseded.append({
            "request": current["request"],
            "event_sequence": current["event_sequence"],
            "epoch": current["epoch"],
            "superseding_request": following["request"],
            "superseding_event_sequence": following["event_sequence"],
            "superseding_epoch": following["epoch"],
        })
if not superseded:
    raise SystemExit(
        f"burst did not exercise an overlapping Superseded path: "
        f"accepted={accepted} terminals={terminals}"
    )
max_staged_count = max((trace["staged_count"] for trace in traces), default=0)
if max_staged_count > 1:
    raise SystemExit(f"burst staged more than one transaction: traces={traces}")
if any(trace["degraded"] or trace["phase"] == "degraded" for trace in traces):
    raise SystemExit(f"burst unexpectedly degraded: traces={traces}")
if (
    accepted[-1]["event_sequence"] != release["sequence"]
    or accepted[-1]["interaction"] != promoted["interaction"]
    or completed_line <= promoted_line
    or terminals.get(accepted[-1]["request"], {}).get("outcome") != "completed"
):
    raise SystemExit(
        f"burst final request did not complete after promotion: "
        f"promoted_line={promoted_line} completed_line={completed_line} terminals={terminals}"
    )

result = {
    "input": {
        "command_count": len(inputs),
        "captured_motion_count": len(motions),
        "unique_motion_coordinates": len({event["x"] for event in motions}),
        "ack_sequences": sequences,
        "ordered_acknowledgements": True,
        "roundtrip_ms": {
            "samples": len(roundtrips),
            "average": statistics.fmean(roundtrips),
            "maximum": max(roundtrips),
        },
    },
    "accepted_request_count": len(accepted),
    "accepted_requests": accepted,
    "superseded_path_count": len(superseded),
    "superseded_requests": superseded,
    "supersession_evidence": "new accepted request preceded the active request terminal outcome",
    "max_staged_count": max_staged_count,
    "staging_bounded": True,
    "client_survived": True,
    "release": {
        "event_sequence": release["sequence"],
        "pointer_x": release["x"],
        "pointer_y": release["y"],
        "expected_divider_x": expected_release_divider,
        "projected_divider_x": projected["divider_x"],
        "durable_divider_x": promoted["divider_x"],
        "coordinate_error_device_px": release_coordinate_error,
        "projected_line": projected_line,
        "promoted_line": promoted_line,
        "completed_line": completed_line,
        "phase": promoted["phase"],
        "staged_count": promoted["staged_count"],
        "matches_projected_target": True,
        "matches_pointer_coordinate": True,
    },
}
print(json.dumps(result, indent=2, sort_keys=True))
PY
then
  cat "$ANALYSIS_LOG" >&2
  exit 1
fi

start_proof_client \
  degraded \
  resize-proof-stall-after-first \
  resize-proof-gated-fast
wait_resize_trace 0 'settled|promoted' 1
capture_frame timeout-initial-settled 0 initial -1 -1
TIMEOUT_DIVIDER_X="$(sed -n 's/.* divider_x=\(-\{0,1\}[0-9][0-9]*\) .*/\1/p' <<< "$TRACE_TEXT")"
[[ -n "$TIMEOUT_DIVIDER_X" && "$TIMEOUT_DIVIDER_X" -ge 0 ]]
read -r TIMEOUT_POINTER_X TIMEOUT_POINTER_Y TIMEOUT_FIRST_X TIMEOUT_RECOVERY_X < <(
  python3 - \
    "$TIMEOUT_DIVIDER_X" \
    "$OUTPUT_SCALE" \
    "$LOGICAL_WIDTH" \
    "$LOGICAL_HEIGHT" <<'PY'
import sys

divider = int(sys.argv[1])
scale = max(float(sys.argv[2]), 0.001)
width = int(sys.argv[3])
height = int(sys.argv[4])
start = min(max(round(divider / scale), 1), width - 1)
margin = max(64, width // 10)
direction = 1 if width - margin - start >= 96 else -1
first = start + direction * 48
recovery = start + direction * 96
if not margin <= first < width - margin or not margin <= recovery < width - margin:
    raise SystemExit(
        f"timeout recovery coordinates are unsafe: start={start} first={first} "
        f"recovery={recovery} width={width}"
    )
print(start, min(max(height // 2, 1), height - 1), first, recovery)
PY
)

start_pointer_sequence
send_pointer_command "motion $TIMEOUT_POINTER_X $TIMEOUT_POINTER_Y" motion
wait_resize_trace "$ACK_SEQUENCE" settled 1
capture_frame timeout-hover-settled "$ACK_SEQUENCE" "$ACK_ACTION" "$ACK_X" "$ACK_Y"
send_pointer_command press press
wait_resize_trace "$ACK_SEQUENCE" settled 1
capture_frame timeout-press-settled "$ACK_SEQUENCE" "$ACK_ACTION" "$ACK_X" "$ACK_Y"

send_pointer_command "motion $TIMEOUT_FIRST_X $TIMEOUT_POINTER_Y" motion
wait_resize_trace "$ACK_SEQUENCE" projected 1
capture_frame timeout-projected "$ACK_SEQUENCE" "$ACK_ACTION" "$ACK_X" "$ACK_Y"
wait_resize_trace "$ACK_SEQUENCE" degraded 1
capture_frame timeout-degraded "$ACK_SEQUENCE" "$ACK_ACTION" "$ACK_X" "$ACK_Y"

send_pointer_command "motion $TIMEOUT_RECOVERY_X $TIMEOUT_POINTER_Y" motion
wait_resize_trace "$ACK_SEQUENCE" promoted 1
capture_frame recovery-promoted "$ACK_SEQUENCE" "$ACK_ACTION" "$ACK_X" "$ACK_Y"

send_pointer_command release release
wait_resize_trace "$ACK_SEQUENCE" promoted 1
capture_frame recovery-release-promoted "$ACK_SEQUENCE" "$ACK_ACTION" "$ACK_X" "$ACK_Y"
finish_pointer_sequence
stop_proof_client
cat "$POINTER_PROTOCOL_LOG" "$POINTER_STDERR_LOG" > "$POINTER_LOG"

# Continue in the same headless compositor with the production root-layout
# actor tree.  Its artifacts are intentionally separate so the original 44
# frame-locked drag/timeout captures remain stable and directly comparable.
EXTENDED_OUT="$OUT/window-shell"
EXTENDED_PROOF="$EXTENDED_OUT/proof.json"
EXTENDED_METRICS="$EXTENDED_OUT/metrics.json"
env \
  BROOD_RESIZE_EXTENDED_OUT="$EXTENDED_OUT" \
  BROOD_RESIZE_CLIENT="$CLIENT_BIN" \
  BROOD_RESIZE_WORKSPACE_MODULE="$WORKSPACE_MODULE" \
  BROOD_RESIZE_PROOF_COMPONENT="$RESIZE_PROOF_COMPONENT" \
  XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  SWAYSOCK="$SWAY_SOCKET" \
  RUST_LOG="$RUST_LOG" \
  "$ROOT/tools/test-resize-overhaul-window-shell-nested.sh"
[[ -s "$EXTENDED_PROOF" && -s "$EXTENDED_METRICS" ]]

# Prove recursive region recomputation with the production root actor and an
# explicit three-level lease chain. Keep this trace and its strict pixel checks
# separate from the established drag and window-shell evidence.
REGION_TREE_OUT="$OUT/region-tree"
REGION_TREE_PROOF="$REGION_TREE_OUT/proof.json"
REGION_TREE_METRICS="$REGION_TREE_OUT/metrics.json"
env \
  BROOD_RESIZE_REGION_OUT="$REGION_TREE_OUT" \
  BROOD_RESIZE_CLIENT="$CLIENT_BIN" \
  BROOD_RESIZE_WORKSPACE_MODULE="$WORKSPACE_MODULE" \
  BROOD_RESIZE_PROOF_COMPONENT="$RESIZE_PROOF_COMPONENT" \
  XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  SWAYSOCK="$SWAY_SOCKET" \
  RUST_LOG="$RUST_LOG" \
  "$ROOT/tools/test-resize-region-tree-nested.sh"
[[ -s "$REGION_TREE_PROOF" && -s "$REGION_TREE_METRICS" ]]

# Exercise exact epoch/configure/frame correlation with two real XDG SHM
# clients. This lane shares only the outer compositor with the other proofs.
MANAGED_OUT="$OUT/managed"
MANAGED_PROOF="$MANAGED_OUT/proof.json"
MANAGED_METRICS="$MANAGED_OUT/metrics.json"
env \
  BROOD_RESIZE_MANAGED_OUT="$MANAGED_OUT" \
  BROOD_RESIZE_CLIENT="$CLIENT_BIN" \
  BROOD_RESIZE_WORKSPACE_MODULE="$WORKSPACE_MODULE" \
  BROOD_RESIZE_PROOF_COMPONENT="$RESIZE_PROOF_COMPONENT" \
  XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  SWAYSOCK="$SWAY_SOCKET" \
  RUST_LOG="$RUST_LOG" \
  "$ROOT/tools/test-resize-managed-nested.sh"
[[ -s "$MANAGED_PROOF" && -s "$MANAGED_METRICS" ]]

if ! python3 - \
  "$CLIENT_LOG_NORMAL" \
  "$BURST_INPUT" \
  "$BURST_TRACE_START_LINE" \
  > "$LIFECYCLE_METRICS" 2>> "$ANALYSIS_LOG" <<'PY'
import json
import pathlib
import re
import sys

log_path = pathlib.Path(sys.argv[1])
input_path = pathlib.Path(sys.argv[2])
burst_start_line = int(sys.argv[3])
lines = log_path.read_text(encoding="utf-8", errors="replace").splitlines()
inputs = [json.loads(line) for line in input_path.read_text().splitlines() if line]
release = next((event for event in reversed(inputs) if event["action"] == "release"), None)
if release is None:
    raise SystemExit("burst input has no final release")
release_sequence = int(release["sequence"])

event_pattern = re.compile(
    r"pointer-interaction event interaction=(?P<interaction>\d+) "
    r"event_sequence=(?P<sequence>\d+) .* phase=ButtonRelease terminal=true$"
)
capture_pattern = re.compile(
    r"pointer-policy capture released interaction=(?P<interaction>\d+) "
    r"event_sequence=(?P<sequence>\d+) phase=ButtonRelease$"
)
claim_pattern = re.compile(
    r"pointer-interaction claimed interaction=(?P<interaction>\d+) "
    r"event_sequence=(?P<sequence>\d+) .* request=(?P<request>\d+)$"
)
accepted_pattern = re.compile(
    r"accepted synchronized layout actor=workspace request=(?P<request>\d+) "
    r"interaction=(?P<interaction>\d+) event_sequence=(?P<sequence>\d+) "
)
mailbox_pattern = re.compile(
    r"accepted synchronized layout terminal mailbox actor=workspace "
    r"actor_generation=\d+ request=(?P<request>\d+) "
    r"interaction=(?P<interaction>\d+) event_sequence=(?P<sequence>\d+)$"
)

def find_after(start, pattern, predicate):
    for index in range(start, len(lines)):
        match = pattern.search(lines[index])
        if match is not None and predicate(match):
            return index + 1, match
    raise SystemExit(f"missing lifecycle evidence after line {start}: {pattern.pattern}")

event_line, event = find_after(
    burst_start_line,
    event_pattern,
    lambda match: int(match["sequence"]) == release_sequence,
)
interaction = int(event["interaction"])
capture_line, _ = find_after(
    event_line,
    capture_pattern,
    lambda match: (
        int(match["interaction"]) == interaction
        and int(match["sequence"]) == release_sequence
    ),
)
claim_line, claim = find_after(
    capture_line,
    claim_pattern,
    lambda match: (
        int(match["interaction"]) == interaction
        and int(match["sequence"]) == release_sequence
    ),
)
request = int(claim["request"])
accepted_line, _ = find_after(
    claim_line,
    accepted_pattern,
    lambda match: (
        int(match["request"]) == request
        and int(match["interaction"]) == interaction
        and int(match["sequence"]) == release_sequence
    ),
)
mailbox_line, _ = find_after(
    accepted_line,
    mailbox_pattern,
    lambda match: (
        int(match["request"]) == request
        and int(match["interaction"]) == interaction
        and int(match["sequence"]) == release_sequence
    ),
)
retired_pattern = re.compile(
    rf"pointer-interaction retired interaction={interaction} "
    r"reason=terminal-request-notification-delivered$"
)
retired_line, _ = find_after(mailbox_line, retired_pattern, lambda _match: True)

paint_pattern = re.compile(
    r"resize-paint-order ordinary_panes=(?P<ordinary>\d+) "
    r"dividers=(?P<dividers>\d+) explicit_overlay_panes=(?P<overlay>\d+) "
    r"elevated_overlay_panes=(?P<elevated>\d+) "
    r"low_z_divider_over_ordinary=true$"
)
paint_line, paint = find_after(0, paint_pattern, lambda match: (
    int(match["ordinary"]) > 0 and int(match["dividers"]) > 0
))

ordering = [event_line, capture_line, claim_line, accepted_line, mailbox_line, retired_line]
if ordering != sorted(ordering) or len(set(ordering)) != len(ordering):
    raise SystemExit(f"interaction lifecycle order is invalid: {ordering}")

print(json.dumps({
    "interaction": interaction,
    "event_sequence": release_sequence,
    "request_id": request,
    "lines": {
        "released_recorded": event_line,
        "capture_cleared": capture_line,
        "exact_pair_claimed": claim_line,
        "request_accepted": accepted_line,
        "terminal_mailbox_accepted": mailbox_line,
        "exact_claim_retired": retired_line,
        "low_z_paint": paint_line,
    },
    "paint": {
        "ordinary_panes": int(paint["ordinary"]),
        "dividers": int(paint["dividers"]),
        "low_z_divider_over_ordinary": True,
    },
    "assertions": {
        "release_registry_precedes_capture_clear": event_line < capture_line,
        "final_release_accepted_after_capture_clear": capture_line < accepted_line,
        "exact_claim_retires_after_terminal_mailbox": mailbox_line < retired_line,
        "low_z_divider_fixture_presented": True,
    },
}, indent=2, sort_keys=True))
PY
then
  cat "$ANALYSIS_LOG" >&2
  exit 1
fi

python3 - \
  "$CLIENT_LOG_NORMAL" \
  "$CLIENT_LOG_DEGRADED" \
  "$BURST_TRACE_START_LINE" \
  > "$RESIZE_EVENTS" <<'PY'
import json
import re
import sys

pattern = re.compile(
    r"resize-present interaction=(?P<interaction>\d+) "
    r"event_sequence=(?P<event_sequence>\d+) epoch=(?P<epoch>\d+) "
    r"divider_x=(?P<divider_x>-?\d+) "
    r"composition_size=(?P<composition_width>\d+)x(?P<composition_height>\d+) "
    r"phase=(?P<phase>settled|projected|promoted|degraded) "
    r"wait_ready=(?P<wait_ready>\d+)/(?P<wait_total>\d+) "
    r"staged_count=(?P<staged_count>\d+) "
    r"managed_expectations=(?P<managed_expectations>\d+) "
    r"managed_staged_resources=(?P<managed_staged_resources>\d+) "
    r"degraded=(?P<degraded>true|false)"
)
burst_start_line = int(sys.argv[3])
for lane, path in zip(("normal", "degraded"), sys.argv[1:3]):
    with open(path, encoding="utf-8", errors="replace") as stream:
        for line_number, line in enumerate(stream, 1):
            match = pattern.search(line)
            if match is None:
                continue
            fields = match.groupdict()
            print(json.dumps({
                "lane": "burst" if lane == "normal" and line_number > burst_start_line else lane,
                "log_line": line_number,
                "interaction": int(fields["interaction"]),
                "event_sequence": int(fields["event_sequence"]),
                "epoch": int(fields["epoch"]),
                "divider_x": int(fields["divider_x"]),
                "phase": fields["phase"],
                "wait_ready": int(fields["wait_ready"]),
                "wait_total": int(fields["wait_total"]),
                "staged_count": int(fields["staged_count"]),
                "degraded": fields["degraded"] == "true",
            }, sort_keys=True))
PY
[[ -s "$RESIZE_EVENTS" ]]

if ! python3 - "$OUT" "$FRAME_MANIFEST" "$OUTPUT_SCALE" \
  > "$METRICS" 2>> "$ANALYSIS_LOG" <<'PY'
import collections
import json
import math
import pathlib
import statistics
import subprocess
import sys

root = pathlib.Path(sys.argv[1])
manifest_path = pathlib.Path(sys.argv[2])
scale = float(sys.argv[3])
records = [json.loads(line) for line in manifest_path.read_text().splitlines() if line]
if len(records) < 37:
    raise SystemExit(f"expected at least 37 synchronized captures, got {len(records)}")

def srgb(linear):
    if linear <= 0.0031308:
        return round(linear * 12.92 * 255)
    return round((1.055 * linear ** (1 / 2.4) - 0.055) * 255)

divider_state_colours = {
    "normal": (0.48, 0.54, 0.64),
    "hover": (0.35, 0.66, 0.95),
    "active": (0.20, 0.78, 1.00),
}
palette = []
state_palette = {}
for state, colour in divider_state_colours.items():
    state_palette[state] = [
        tuple(round(channel * 255) for channel in colour),
        tuple(srgb(channel) for channel in colour),
    ]
    palette.append(tuple(round(channel * 255) for channel in colour))
    palette.append(tuple(srgb(channel) for channel in colour))

def is_divider_colour(red, green, blue):
    return min(
        (red - target[0]) ** 2 + (green - target[1]) ** 2 + (blue - target[2]) ** 2
        for target in palette
    ) <= 30 ** 2

def load_rgb(path):
    identify = subprocess.check_output(
        ["magick", str(path), "-format", "%w %h %k", "info:"], text=True
    ).split()
    width, height, colours = map(int, identify)
    raw = subprocess.check_output(
        ["magick", str(path), "-alpha", "off", "-depth", "8", "rgb:-"]
    )
    if len(raw) != width * height * 3:
        raise SystemExit(
            f"unexpected raw image length for {path}: {len(raw)} != {width * height * 3}"
        )
    return width, height, colours, raw

def divider_metrics(width, height, raw, expected_x):
    row_start = max(0, round(height * 0.03))
    row_end = min(height, round(height * 0.97))
    sampled_rows = max(row_end - row_start, 1)
    scores = []
    for x in range(width):
        matches = 0
        for y in range(row_start, row_end):
            offset = (y * width + x) * 3
            if is_divider_colour(raw[offset], raw[offset + 1], raw[offset + 2]):
                matches += 1
        scores.append(matches / sampled_rows)

    strong_threshold = max(0.55, max(scores) * 0.88)
    clusters = []
    start = None
    for x, score in enumerate(scores + [0.0]):
        if score >= strong_threshold and start is None:
            start = x
        elif score < strong_threshold and start is not None:
            clusters.append((start, x - 1))
            start = None
    thin = [cluster for cluster in clusters if cluster[1] - cluster[0] + 1 <= 2]
    traced = [
        cluster
        for cluster in thin
        if abs((cluster[0] + cluster[1]) / 2 - expected_x) <= 3
    ]
    if len(traced) != 1:
        raise SystemExit(
            f"expected one thin divider cluster at trace_x={expected_x}, got {traced}; "
            f"all thin clusters={thin}; all strong clusters={clusters}"
        )
    left, right = traced[0]
    if right - left + 1 != 1:
        raise SystemExit(f"divider is not one device pixel wide: {(left, right)}")
    detected = left
    expected_error = abs(detected - expected_x)
    if expected_error > 3:
        raise SystemExit(
            f"divider does not match trace: detected={detected} trace={expected_x}"
        )
    column_colours = collections.Counter()
    for y in range(row_start, row_end):
        offset = (y * width + detected) * 3
        column_colours[(raw[offset], raw[offset + 1], raw[offset + 2])] += 1
    mode_colour, mode_count = column_colours.most_common(1)[0]
    state = min(
        state_palette,
        key=lambda candidate: min(
            sum((mode_colour[index] - target[index]) ** 2 for index in range(3))
            for target in state_palette[candidate]
        ),
    )
    return {
        "detected_x": detected,
        "trace_x": expected_x,
        "trace_error_px": expected_error,
        "device_width": 1,
        "coverage": scores[detected],
        "mode_rgb": list(mode_colour),
        "mode_coverage": mode_count / sampled_rows,
        "state": state,
    }

captures = []
loaded = {}
for record in records:
    if record["lane"] != "normal":
        continue
    path = root / record["file"]
    data = path.read_bytes()
    if not data.startswith(b"\x89PNG\r\n\x1a\n") or len(data) < 4096:
        raise SystemExit(f"invalid PNG capture: {path}")
    width, height, colours, raw = load_rgb(path)
    # The proof fixture deliberately uses a small set of exact solid colours;
    # anti-aliased edges add only a handful more.  Barcode/extent decoding below
    # is the strong content check, while this catches blank or flat captures.
    if colours < 12:
        raise SystemExit(f"resize capture has too little visual detail: {path} colours={colours}")
    trace = record["trace"]
    if trace["divider_x"] < 0:
        raise SystemExit(f"capture has no presented divider: {record['label']}")
    if trace["degraded"] or trace["phase"] == "degraded":
        raise SystemExit(f"resize degraded in capture: {record['label']} trace={trace}")
    divider = divider_metrics(width, height, raw, trace["divider_x"])
    captures.append({
        "frame": record["frame"],
        "label": record["label"],
        "file": record["file"],
        "width": width,
        "height": height,
        "colours": colours,
        "phase": trace["phase"],
        "event_sequence": trace["event_sequence"],
        "pointer_ack": record["pointer_ack"],
        "epoch": trace["epoch"],
        "wait_ready": trace["wait_ready"],
        "wait_total": trace["wait_total"],
        "staged_count": trace["staged_count"],
        "divider": divider,
    })
    loaded[record["label"]] = (width, height, raw)

initial = next(capture for capture in captures if capture["label"] == "initial-settled")
motion_final = [
    capture for capture in captures
    if capture["label"].startswith("motion-") and capture["label"].endswith("-final")
]
if len(motion_final) != 16:
    raise SystemExit(f"expected sixteen final motion frames, got {len(motion_final)}")
positions = [capture["divider"]["detected_x"] for capture in motion_final]
trace_positions = [capture["divider"]["trace_x"] for capture in motion_final]
direction = 1 if positions[-1] > positions[0] else -1
if not all((second - first) * direction > 0 for first, second in zip(positions, positions[1:])):
    raise SystemExit(f"captured divider motion is not strictly monotonic: {positions}")
if not all(
    (second - first) * direction > 0
    for first, second in zip(trace_positions, trace_positions[1:])
):
    raise SystemExit(f"traced divider motion is not strictly monotonic: {trace_positions}")
if abs(positions[-1] - initial["divider"]["detected_x"]) < 20:
    raise SystemExit(
        f"divider did not move far enough: initial={initial['divider']} final={positions[-1]}"
    )

release_final = next(capture for capture in captures if capture["label"] == "release-final")
if release_final["phase"] != "promoted":
    raise SystemExit(f"release did not finish with atomic promotion: {release_final}")
if abs(release_final["divider"]["detected_x"] - positions[-1]) > 1:
    raise SystemExit(
        f"release changed durable divider geometry: motion={positions[-1]} "
        f"release={release_final['divider']['detected_x']}"
    )

by_label = {capture["label"]: capture for capture in captures}
if by_label["initial-settled"]["divider"]["state"] != "normal":
    raise SystemExit(f"initial divider is not normal: {by_label['initial-settled']}")
if by_label["hover-settled"]["divider"]["state"] != "hover":
    raise SystemExit(f"hover did not recolor divider: {by_label['hover-settled']}")
if by_label["press-settled"]["divider"]["state"] != "active":
    raise SystemExit(f"capture press did not activate divider: {by_label['press-settled']}")
active_drag = [
    capture for capture in captures
    if capture["label"].startswith("motion-")
]
if len(active_drag) != 32 or any(
    capture["divider"]["state"] != "active" for capture in active_drag
):
    raise SystemExit(
        "captured drag did not retain active divider chrome: "
        + repr([(capture["label"], capture["divider"]["state"]) for capture in active_drag])
    )
first_motion_delta_device = abs(
    by_label["motion-01-projected"]["pointer_ack"]["x"]
    - by_label["hover-settled"]["pointer_ack"]["x"]
) * scale
if first_motion_delta_device <= 8.0:
    raise SystemExit(
        f"first captured motion did not leave the original 8px half-zone: "
        f"delta_device={first_motion_delta_device}"
    )
if release_final["divider"]["state"] not in {"normal", "hover"}:
    raise SystemExit(f"release did not clear active divider chrome: {release_final}")

width, height, final_raw = loaded["release-final"]
luminance = [
    (54 * final_raw[index] + 183 * final_raw[index + 1] + 19 * final_raw[index + 2]) / 256
    for index in range(0, len(final_raw), 3)
]
laplacian = []
edge_count = 0
edge_samples = 0
for y in range(2, height - 2, 2):
    for x in range(2, width - 2, 2):
        center = luminance[y * width + x]
        laplacian.append(
            4 * center
            - luminance[y * width + x - 1]
            - luminance[y * width + x + 1]
            - luminance[(y - 1) * width + x]
            - luminance[(y + 1) * width + x]
        )
        edge_count += abs(center - luminance[y * width + x + 1]) >= 20
        edge_count += abs(center - luminance[(y + 1) * width + x]) >= 20
        edge_samples += 2
sharpness = statistics.pvariance(laplacian) if len(laplacian) > 1 else 0.0
edge_density = edge_count / max(edge_samples, 1)
if not math.isfinite(sharpness) or sharpness < 8.0 or edge_density < 0.002:
    raise SystemExit(
        f"final frame is not demonstrably sharp: laplacian_variance={sharpness} "
        f"edge_density={edge_density}"
    )

roundtrip_by_event = {}
for record in records:
    acknowledgement = record["pointer_ack"]
    sequence = acknowledgement["sequence"]
    if sequence <= 0:
        continue
    key = (record["lane"], sequence)
    duration = acknowledgement["roundtrip_ms"]
    previous = roundtrip_by_event.setdefault(key, duration)
    if previous != duration:
        raise SystemExit(
            f"pointer roundtrip changed between captures for event={key}: "
            f"{previous} != {duration}"
        )
roundtrip_values = list(roundtrip_by_event.values())
if len(roundtrip_values) != 24 or any(value < 0 or value >= 5_000 for value in roundtrip_values):
    raise SystemExit(f"invalid synchronized pointer roundtrip metrics: {roundtrip_values}")

metrics = {
    "capture_count": len(captures),
    "output_scale": scale,
    "input_roundtrip_ms": {
        "samples": len(roundtrip_values),
        "average": statistics.fmean(roundtrip_values),
        "maximum": max(roundtrip_values),
    },
    "one_device_pixel_divider_every_frame": all(
        capture["divider"]["device_width"] == 1 for capture in captures
    ),
    "no_degraded_frames": True,
    "chrome": {
        "initial": by_label["initial-settled"]["divider"]["state"],
        "hover": by_label["hover-settled"]["divider"]["state"],
        "captured": by_label["press-settled"]["divider"]["state"],
        "active_drag_frames": len(active_drag),
        "first_motion_delta_device_px": first_motion_delta_device,
        "active_persists_outside_hit_zone": all(
            capture["divider"]["state"] == "active"
            for capture in active_drag
        ) and first_motion_delta_device > 8.0,
        "release": release_final["divider"]["state"],
        "states_visually_distinct": len({
            tuple(by_label["initial-settled"]["divider"]["mode_rgb"]),
            tuple(by_label["hover-settled"]["divider"]["mode_rgb"]),
            tuple(by_label["press-settled"]["divider"]["mode_rgb"]),
        }) == 3,
    },
    "motion": {
        "direction": "right" if direction > 0 else "left",
        "captured_divider_x": positions,
        "traced_divider_x": trace_positions,
        "strictly_monotonic": True,
    },
    "final": {
        "label": release_final["label"],
        "phase": release_final["phase"],
        "divider_x": release_final["divider"]["detected_x"],
        "divider_width_device_px": release_final["divider"]["device_width"],
        "laplacian_variance": sharpness,
        "edge_density": edge_density,
        "sharp": True,
    },
    "captures": captures,
}
print(json.dumps(metrics, indent=2, sort_keys=True))
PY
then
  cat "$ANALYSIS_LOG" >&2
  exit 1
fi

FIDUCIAL_METRICS="$OUT/fiducials.json"
if ! python3 - "$OUT" "$FRAME_MANIFEST" "$OUTPUT_SCALE" \
  > "$FIDUCIAL_METRICS" 2>> "$ANALYSIS_LOG" <<'PY'
import json
import math
import pathlib
import statistics
import struct
import subprocess
import sys

root = pathlib.Path(sys.argv[1])
manifest_path = pathlib.Path(sys.argv[2])
scale = float(sys.argv[3])
records = [json.loads(line) for line in manifest_path.read_text().splitlines() if line]

ROLE_LINEAR = {
    "fast": (0.04, 0.82, 0.92),
    "slow": (0.92, 0.12, 0.72),
    "stall": (0.96, 0.58, 0.08),
}
INITIAL_BACKGROUND = (0.04, 0.08, 0.18)


def srgb_byte(linear):
    if linear <= 0.0031308:
        return round(linear * 12.92 * 255)
    return round((1.055 * linear ** (1 / 2.4) - 0.055) * 255)


def variants(linear):
    return [
        tuple(round(channel * 255) for channel in linear),
        tuple(srgb_byte(channel) for channel in linear),
    ]


ROLE_PALETTES = {role: variants(colour) for role, colour in ROLE_LINEAR.items()}


def squared_distance(left, right):
    return sum((left[index] - right[index]) ** 2 for index in range(3))


def matches_palette(pixel, palette, tolerance=42):
    return min(squared_distance(pixel, target) for target in palette) <= tolerance ** 2


def near_white(pixel):
    return min(pixel) >= 205 and max(pixel) - min(pixel) <= 45


def load_rgb(path):
    identify = subprocess.check_output(
        ["magick", str(path), "-format", "%w %h", "info:"], text=True
    ).split()
    width, height = map(int, identify)
    raw = subprocess.check_output(
        ["magick", str(path), "-alpha", "off", "-depth", "8", "rgb:-"]
    )
    if len(raw) != width * height * 3:
        raise SystemExit(f"invalid raw RGB length for {path}")
    return width, height, raw


def pixel(raw, width, x, y):
    offset = (y * width + x) * 3
    return raw[offset], raw[offset + 1], raw[offset + 2]


def mean_patch(raw, width, height, x, y, radius_x=1, radius_y=1):
    samples = []
    for sample_y in range(max(0, y - radius_y), min(height, y + radius_y + 1)):
        for sample_x in range(max(0, x - radius_x), min(width, x + radius_x + 1)):
            samples.append(pixel(raw, width, sample_x, sample_y))
    return tuple(round(statistics.mean(channel[index] for channel in samples)) for index in range(3))


def role_score(raw, width, rect, role):
    left, top, pane_width, pane_height = rect
    palette = ROLE_PALETTES[role]
    matches = 0
    for y in range(top, top + pane_height, 4):
        for x in range(left, left + pane_width, 4):
            matches += matches_palette(pixel(raw, width, x, y), palette)
    return matches


def contiguous_role_run(raw, width, rect, role):
    left, _top, pane_width, pane_height = rect
    y = min(pane_height - 1, max(1, round(pane_height * 0.27)))
    palette = ROLE_PALETTES[role]
    matching = [
        x
        for x in range(0, max(1, pane_width // 4))
        if matches_palette(pixel(raw, width, left + x, y), palette)
    ]
    if not matching:
        raise SystemExit(f"could not find {role} marker rail in rect={rect}")
    start = matching[0]
    end = start
    misses = 0
    for candidate in range(start, max(1, pane_width // 4)):
        if matches_palette(pixel(raw, width, left + candidate, y), palette):
            end = candidate
            misses = 0
        else:
            misses += 1
            if misses >= 2:
                break
    return start, end - start + 1


def marker_vertical_run(raw, width, height, rect, role, edge, rail_width):
    left, _top, _pane_width, pane_height = rect
    x = min(width - 1, left + edge + max(1, rail_width // 2))
    palette = ROLE_PALETTES[role]
    runs = []
    start = None
    for y in range(pane_height):
        matched = matches_palette(pixel(raw, width, x, y), palette)
        if matched and start is None:
            start = y
        elif not matched and start is not None:
            runs.append((start, y - 1))
            start = None
    if start is not None:
        runs.append((start, pane_height - 1))
    if not runs:
        raise SystemExit(f"could not find {role} marker vertical run in rect={rect}")
    return max(runs, key=lambda run: run[1] - run[0])


def decode_horizontal_bits(raw, width, height, rect, edge, y, count):
    left, _top, pane_width, _pane_height = rect
    inner_width = pane_width - edge * 2
    if inner_width <= count:
        raise SystemExit(f"pane is too narrow for {count}-cell barcode: {rect}")
    value = 0
    cells = []
    for index in range(count):
        center = left + edge + round((index + 0.5) * inner_width / count)
        radius = max(1, round(inner_width / count * 0.16))
        sample = mean_patch(raw, width, height, center, y, radius, 1)
        luminance = (54 * sample[0] + 183 * sample[1] + 19 * sample[2]) / 256
        bit = luminance >= 145
        value |= int(bit) << index
        cells.append({"bit": int(bit), "rgb": list(sample)})
    return value, cells


def decode_vertical_bits(raw, width, height, rect, edge, rail_width, marker_run):
    left, _top, pane_width, _pane_height = rect
    start_y, end_y = marker_run
    inner_height = end_y - start_y + 1
    x = left + pane_width - edge - max(1, rail_width // 2)
    value = 0
    for index in range(32):
        center_y = start_y + round((index + 0.5) * inner_height / 32)
        radius = max(1, round(inner_height / 32 * 0.14))
        sample = mean_patch(raw, width, height, x, center_y, 1, radius)
        luminance = (54 * sample[0] + 183 * sample[1] + 19 * sample[2]) / 256
        value |= int(luminance >= 145) << index
    return value


def float_from_bits(bits):
    return struct.unpack("<f", int(bits).to_bytes(4, "little"))[0]


def background_for_epoch(epoch):
    return tuple(0.22 + 0.20 * ((epoch >> shift) & 3) for shift in (0, 2, 4))


def background_sample(raw, width, height, rect):
    left, _top, pane_width, pane_height = rect
    samples = []
    for u, v in ((0.28, 0.31), (0.34, 0.68), (0.68, 0.31), (0.74, 0.68)):
        samples.append(
            mean_patch(
                raw,
                width,
                height,
                left + round(pane_width * u),
                round(pane_height * v),
                2,
                2,
            )
        )
    return tuple(round(statistics.median(sample[index] for sample in samples)) for index in range(3))


def background_match(sample, epoch):
    candidates = [("epoch", target) for target in variants(background_for_epoch(epoch))]
    if epoch == 0:
        candidates.extend(("initial", target) for target in variants(INITIAL_BACKGROUND))
    label, target = min(candidates, key=lambda candidate: squared_distance(sample, candidate[1]))
    error = math.sqrt(squared_distance(sample, target))
    if error > 48:
        raise SystemExit(
            f"background does not match decoded epoch={epoch}: sample={sample} "
            f"nearest={label}:{target} error={error:.2f}"
        )
    return label, error


def border_coverage(raw, width, rect):
    left, _top, pane_width, pane_height = rect
    horizontal = []
    for y in range(0, min(5, pane_height)):
        horizontal.append(
            sum(near_white(pixel(raw, width, x, y)) for x in range(left + 2, left + pane_width - 2))
            / max(pane_width - 4, 1)
        )
    vertical = []
    for x in range(left, min(left + 6, left + pane_width)):
        vertical.append(
            sum(near_white(pixel(raw, width, x, y)) for y in range(2, pane_height - 2))
            / max(pane_height - 4, 1)
        )
    for x in range(max(left, left + pane_width - 6), left + pane_width):
        vertical.append(
            sum(near_white(pixel(raw, width, x, y)) for y in range(2, pane_height - 2))
            / max(pane_height - 4, 1)
        )
    coverage = min(max(horizontal, default=0.0), max(vertical, default=0.0))
    if coverage < 0.68:
        raise SystemExit(f"near-white extent border is incomplete: rect={rect} coverage={coverage}")
    return coverage


def analyse_pane(raw, width, height, rect):
    scores = {role: role_score(raw, width, rect, role) for role in ROLE_LINEAR}
    role = max(scores, key=scores.get)
    if scores[role] < 35:
        raise SystemExit(f"proof role marker not found: rect={rect} scores={scores}")
    edge, rail_width = contiguous_role_run(raw, width, rect, role)
    marker_run = marker_vertical_run(raw, width, height, rect, role, edge, rail_width)
    marker_top, marker_bottom = marker_run
    top_y = max(1, round((edge + marker_top) * 0.5))
    bottom_y = min(height - 2, height - top_y - 1)
    epoch, _epoch_cells = decode_horizontal_bits(
        raw, width, height, rect, edge, top_y, 64
    )
    size_bits, _size_cells = decode_horizontal_bits(
        raw, width, height, rect, edge, bottom_y, 64
    )
    encoded_width = float_from_bits(size_bits & 0xFFFF_FFFF)
    encoded_height = float_from_bits(size_bits >> 32)
    scale_bits = decode_vertical_bits(
        raw, width, height, rect, edge, rail_width, marker_run
    )
    encoded_scale = float_from_bits(scale_bits)
    for name, value in (
        ("width", encoded_width),
        ("height", encoded_height),
        ("scale", encoded_scale),
    ):
        if not math.isfinite(value) or value <= 0:
            raise SystemExit(
                f"decoded {name} is invalid for role={role}: value={value} "
                f"edge={edge} rail={rail_width} run={marker_run}"
            )
    sampled_background = background_sample(raw, width, height, rect)
    background_kind, background_error = background_match(sampled_background, epoch)
    return {
        "role": role,
        "role_scores": scores,
        "epoch": epoch,
        "encoded_width": encoded_width,
        "encoded_height": encoded_height,
        "encoded_scale": encoded_scale,
        "background_rgb": list(sampled_background),
        "background_kind": background_kind,
        "background_error": background_error,
        "edge_px": edge,
        "rail_width_px": rail_width,
        "extent_border_coverage": border_coverage(raw, width, rect),
        "rect": list(rect),
    }


decoded = []
for record in records:
    path = root / record["file"]
    width, height, raw = load_rgb(path)
    divider = record["trace"]["divider_x"]
    if not 32 <= divider <= width - 32:
        raise SystemExit(f"invalid divider for proof decode: {record['label']} x={divider}")
    rects = [(0, 0, divider, height), (divider, 0, width - divider, height)]
    try:
        panes = [analyse_pane(raw, width, height, rect) for rect in rects]
    except SystemExit as error:
        raise SystemExit(f"frame {record['label']}: {error}") from error
    roles = [pane["role"] for pane in panes]
    expected_roles = {"fast", "slow"} if record["lane"] == "normal" else {"fast", "stall"}
    if set(roles) != expected_roles or len(set(roles)) != 2:
        raise SystemExit(
            f"frame {record['label']} has roles={roles}, expected={sorted(expected_roles)}"
        )
    decoded.append({
        "frame": record["frame"],
        "lane": record["lane"],
        "label": record["label"],
        "trace": record["trace"],
        "panes": {pane["role"]: pane for pane in panes},
    })


def close(left, right, tolerance=1.6):
    return abs(left - right) <= tolerance


normal = [frame for frame in decoded if frame["lane"] == "normal"]
if len(normal) != 37:
    raise SystemExit(f"expected 37 normal synchronized captures, got {len(normal)}")
visible = None
projected_pairs = 0
promoted_pairs = 0
for frame in normal:
    trace = frame["trace"]
    panes = frame["panes"]
    if trace["degraded"] or trace["phase"] == "degraded":
        raise SystemExit(f"normal drag degraded: {frame['label']} trace={trace}")
    if trace["staged_count"] > 1:
        raise SystemExit(f"staged transaction count is unbounded: {frame['label']} trace={trace}")
    if trace["phase"] == "settled":
        current = {
            role: (pane["epoch"], pane["encoded_width"], pane["encoded_height"])
            for role, pane in panes.items()
        }
        if visible is None:
            visible = current
        else:
            for role in panes:
                expected = visible[role]
                actual = current[role]
                if actual[0] != expected[0] or not close(actual[1], expected[1]):
                    raise SystemExit(
                        f"settled input changed visible content: {frame['label']} "
                        f"role={role} expected={expected} actual={actual}"
                    )
        continue
    if trace["phase"] == "projected":
        if visible is None:
            raise SystemExit("projected frame appeared before settled proof content")
        for role, pane in panes.items():
            expected = visible[role]
            if pane["epoch"] != expected[0] or not close(pane["encoded_width"], expected[1]):
                raise SystemExit(
                    f"projected waited pair is mixed/new: {frame['label']} role={role} "
                    f"expected_old={expected} decoded={pane}"
                )
        if frame["label"].startswith("motion-"):
            current_widths = {
                "fast": panes["fast"]["rect"][2] / scale,
                "slow": panes["slow"]["rect"][2] / scale,
            }
            if not any(
                abs(panes[role]["encoded_width"] - current_widths[role]) > 2.0
                for role in panes
            ):
                raise SystemExit(
                    f"motion projection did not retain old-size content: {frame['label']}"
                )
        if trace["staged_count"] != 1 or trace["wait_total"] != 2:
            raise SystemExit(f"projected barrier metrics are invalid: {frame['label']} {trace}")
        projected_pairs += 1
        continue
    if trace["phase"] == "promoted":
        epochs = {pane["epoch"] for pane in panes.values()}
        if epochs != {trace["epoch"]}:
            raise SystemExit(
                f"promoted waited pair is mixed/stale: {frame['label']} "
                f"epochs={epochs} trace_epoch={trace['epoch']}"
            )
        for pane in panes.values():
            expected_width = pane["rect"][2] / scale
            expected_height = pane["rect"][3] / scale
            if not close(pane["encoded_width"], expected_width) or not close(
                pane["encoded_height"], expected_height
            ):
                raise SystemExit(
                    f"promoted size barcode does not match geometry: {frame['label']} "
                    f"pane={pane} expected={expected_width}x{expected_height}"
                )
            if not close(pane["encoded_scale"], scale, 0.02):
                raise SystemExit(
                    f"scale barcode mismatch: {frame['label']} pane={pane} scale={scale}"
                )
        if trace["wait_ready"] != 2 or trace["wait_total"] != 2 or trace["staged_count"] != 0:
            raise SystemExit(f"promotion barrier metrics are invalid: {frame['label']} {trace}")
        visible = {
            role: (pane["epoch"], pane["encoded_width"], pane["encoded_height"])
            for role, pane in panes.items()
        }
        promoted_pairs += 1
        continue
    raise SystemExit(f"unexpected normal phase in capture: {frame['label']} {trace}")

if projected_pairs != 17 or promoted_pairs != 17:
    raise SystemExit(
        f"expected 17 old/old projections and 17 new/new promotions, got "
        f"{projected_pairs}/{promoted_pairs}"
    )

degraded = {frame["label"]: frame for frame in decoded if frame["lane"] == "degraded"}
required_degraded = {
    "timeout-initial-settled",
    "timeout-hover-settled",
    "timeout-press-settled",
    "timeout-projected",
    "timeout-degraded",
    "recovery-promoted",
    "recovery-release-promoted",
}
if set(degraded) != required_degraded:
    raise SystemExit(
        f"degraded lane labels mismatch: got={sorted(degraded)} "
        f"expected={sorted(required_degraded)}"
    )
timeout_projected = degraded["timeout-projected"]
timeout_initial = degraded["timeout-initial-settled"]
old_timeout_epochs = {
    pane["epoch"] for pane in timeout_initial["panes"].values()
}
if len(old_timeout_epochs) != 1:
    raise SystemExit(
        f"timeout lane did not start with a coherent waited pair: {timeout_initial}"
    )
if {
    pane["epoch"] for pane in timeout_projected["panes"].values()
} != old_timeout_epochs:
    raise SystemExit(f"timeout projection did not retain old/old content: {timeout_projected}")
timeout = degraded["timeout-degraded"]
timeout_trace = timeout["trace"]
if not (
    timeout_trace["phase"] == "degraded"
    and timeout_trace["degraded"]
    and timeout_trace["wait_ready"] == 1
    and timeout_trace["wait_total"] == 2
    and timeout_trace["staged_count"] == 0
):
    raise SystemExit(f"timeout did not clear its bounded staged barrier: {timeout_trace}")
if {pane["epoch"] for pane in timeout["panes"].values()} != old_timeout_epochs:
    raise SystemExit(
        f"degraded frame exposed a partial waited set instead of old/old content: {timeout}"
    )

recovery = degraded["recovery-promoted"]
recovery_trace = recovery["trace"]
if not (
    recovery_trace["phase"] == "promoted"
    and not recovery_trace["degraded"]
    and recovery_trace["event_sequence"] > timeout_trace["event_sequence"]
    and recovery_trace["wait_ready"] == 2
    and recovery_trace["wait_total"] == 2
    and recovery_trace["staged_count"] == 0
    and {pane["epoch"] for pane in recovery["panes"].values()}
    == {recovery_trace["epoch"]}
):
    raise SystemExit(f"later resize did not recover atomic promotion: {recovery}")

release_recovery = degraded["recovery-release-promoted"]
if (
    release_recovery["trace"]["phase"] != "promoted"
    or release_recovery["trace"]["degraded"]
    or {pane["epoch"] for pane in release_recovery["panes"].values()}
    != {release_recovery["trace"]["epoch"]}
):
    raise SystemExit(f"recovered release was not durable and coherent: {release_recovery}")

result = {
    "decoded_capture_count": len(decoded),
    "normal": {
        "capture_count": len(normal),
        "projected_old_old_pairs": projected_pairs,
        "promoted_new_new_pairs": promoted_pairs,
        "mixed_waited_pairs": 0,
        "bounded_staged_count": True,
        "roles": ["fast", "slow"],
    },
    "timeout": {
        "degraded_epoch": timeout_trace["epoch"],
        "event_sequence": timeout_trace["event_sequence"],
        "wait_ready": timeout_trace["wait_ready"],
        "wait_total": timeout_trace["wait_total"],
        "staged_count_after_degrade": timeout_trace["staged_count"],
        "visible_fast_epoch": timeout["panes"]["fast"]["epoch"],
        "visible_stall_epoch": timeout["panes"]["stall"]["epoch"],
        "old_old_fallback": True,
        "recovered_event_sequence": recovery_trace["event_sequence"],
        "recovered_epoch": recovery_trace["epoch"],
        "later_resize_promoted": True,
        "release_promoted": True,
    },
    "all_backgrounds_match_epoch_barcodes": True,
    "all_scale_barcodes_match_fractional_output": all(
        close(pane["encoded_scale"], scale, 0.02)
        for frame in decoded
        for pane in frame["panes"].values()
    ),
    "all_extent_borders_complete": all(
        pane["extent_border_coverage"] >= 0.68
        for frame in decoded
        for pane in frame["panes"].values()
    ),
    "captures": decoded,
}
print(json.dumps(result, indent=2, sort_keys=True))
PY
then
  cat "$ANALYSIS_LOG" >&2
  exit 1
fi

MERGED_METRICS="$OUT/metrics-merged.json"
jq --slurp '.[0] + {fiducials: .[1], burst: .[2], window_shell: .[3], region_tree: .[4], interaction_lifecycle: .[5]}' \
  "$METRICS" \
  "$FIDUCIAL_METRICS" \
  "$BURST_METRICS" \
  "$EXTENDED_METRICS" \
  "$REGION_TREE_METRICS" \
  "$LIFECYCLE_METRICS" \
  > "$MERGED_METRICS"
mv "$MERGED_METRICS" "$METRICS"

mapfile -t FRAME_FILES < <(find "$FRAMES_DIR" -maxdepth 1 -name 'frame-*.png' -type f | sort)
if ((${#FRAME_FILES[@]} > 0)); then
  magick montage \
    -font DejaVu-Sans \
    -label '%f' \
    "${FRAME_FILES[@]}" \
    -thumbnail 320x180 \
    -tile 4x \
    -geometry +4+4 \
    "$OUT/contact-sheet.png"
  magick \
    -delay 28 \
    -loop 0 \
    "${FRAME_FILES[@]}" \
    -resize 50% \
    "$OUT/resize.gif"
fi

python3 - \
  "$FRAME_MANIFEST" \
  "$RESIZE_EVENTS" \
  "$METRICS" \
  "$OUTPUT_NAME" \
  "$OUTPUT_SCALE" \
  "$FRACTIONAL_SCALE_SUPPORTED" \
  "$LOGICAL_WIDTH" \
  "$LOGICAL_HEIGHT" \
  "$MODE_WIDTH" \
  "$MODE_HEIGHT" \
  "$EXTENDED_PROOF" \
  "$REGION_TREE_PROOF" \
  "$MANAGED_PROOF" \
  > "$PROOF" <<'PY'
import json
import pathlib
import sys

manifest_path = pathlib.Path(sys.argv[1])
events_path = pathlib.Path(sys.argv[2])
metrics_path = pathlib.Path(sys.argv[3])
frames = [json.loads(line) for line in manifest_path.read_text().splitlines() if line]
events = [json.loads(line) for line in events_path.read_text().splitlines() if line]
metrics = json.loads(metrics_path.read_text())
extended_proof = json.loads(pathlib.Path(sys.argv[11]).read_text())
region_tree_proof = json.loads(pathlib.Path(sys.argv[12]).read_text())
managed_proof = json.loads(pathlib.Path(sys.argv[13]).read_text())
frame_locked_commands = sum(
    max(
        (frame["pointer_ack"]["sequence"] for frame in frames if frame["lane"] == lane),
        default=0,
    )
    for lane in {frame["lane"] for frame in frames}
)
frame_roundtrip = metrics["input_roundtrip_ms"]
burst_commands = metrics["burst"]["input"]["command_count"]
burst_roundtrip = metrics["burst"]["input"]["roundtrip_ms"]
extended_commands = metrics["window_shell"]["input"]["count"]
extended_captures = metrics["window_shell"]["captures"]["total"]
roundtrip_samples = frame_roundtrip["samples"] + burst_roundtrip["samples"]
combined_roundtrip = {
    "samples": roundtrip_samples,
    "average": (
        frame_roundtrip["average"] * frame_roundtrip["samples"]
        + burst_roundtrip["average"] * burst_roundtrip["samples"]
    ) / roundtrip_samples,
    "maximum": max(frame_roundtrip["maximum"], burst_roundtrip["maximum"]),
}
proof = {
    "kind": "brood-resize-overhaul-nested",
    "verified": True,
    "output": {
        "name": sys.argv[4],
        "scale": float(sys.argv[5]),
        "fractional_scale_1_25": sys.argv[6] == "true",
        "logical_size": [int(sys.argv[7]), int(sys.argv[8])],
        "mode_size": [int(sys.argv[9]), int(sys.argv[10])],
    },
    "input": {
        "protocols": [
            "virtual-pointer-input sequence",
            "Sway IPC compositor configure with ordered acknowledgement",
            "virtual-keyboard-key compositor roundtrip",
        ],
        "commands": frame_locked_commands + burst_commands + extended_commands,
        "frame_locked_commands": frame_locked_commands,
        "burst_commands": burst_commands,
        "window_shell_commands": extended_commands,
        "captures": len(frames) + extended_captures,
        "frame_locked_captures": len(frames),
        "window_shell_captures": extended_captures,
        "roundtrip_ms": combined_roundtrip,
        "frame_locked_roundtrip_ms": frame_roundtrip,
        "burst_roundtrip_ms": burst_roundtrip,
    },
    "trace_events": len(events),
    "assertions": {
        "frame_locked_capture_count": len(frames) == 44,
        "one_device_pixel_divider": metrics["one_device_pixel_divider_every_frame"],
        "monotonic_motion": metrics["motion"]["strictly_monotonic"],
        "atomic_release_promotion": metrics["final"]["phase"] == "promoted",
        "final_sharp": metrics["final"]["sharp"],
        "no_degraded_frames": metrics["no_degraded_frames"],
        "divider_chrome_states": (
            metrics["chrome"]["initial"] == "normal"
            and metrics["chrome"]["hover"] == "hover"
            and metrics["chrome"]["captured"] == "active"
            and metrics["chrome"]["active_persists_outside_hit_zone"]
            and metrics["chrome"]["release"] in ("normal", "hover")
            and metrics["chrome"]["states_visually_distinct"]
        ),
        "projected_old_old_pairs": metrics["fiducials"]["normal"]["projected_old_old_pairs"] == 17,
        "promoted_new_new_pairs": metrics["fiducials"]["normal"]["promoted_new_new_pairs"] == 17,
        "no_mixed_waited_pairs": metrics["fiducials"]["normal"]["mixed_waited_pairs"] == 0,
        "timeout_clears_staging": metrics["fiducials"]["timeout"]["staged_count_after_degrade"] == 0,
        "resize_recovers_after_timeout": metrics["fiducials"]["timeout"]["later_resize_promoted"],
        "epoch_backgrounds_and_barcodes": metrics["fiducials"]["all_backgrounds_match_epoch_barcodes"],
        "fractional_scale_barcodes": metrics["fiducials"]["all_scale_barcodes_match_fractional_output"],
        "exact_extent_borders": metrics["fiducials"]["all_extent_borders_complete"],
        "synchronized_input_roundtrips": (
            combined_roundtrip["samples"] == 34
            and combined_roundtrip["maximum"] < 5_000
        ),
        "burst_acknowledgements_ordered": metrics["burst"]["input"]["ordered_acknowledgements"],
        "burst_unique_captured_motions": (
            metrics["burst"]["input"]["captured_motion_count"] == 8
            and metrics["burst"]["input"]["unique_motion_coordinates"] == 8
        ),
        "burst_supersedes_inflight_resize": metrics["burst"]["superseded_path_count"] >= 1,
        "burst_staging_bounded": (
            metrics["burst"]["staging_bounded"]
            and metrics["burst"]["max_staged_count"] <= 1
        ),
        "burst_client_survives": metrics["burst"]["client_survived"],
        "burst_release_becomes_durable": (
            metrics["burst"]["release"]["phase"] == "promoted"
            and metrics["burst"]["release"]["staged_count"] == 0
            and metrics["burst"]["release"]["matches_projected_target"]
            and metrics["burst"]["release"]["matches_pointer_coordinate"]
        ),
        "low_z_divider_visible_over_ordinary_pane": (
            metrics["one_device_pixel_divider_every_frame"]
            and metrics["interaction_lifecycle"]["assertions"]["low_z_divider_fixture_presented"]
        ),
        "release_registry_precedes_capture_clear": (
            metrics["interaction_lifecycle"]["assertions"]["release_registry_precedes_capture_clear"]
        ),
        "final_release_accepted_after_capture_clear": (
            metrics["interaction_lifecycle"]["assertions"]["final_release_accepted_after_capture_clear"]
        ),
        "exact_claim_retires_after_terminal_mailbox": (
            metrics["interaction_lifecycle"]["assertions"]["exact_claim_retires_after_terminal_mailbox"]
        ),
        "explicit_picker_and_keybindings_overlays": (
            extended_proof["assertions"]["picker_is_explicit_elevated_overlay"]
            and extended_proof["assertions"]["keybindings_is_explicit_elevated_overlay"]
            and extended_proof["assertions"]["overlay_close_removes_paint_partition"]
        ),
        "native_window_live_projection": metrics["window_shell"]["assertions"]["native_window_live_projection"],
        "native_window_root_synchronization": (
            metrics["window_shell"]["assertions"]["native_uses_root_synchronization"]
            and metrics["window_shell"]["assertions"]["native_revision_echoed"]
            and extended_proof["assertions"]["native_configure_burst_settles_once"]
            and extended_proof["assertions"]["no_ordinary_layout_during_native_completion"]
        ),
        "recursive_root_atomicity": (
            metrics["window_shell"]["assertions"]["atomic_old_old_then_new_new"]
            and metrics["window_shell"]["assertions"]["no_mixed_waited_pairs"]
            and metrics["window_shell"]["assertions"]["staging_bounded"]
            and metrics["window_shell"]["assertions"]["no_degraded_final"]
        ),
        "session_switcher_owned_layouts": (
            metrics["window_shell"]["assertions"]["sidebar_root_transactions"]
            and metrics["window_shell"]["assertions"]["session_switcher_visibility"]
            and extended_proof["assertions"]["production_shell_claims_root"]
            and extended_proof["assertions"]["workspace_owns_only_recursive_rest"]
        ),
        "inbox_owned_layouts": metrics["window_shell"]["assertions"]["inbox_visibility"],
        "recursive_exact_extents_and_chrome": (
            metrics["window_shell"]["assertions"]["one_device_pixel_divider"]
            and metrics["window_shell"]["assertions"]["exact_gpu_logical_extents"]
            and metrics["window_shell"]["assertions"]["status_bar_extent_stable"]
            and metrics["window_shell"]["assertions"]["fractional_scale_fiducials"]
        ),
        "shell_synchronized_real_input": metrics["window_shell"]["assertions"]["synchronized_real_input"],
        "recursive_region_tree": (
            region_tree_proof["verified"]
            and all(region_tree_proof["assertions"].values())
        ),
        "managed_resize_epoch_correlation": (
            managed_proof["verified"]
            and all(managed_proof["assertions"].values())
        ),
    },
    "artifacts": {
        "frame_manifest": "frame-manifest.jsonl",
        "resize_events": "resize-events.jsonl",
        "metrics": "metrics.json",
        "contact_sheet": "contact-sheet.png",
        "animation": "resize.gif",
        "client_normal_log": "client-normal.log",
        "client_degraded_log": "client-degraded.log",
        "compositor_log": "compositor.log",
        "pointer_log": "pointer.log",
        "burst_input": "burst-input.jsonl",
        "burst_metrics": "burst-metrics.json",
        "interaction_lifecycle": "interaction-lifecycle.json",
        "window_shell_proof": "window-shell/proof.json",
        "window_shell_metrics": "window-shell/metrics.json",
        "window_frame_manifest": "window-shell/window-frame-manifest.jsonl",
        "sidebar_frame_manifest": "window-shell/sidebar-frame-manifest.jsonl",
        "window_contact_sheet": "window-shell/window-contact-sheet.png",
        "sidebar_contact_sheet": "window-shell/sidebar-contact-sheet.png",
        "region_tree_proof": "region-tree/proof.json",
        "region_tree_metrics": "region-tree/metrics.json",
        "region_tree_frame_manifest": "region-tree/frame-manifest.jsonl",
        "managed_proof": "managed/proof.json",
        "managed_metrics": "managed/metrics.json",
        "managed_frame_manifest": "managed/frame-manifest.jsonl",
    },
}
print(json.dumps(proof, indent=2, sort_keys=True))
PY

jq -e '
  .verified == true
  and .assertions.frame_locked_capture_count == true
  and .assertions.one_device_pixel_divider == true
  and .assertions.monotonic_motion == true
  and .assertions.atomic_release_promotion == true
  and .assertions.final_sharp == true
  and .assertions.no_degraded_frames == true
  and .assertions.divider_chrome_states == true
  and .assertions.projected_old_old_pairs == true
  and .assertions.promoted_new_new_pairs == true
  and .assertions.no_mixed_waited_pairs == true
  and .assertions.timeout_clears_staging == true
  and .assertions.resize_recovers_after_timeout == true
  and .assertions.epoch_backgrounds_and_barcodes == true
  and .assertions.fractional_scale_barcodes == true
  and .assertions.exact_extent_borders == true
  and .assertions.synchronized_input_roundtrips == true
  and .assertions.burst_acknowledgements_ordered == true
  and .assertions.burst_unique_captured_motions == true
  and .assertions.burst_supersedes_inflight_resize == true
  and .assertions.burst_staging_bounded == true
  and .assertions.burst_client_survives == true
  and .assertions.burst_release_becomes_durable == true
  and .assertions.low_z_divider_visible_over_ordinary_pane == true
  and .assertions.release_registry_precedes_capture_clear == true
  and .assertions.final_release_accepted_after_capture_clear == true
  and .assertions.exact_claim_retires_after_terminal_mailbox == true
  and .assertions.explicit_picker_and_keybindings_overlays == true
  and .assertions.native_window_live_projection == true
  and .assertions.native_window_root_synchronization == true
  and .assertions.recursive_root_atomicity == true
  and .assertions.session_switcher_owned_layouts == true
  and .assertions.inbox_owned_layouts == true
  and .assertions.recursive_exact_extents_and_chrome == true
  and .assertions.shell_synchronized_real_input == true
  and .assertions.recursive_region_tree == true
  and .assertions.managed_resize_epoch_correlation == true
  and .output.fractional_scale_1_25 == true
' "$PROOF" > /dev/null

echo "resize_overhaul_nested=verified"
echo "proof=$PROOF"
echo "metrics=$METRICS"
echo "manifest=$FRAME_MANIFEST"
echo "events=$RESIZE_EVENTS"
echo "burst=$BURST_METRICS"
echo "window_shell=$EXTENDED_PROOF"
echo "region_tree=$REGION_TREE_PROOF"
echo "managed=$MANAGED_PROOF"
echo "contact_sheet=$OUT/contact-sheet.png"
echo "animation=$OUT/resize.gif"
