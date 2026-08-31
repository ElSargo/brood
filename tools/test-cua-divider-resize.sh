#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

if [[ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]]; then
  exec dbus-run-session -- "$0" "$@"
fi

CUA_DRIVER_BIN="${CUA_DRIVER_BIN:-$(command -v cua-driver || true)}"
if [[ -z "$CUA_DRIVER_BIN" || ! -x "$CUA_DRIVER_BIN" ]]; then
  echo "ERR cua-driver is required; set CUA_DRIVER_BIN to the Cua project driver" >&2
  exit 2
fi
for tool in grim jq magick nix python3 rg sway swaymsg; do
  if ! command -v "$tool" > /dev/null 2>&1; then
    echo "ERR $tool is required for the Cua divider resize grade" >&2
    exit 2
  fi
done

if [[ -n "${BROOD_CUA_PACKAGE:-}" ]]; then
  PACKAGE="$(readlink -f "$BROOD_CUA_PACKAGE")"
else
  PACKAGE="$(nix build .#brood --no-link --print-out-paths)"
  PACKAGE="$(readlink -f "$PACKAGE")"
fi

OUT="${BROOD_CUA_RESIZE_OUT:-$(mktemp -d /tmp/brood-cua-divider-grade.XXXXXX)}"
RUNTIME_DIR="$(mktemp -d /tmp/brood-cua-divider-runtime.XXXXXX)"
SERVER_LOG="$OUT/server.log"
CLIENT_LOG="$OUT/client.log"
COMPOSITOR_LOG="$OUT/compositor.log"
CUA_LOG="$OUT/cua-driver.log"
SAMPLES="$OUT/samples.jsonl"
RUNS="$OUT/runs.jsonl"
METRICS="$OUT/metrics.json"
REPORT="$OUT/report.md"
SWAY_CONFIG="$OUT/sway.conf"
CUA_SOCKET="$RUNTIME_DIR/cua-driver.sock"
DBUS_DESTINATION="org.brood.CuaDividerGrade.p$$"
DRAG_DURATION_MS="${BROOD_CUA_DRAG_DURATION_MS:-1500}"
TARGET_FRAME_RATE_HZ="${BROOD_CUA_TARGET_FRAME_RATE_HZ:-180}"
DRAG_STEPS="${BROOD_CUA_DRAG_STEPS:-$((DRAG_DURATION_MS * TARGET_FRAME_RATE_HZ / 1000))}"
DRAG_REPEATS="${BROOD_CUA_DRAG_REPEATS:-3}"
CAPTURE_INTERVAL_MS="${BROOD_CUA_CAPTURE_INTERVAL_MS:-100}"
MIN_SCORE="${BROOD_CUA_MIN_SCORE:-90}"
for value in "$DRAG_DURATION_MS" "$TARGET_FRAME_RATE_HZ" "$DRAG_STEPS" "$DRAG_REPEATS" "$CAPTURE_INTERVAL_MS"; do
  if [[ ! "$value" =~ ^[1-9][0-9]*$ ]]; then
    echo "ERR Cua resize timing and count values must be positive integers" >&2
    exit 2
  fi
done
if ((CAPTURE_INTERVAL_MS > DRAG_DURATION_MS)); then
  echo "ERR BROOD_CUA_CAPTURE_INTERVAL_MS exceeds the drag duration" >&2
  exit 2
fi
CAPTURE_INTERVAL_SECONDS="$(python3 -c "print($CAPTURE_INTERVAL_MS / 1000)")"
mkdir -p "$OUT/frames"
chmod 700 "$RUNTIME_DIR"
: > "$SAMPLES"
: > "$RUNS"

CLIENT_PID=""
SERVER_PID=""
COMPOSITOR_PID=""
CUA_PID=""
cleanup() {
  local result=$?
  set +e
  terminate_process_bounded "$CLIENT_PID"
  terminate_process_bounded "$SERVER_PID"
  terminate_process_bounded "$CUA_PID"
  terminate_process_bounded "$COMPOSITOR_PID"
  rm -rf -- "$RUNTIME_DIR"
  if ((result != 0)); then
    echo "cua_resize_out=$OUT" >&2
    [[ -f "$CLIENT_LOG" ]] && tail -n 80 "$CLIENT_LOG" >&2
  fi
  return "$result"
}
trap cleanup EXIT

printf '%s\n' \
  'xwayland disable' \
  "output * mode --custom 1280x720@${TARGET_FRAME_RATE_HZ}Hz" \
  'output * scale 1' \
  'seat seat0 fallback true' \
  'default_border none' \
  'default_floating_border none' \
  > "$SWAY_CONFIG"

env -u brood_session \
  RUST_LOG=broodd=info,brood_client=info,smithay=warn,wgpu_core=warn,wgpu_hal=warn \
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
for _ in $(seq 1 300); do
  if ctl status-json 2> /dev/null | jq -e '
    .hosted_runtime_io_children >= 1
    and any(.plugin_actors[]; .[0] | contains("terminal-session"))
  ' > /dev/null; then
    break
  fi
  kill -0 "$SERVER_PID" 2> /dev/null || {
    echo "ERR Brood server exited during Cua grade startup" >&2
    exit 1
  }
  sleep 0.05
done
ctl status-json | jq -e '
  .hosted_runtime_io_children >= 1
  and any(.plugin_actors[]; .[0] | contains("terminal-session"))
' > /dev/null

env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WLR_BACKENDS=headless \
  WLR_HEADLESS_OUTPUTS=1 \
  WLR_LIBINPUT_NO_DEVICES=1 \
  sway --unsupported-gpu --config "$SWAY_CONFIG" \
  > "$COMPOSITOR_LOG" 2>&1 &
COMPOSITOR_PID=$!

WAYLAND_SOCKET=""
SWAY_SOCKET=""
for _ in $(seq 1 200); do
  WAYLAND_SOCKET="$(find "$RUNTIME_DIR" -maxdepth 1 -type s -name 'wayland-*' -printf '%f\n' | head -1)"
  SWAY_SOCKET="$(find "$RUNTIME_DIR" -maxdepth 1 -type s -name 'sway-ipc.*.sock' -print | head -1)"
  [[ -n "$WAYLAND_SOCKET" && -n "$SWAY_SOCKET" ]] && break
  kill -0 "$COMPOSITOR_PID" 2> /dev/null || {
    echo "ERR headless Sway exited during Cua grade startup" >&2
    exit 1
  }
  sleep 0.05
done
[[ -n "$WAYLAND_SOCKET" && -n "$SWAY_SOCKET" ]]

wayland_env() {
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" SWAYSOCK="$SWAY_SOCKET" "$@"
}

wayland_env swaymsg -t get_outputs -r > "$OUT/sway-outputs.json"
TARGET_REFRESH_MILLIHZ=$((TARGET_FRAME_RATE_HZ * 1000))
jq -e --argjson refresh "$TARGET_REFRESH_MILLIHZ" '
  any(.[]; .active and .current_mode.refresh == $refresh)
' "$OUT/sway-outputs.json" > /dev/null

env -u brood_session \
  XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  WINIT_UNIX_BACKEND=wayland \
  WGPU_BACKEND=vulkan \
  BROOD_TRACE_RESIZE=1 \
  BROOD_TRACE_PLUGIN_INPUT=1 \
  BROOD_TRACE_PLUGIN_GRAPHICS=1 \
  BROOD_TRACE_PLUGIN_WORKERS=1 \
  RUST_BACKTRACE=1 \
  RUST_LOG=broodd=info,brood_client=info,smithay=warn,wgpu_core=warn,wgpu_hal=warn \
  "$PACKAGE/bin/brood-client" \
  --config "$PACKAGE/brood.toml" \
  --dbus-destination "$DBUS_DESTINATION" \
  --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
  > "$CLIENT_LOG" 2>&1 &
CLIENT_PID=$!

for _ in $(seq 1 400); do
  if rg -q 'started presentation WASM actor id=terminal-ui' "$CLIENT_LOG" \
    && rg -q 'resize-present .*divider_x=[0-9]+ .*phase=(settled|promoted)' "$CLIENT_LOG"; then
    break
  fi
  kill -0 "$CLIENT_PID" 2> /dev/null || {
    echo "ERR Brood client exited before the production divider appeared" >&2
    exit 1
  }
  sleep 0.05
done
rg -q 'started presentation WASM actor id=terminal-ui' "$CLIENT_LOG"
rg -q 'resize-present .*divider_x=[0-9]+ .*phase=(settled|promoted)' "$CLIENT_LOG"

env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  CUA_DRIVER_RS_ENABLE_WAYLAND=1 \
  "$CUA_DRIVER_BIN" serve \
  --socket "$CUA_SOCKET" \
  --no-overlay \
  --dangerously-bypass-approvals \
  > "$CUA_LOG" 2>&1 &
CUA_PID=$!
for _ in $(seq 1 200); do
  if "$CUA_DRIVER_BIN" call get_screen_size '{}' --socket "$CUA_SOCKET" \
    > "$OUT/cua-screen.json" 2> /dev/null; then
    break
  fi
  kill -0 "$CUA_PID" 2> /dev/null || {
    echo "ERR Cua Driver exited during nested Wayland startup" >&2
    exit 1
  }
  sleep 0.05
done
"$CUA_DRIVER_BIN" call get_screen_size '{}' --socket "$CUA_SOCKET" \
  > "$OUT/cua-screen.json"

"$CUA_DRIVER_BIN" call list_windows '{}' --socket "$CUA_SOCKET" > "$OUT/cua-windows.json"
WINDOW_ID="$(jq -r '.windows[] | select(.title == "Brood") | .window_id' "$OUT/cua-windows.json" | head -1)"
[[ "$WINDOW_ID" =~ ^[0-9]+$ ]]

TREE_JSON="$(wayland_env swaymsg -t get_tree -r)"
WINDOW_X="$(jq -r --argjson pid "$CLIENT_PID" '.. | objects | select(.pid? == $pid) | .rect.x' <<< "$TREE_JSON" | head -1)"
WINDOW_Y="$(jq -r --argjson pid "$CLIENT_PID" '.. | objects | select(.pid? == $pid) | .rect.y' <<< "$TREE_JSON" | head -1)"
WINDOW_WIDTH="$(jq -r --argjson pid "$CLIENT_PID" '.. | objects | select(.pid? == $pid) | .rect.width' <<< "$TREE_JSON" | head -1)"
WINDOW_HEIGHT="$(jq -r --argjson pid "$CLIENT_PID" '.. | objects | select(.pid? == $pid) | .rect.height' <<< "$TREE_JSON" | head -1)"
[[ "$WINDOW_X" =~ ^-?[0-9]+$ && "$WINDOW_Y" =~ ^-?[0-9]+$ ]]
[[ "$WINDOW_WIDTH" =~ ^[0-9]+$ && "$WINDOW_HEIGHT" =~ ^[0-9]+$ ]]

wayland_env grim -l 0 -o HEADLESS-1 "$OUT/before.png"

for run in $(seq 1 "$DRAG_REPEATS"); do
  kill -0 "$CLIENT_PID" 2> /dev/null || break
  START_DIVIDER="$(sed -n 's/.* divider_x=\([0-9][0-9]*\) .*/\1/p' "$CLIENT_LOG" | tail -1)"
  [[ "$START_DIVIDER" =~ ^[0-9]+$ ]]
  if ((run % 2 == 1)); then
    TARGET_DIVIDER=$((START_DIVIDER - 196))
  else
    TARGET_DIVIDER=$((START_DIVIDER + 196))
  fi
  ((TARGET_DIVIDER < 360)) && TARGET_DIVIDER=360
  ((TARGET_DIVIDER > WINDOW_WIDTH - 240)) && TARGET_DIVIDER=$((WINDOW_WIDTH - 240))
  FROM_X=$((WINDOW_X + START_DIVIDER))
  TO_X=$((WINDOW_X + TARGET_DIVIDER))
  DRAG_Y=$((WINDOW_Y + WINDOW_HEIGHT / 2))
  START_LINE=$(( $(wc -l < "$CLIENT_LOG") + 1 ))
  RETIRE_BASELINE="$(rg -c 'pointer-interaction retired interaction=.*terminal-request-notification-delivered' "$CLIENT_LOG" || true)"
  START_NS="$(date +%s%N)"
  DRAG_ARGS="$(jq -cn \
    --argjson pid "$CLIENT_PID" \
    --argjson window_id "$WINDOW_ID" \
    --argjson from_x "$FROM_X" \
    --argjson from_y "$DRAG_Y" \
    --argjson to_x "$TO_X" \
    --argjson to_y "$DRAG_Y" \
    --argjson duration_ms "$DRAG_DURATION_MS" \
    --argjson steps "$DRAG_STEPS" \
    --arg session "brood-divider-grade-$run" \
    '{pid:$pid,window_id:$window_id,from_x:$from_x,from_y:$from_y,to_x:$to_x,to_y:$to_y,duration_ms:$duration_ms,steps:$steps,delivery_mode:"foreground",session:$session}')"
  "$CUA_DRIVER_BIN" call drag "$DRAG_ARGS" --socket "$CUA_SOCKET" \
    > "$OUT/cua-drag-$run.json" 2> "$OUT/cua-drag-$run.stderr" &
  DRAG_PID=$!

  SAMPLE_COUNT=$((DRAG_DURATION_MS / CAPTURE_INTERVAL_MS))
  for sample in $(seq 1 "$SAMPLE_COUNT"); do
    sleep "$CAPTURE_INTERVAL_SECONDS"
    CAPTURE="$OUT/frames/run-${run}-$(printf '%03d' "$sample").png"
    CAPTURE_NS="$(date +%s%N)"
    wayland_env grim -l 0 -o HEADLESS-1 "$CAPTURE"
    jq -cn \
      --argjson run "$run" \
      --argjson sample "$sample" \
      --arg file "${CAPTURE#$OUT/}" \
      --argjson captured_ns "$CAPTURE_NS" \
      --argjson drag_started_ns "$START_NS" \
      --argjson duration_ms "$DRAG_DURATION_MS" \
      --argjson start_x "$START_DIVIDER" \
      --argjson target_x "$TARGET_DIVIDER" \
      '{run:$run,sample:$sample,file:$file,captured_ns:$captured_ns,drag_started_ns:$drag_started_ns,duration_ms:$duration_ms,start_x:$start_x,target_x:$target_x}' \
      >> "$SAMPLES"
  done

  set +e
  wait "$DRAG_PID"
  DRAG_RESULT=$?
  set -e
  for _ in $(seq 1 100); do
    RETIRE_COUNT="$(rg -c 'pointer-interaction retired interaction=.*terminal-request-notification-delivered' "$CLIENT_LOG" || true)"
    if ! kill -0 "$CLIENT_PID" 2> /dev/null \
      || ((RETIRE_COUNT > RETIRE_BASELINE)); then
      break
    fi
    sleep 0.02
  done
  END_NS="$(date +%s%N)"
  END_LINE="$(wc -l < "$CLIENT_LOG")"
  CLIENT_ALIVE=false
  kill -0 "$CLIENT_PID" 2> /dev/null && CLIENT_ALIVE=true
  FINAL_DIVIDER="$(sed -n 's/.* divider_x=\([0-9][0-9]*\) .*/\1/p' "$CLIENT_LOG" | tail -1)"
  FINAL_DIVIDER="${FINAL_DIVIDER:-$START_DIVIDER}"
  FINAL_CAPTURE="$OUT/frames/run-${run}-final.png"
  wayland_env grim -l 0 -o HEADLESS-1 "$FINAL_CAPTURE"
  jq -cn \
    --argjson run "$run" \
    --argjson start_line "$START_LINE" \
    --argjson end_line "$END_LINE" \
    --argjson started_ns "$START_NS" \
    --argjson ended_ns "$END_NS" \
    --argjson duration_ms "$DRAG_DURATION_MS" \
    --argjson target_frame_rate_hz "$TARGET_FRAME_RATE_HZ" \
    --argjson steps "$DRAG_STEPS" \
    --argjson start_x "$START_DIVIDER" \
    --argjson target_x "$TARGET_DIVIDER" \
    --argjson final_x "$FINAL_DIVIDER" \
    --argjson drag_result "$DRAG_RESULT" \
    --argjson client_alive "$CLIENT_ALIVE" \
    --arg final_file "${FINAL_CAPTURE#$OUT/}" \
    '{run:$run,start_line:$start_line,end_line:$end_line,started_ns:$started_ns,ended_ns:$ended_ns,duration_ms:$duration_ms,target_frame_rate_hz:$target_frame_rate_hz,steps:$steps,start_x:$start_x,target_x:$target_x,final_x:$final_x,drag_result:$drag_result,client_alive:$client_alive,final_file:$final_file}' \
    >> "$RUNS"
done

ANALYZER_ARGS=(
  "$ROOT/tools/analyze-cua-divider-resize.py"
  --root "$OUT"
  --client-log "$CLIENT_LOG"
  --runs "$RUNS"
  --samples "$SAMPLES"
  --metrics "$METRICS"
  --report "$REPORT"
  --minimum-score "$MIN_SCORE"
)
if [[ "${BROOD_CUA_ALLOW_FAIL:-0}" == 1 ]]; then
  ANALYZER_ARGS+=(--allow-fail)
fi
python3 "${ANALYZER_ARGS[@]}"

echo "cua_resize_metrics=$METRICS"
echo "cua_resize_report=$REPORT"
echo "cua_resize_frames=$OUT/frames"
