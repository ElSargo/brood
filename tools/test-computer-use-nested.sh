#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]]; then
  exec dbus-run-session -- "$0" "$@"
fi

for tool in cc jq magick pkg-config python3 sway wayland-scanner weston-simple-egl weston-simple-shm; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "ERR $tool is required for the computer-use proof" >&2
    exit 2
  }
done

if [[ -n "${BROOD_COMPUTER_USE_BIN:-}" ]]; then
  BROOD_BIN="$(readlink -f "$BROOD_COMPUTER_USE_BIN")"
  CONFIG="$ROOT/brood.toml"
elif [[ -n "${BROOD_COMPUTER_USE_PACKAGE:-}" ]]; then
  PACKAGE="$BROOD_COMPUTER_USE_PACKAGE"
  PACKAGE="$(readlink -f "$PACKAGE")"
  BROOD_BIN="$PACKAGE/bin/brood"
  CONFIG="$PACKAGE/brood.toml"
else
  PACKAGE="$(nix build "path:$ROOT#brood" --no-link --print-out-paths)"
  PACKAGE="$(readlink -f "$PACKAGE")"
  BROOD_BIN="$PACKAGE/bin/brood"
  CONFIG="$PACKAGE/brood.toml"
fi
CLIENT_BIN="$(dirname "$BROOD_BIN")/brood-client"
[[ -x "$CLIENT_BIN" ]] || {
  echo "ERR brood-client is required beside $BROOD_BIN for takeover proof" >&2
  exit 2
}
CLIENT_INIT_ARGS=()
CLIENT_CONFIG="$CONFIG"
if [[ -n "${BROOD_COMPUTER_USE_CLIENT_INIT:-}" ]]; then
  CLIENT_INIT_ARGS=(
    --no-init
    --wasm-actor "session-switcher=$(readlink -f "$BROOD_COMPUTER_USE_CLIENT_INIT")"
  )
  CLIENT_CONFIG=""
fi
OUT="${BROOD_COMPUTER_USE_OUT:-$(mktemp -d /tmp/brood-computer-use.XXXXXX)}"
RUNTIME_DIR="$(mktemp -d /tmp/brood-computer-use-runtime.XXXXXX)"
PRESENT_RUNTIME_DIR="$(mktemp -d /tmp/brood-computer-use-present.XXXXXX)"
DESTINATION="org.brood.ComputerUseTest.p$$"
SERVER_LOG="$OUT/server.log"
mkdir -p "$OUT"
chmod 700 "$RUNTIME_DIR"
chmod 700 "$PRESENT_RUNTIME_DIR"

SERVER_PID=""
APP_PID=""
BATCH_PID=""
CLIENT_PID=""
SWAY_PID=""
cleanup() {
  local result=$?
  set +e
  for pid in "$BATCH_PID" "$CLIENT_PID" "$SWAY_PID" "$APP_PID" "$SERVER_PID"; do
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null
      wait "$pid" 2>/dev/null
    fi
  done
  rm -rf -- "$RUNTIME_DIR" "$PRESENT_RUNTIME_DIR"
  if ((result != 0)); then
    tail -n 200 "$SERVER_LOG" >&2 || true
  fi
  return "$result"
}
trap cleanup EXIT

read -r -a GTK_FLAGS <<< "$(pkg-config --cflags --libs gtk4)"
cc -std=c11 -Wall -Wextra -Werror \
  tools/computer-use-proof-client.c \
  -o "$OUT/computer-use-proof-client" \
  "${GTK_FLAGS[@]}" -lm
VIRTUAL_POINTER_XML="$(pkg-config --variable=pkgdatadir wlr-protocols)/unstable/wlr-virtual-pointer-unstable-v1.xml"
wayland-scanner client-header "$VIRTUAL_POINTER_XML" \
  "$OUT/wlr-virtual-pointer-unstable-v1-client-protocol.h"
wayland-scanner private-code "$VIRTUAL_POINTER_XML" \
  "$OUT/wlr-virtual-pointer-unstable-v1-protocol.c"
read -r -a WAYLAND_FLAGS <<< "$(pkg-config --cflags --libs wayland-client)"
cc -std=c11 -Wall -Wextra -Werror -I "$OUT" \
  tools/virtual-pointer-click.c \
  "$OUT/wlr-virtual-pointer-unstable-v1-protocol.c" \
  -o "$OUT/virtual-pointer-click" \
  "${WAYLAND_FLAGS[@]}"

env -u brood_session \
  XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  RUST_LOG="${RUST_LOG:-broodd=info,smithay=warn}" \
  "$BROOD_BIN" run \
  --no-session-runner \
  --no-default-modules \
  --no-init \
  --headless \
  --config "$CONFIG" \
  --dbus-destination "$DESTINATION" \
  --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
  >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!

ctl() {
  env -u brood_session XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    "$BROOD_BIN" ctl \
    --dbus-destination "$DESTINATION" \
    --dbus-address "$DBUS_SESSION_BUS_ADDRESS" "$@"
}

for _ in $(seq 1 300); do
  STATUS="$(ctl status-json 2>/dev/null || true)"
  [[ -n "$STATUS" ]] && break
  kill -0 "$SERVER_PID" 2>/dev/null || exit 1
  sleep 0.05
done
[[ "$(jq -r .presentation <<<"$STATUS")" == "Detached" ]]
WAYLAND_DISPLAY="$(jq -r .wayland_display <<<"$STATUS")"

capture_pair() {
  local kind="$1"
  local first="$OUT/${kind}-first"
  local second="$OUT/${kind}-second"
  for _ in $(seq 1 300); do
    STATUS="$(ctl status-json)"
    if (( $(jq -r .computer_use_capture_attempts <<<"$STATUS") > 0 )) \
      && (( $(jq -r .total_windows <<<"$STATUS") > 0 )); then
      break
    fi
    sleep 0.05
  done
  ctl debug dump "$first" surfaces >/dev/null
  sleep 0.2
  ctl debug dump "$second" surfaces >/dev/null
  test -s "$first/automation.png"
  test -s "$second/automation.png"
  jq -e '.viewport_width == 1440 and .viewport_height == 900 and .active_window != null and (.windows | length) >= 1' \
    "$second/automation.json" >/dev/null
  [[ "$(magick identify -format '%wx%h' "$second/automation.png")" == "1440x900" ]]
  [[ "$(sha256sum "$first/automation.png" | cut -d' ' -f1)" != \
     "$(sha256sum "$second/automation.png" | cut -d' ' -f1)" ]]
}

env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_DISPLAY" \
  weston-simple-shm >"$OUT/shm-client.log" 2>&1 &
APP_PID=$!
capture_pair shm
kill "$APP_PID"
wait "$APP_PID" 2>/dev/null || true
APP_PID=""

SHM_FRAME_SEQUENCE="$(jq -r .frame_sequence "$OUT/shm-second/automation.json")"
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_DISPLAY" \
  weston-simple-egl >"$OUT/dmabuf-client.log" 2>&1 &
APP_PID=$!
capture_pair dmabuf
DMA_FRAME_SEQUENCE="$(jq -r .frame_sequence "$OUT/dmabuf-second/automation.json")"
kill "$APP_PID"
wait "$APP_PID" 2>/dev/null || true
APP_PID=""
STATUS="$(ctl status-json)"
printf '%s\n' "$STATUS" >"$OUT/status.json"

jq -n \
  --argjson shm_frame_sequence "$SHM_FRAME_SEQUENCE" \
  --argjson dmabuf_frame_sequence "$DMA_FRAME_SEQUENCE" \
  --argjson status "$STATUS" \
  '{
    presentation: $status.presentation,
    shm_frame_sequence: $shm_frame_sequence,
    dmabuf_frame_sequence: $dmabuf_frame_sequence,
    capture_attempts: $status.computer_use_capture_attempts,
    capture_failures: $status.computer_use_capture_failures,
    cadence_drops: $status.computer_use_capture_cadence_drops,
    observation_backpressure_drops: $status.computer_use_observation_backpressure_drops,
    dmabuf_imports_accepted: $status.dmabuf_accepted
  }' >"$OUT/metrics.json"

jq -e '
  .presentation == "Detached"
  and .dmabuf_frame_sequence > .shm_frame_sequence
  and .capture_attempts >= 4
  and .capture_failures == 0
  and .dmabuf_imports_accepted >= 1
' "$OUT/metrics.json" >/dev/null

ACTION_STATE="$OUT/action-state.json"
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_DISPLAY" \
  "$OUT/computer-use-proof-client" "$ACTION_STATE" >"$OUT/action-client.log" 2>&1 &
APP_PID=$!
for _ in $(seq 1 300); do
  ctl debug dump "$OUT/action-ready" surfaces >/dev/null 2>&1 || true
  if [[ -s "$OUT/action-ready/automation.json" ]] \
    && jq -e 'any(.windows[]; .application_id == "org.brood.ComputerUseProof")' \
      "$OUT/action-ready/automation.json" >/dev/null; then
    break
  fi
  sleep 0.05
done
ACTION_PANE="$(jq -r '.windows[] | select(.application_id == "org.brood.ComputerUseProof") | .pane' "$OUT/action-ready/automation.json")"
LEASE="$(ctl computer-use start phase2-proof 1 "$ACTION_PANE" 30000)"
LEASE_GENERATION="$(jq -r .generation <<<"$LEASE")"
for _ in $(seq 1 300); do
  ctl debug dump "$OUT/action-ready" surfaces >/dev/null
  if jq -e '.windows[] | select(.pane == '"$ACTION_PANE"' and .content_width > 0 and .content_height > 0)' \
    "$OUT/action-ready/automation.json" >/dev/null; then
    break
  fi
  sleep 0.05
done
ACTION_WIDTH="$(jq -r '.windows[] | select(.pane == '"$ACTION_PANE"') | .content_width' "$OUT/action-ready/automation.json")"
ACTION_HEIGHT="$(jq -r '.windows[] | select(.pane == '"$ACTION_PANE"') | .content_height' "$OUT/action-ready/automation.json")"
((ACTION_WIDTH > 0 && ACTION_HEIGHT > 0))
ACTION_BATCH="$(jq -nc \
  --arg task phase2-proof \
  --argjson generation "$LEASE_GENERATION" \
  --argjson pane "$ACTION_PANE" \
  --arg text "Agent42" \
  '{
    protocol_version: 1,
    task: $task,
    lease_generation: $generation,
    expected_sequence: 1,
    target_session: 1,
    target_window: $pane,
    approval_proof: null,
    actions: [
      {type:"move", x:640, y:440},
      {type:"click", x:80, y:80, button:"left"},
      {type:"double_click", x:180, y:80, button:"left"},
      {type:"drag", path:[{x:80,y:200},{x:160,y:240},{x:240,y:280},{x:300,y:320}], button:"left"},
      {type:"scroll", x:500, y:250, delta_x:0, delta_y:90},
      {type:"click", x:100, y:400, button:"left"},
      {type:"type", text:$text},
      {type:"keypress", keys:["CTRL","L"]},
      {type:"wait", duration_ms:100},
      {type:"screenshot"}
    ]
  }')"
ctl computer-use batch "$ACTION_BATCH" >"$OUT/action-response.json"
for _ in $(seq 1 100); do
  if jq -e '
    .click_count >= 4
    and .double_clicks >= 1
    and .drag_distance >= 150
    and .scroll_events >= 1
    and (.typed_text | contains("Agent42"))
    and .shortcut_count >= 1
  ' "$ACTION_STATE" >/dev/null 2>&1; then
    break
  fi
  sleep 0.05
done
jq -e '
  .click_count >= 4
  and .double_clicks >= 1
  and .drag_distance >= 150
  and .scroll_events >= 1
  and (.typed_text | contains("Agent42"))
  and .shortcut_count >= 1
  and (.button_down | not)
  and (.control_down | not)
' "$ACTION_STATE" >/dev/null
cp "$ACTION_STATE" "$OUT/action-state-final.json"
python3 -c \
  'import json, pathlib, sys; pathlib.Path(sys.argv[2]).write_bytes(bytes(json.load(open(sys.argv[1]))["observation"]["png"]))' \
  "$OUT/action-response.json" "$OUT/action-observation.png"
[[ "$(magick identify -format '%wx%h' "$OUT/action-observation.png")" == "1440x900" ]]
jq -e '
  .sequence == 1
  and (.results | length) == 10
  and all(.results[]; .status == "completed")
  and .observation.settle.settled
  and .observation.settle.quiet_ms >= 75
  and .observation.settle.frame_sequence > 0
  and .observation.settle.input_sequence > 0
' "$OUT/action-response.json" >/dev/null

STALE_BATCH="$(jq -nc --argjson generation "$((LEASE_GENERATION + 1))" --argjson pane "$ACTION_PANE" '{protocol_version:1,task:"phase2-proof",lease_generation:$generation,expected_sequence:2,target_session:1,target_window:$pane,approval_proof:null,actions:[{type:"screenshot"}]}')"
if ctl computer-use batch "$STALE_BATCH" >"$OUT/stale-lease.out" 2>"$OUT/stale-lease.err"; then
  echo "ERR stale lease was accepted" >&2
  exit 1
fi
CROSS_SESSION_BATCH="$(jq -nc --argjson generation "$LEASE_GENERATION" --argjson pane "$ACTION_PANE" '{protocol_version:1,task:"phase2-proof",lease_generation:$generation,expected_sequence:2,target_session:2,target_window:$pane,approval_proof:null,actions:[{type:"screenshot"}]}')"
if ctl computer-use batch "$CROSS_SESSION_BATCH" >"$OUT/cross-session.out" 2>"$OUT/cross-session.err"; then
  echo "ERR cross-session batch was accepted" >&2
  exit 1
fi
OUT_OF_BOUNDS_X=$((ACTION_WIDTH + 1))
((OUT_OF_BOUNDS_X < 1440))
OUT_OF_BOUNDS_BATCH="$(jq -nc --argjson generation "$LEASE_GENERATION" --argjson pane "$ACTION_PANE" --argjson x "$OUT_OF_BOUNDS_X" '{protocol_version:1,task:"phase2-proof",lease_generation:$generation,expected_sequence:2,target_session:1,target_window:$pane,approval_proof:null,actions:[{type:"click",x:$x,y:10,button:"left"}]}')"
if ctl computer-use batch "$OUT_OF_BOUNDS_BATCH" >"$OUT/out-of-bounds.out" 2>"$OUT/out-of-bounds.err"; then
  echo "ERR out-of-bounds batch was accepted" >&2
  exit 1
fi
ctl computer-use stop phase2-proof >/dev/null

LONG_PATH="$(jq -nc '[range(0;200) as $i | {x:(50 + (($i % 100) * 4)), y:(170 + (($i % 40) * 4))}]')"
drag_batch() {
  local task="$1"
  local generation="$2"
  jq -nc \
    --arg task "$task" \
    --argjson generation "$generation" \
    --argjson pane "$ACTION_PANE" \
    --argjson path "$LONG_PATH" \
    '{protocol_version:1,task:$task,lease_generation:$generation,expected_sequence:1,target_session:1,target_window:$pane,approval_proof:null,actions:[{type:"drag",path:$path,button:"left"},{type:"screenshot"}]}'
}

STOP_LEASE="$(ctl computer-use start stop-cleanup 1 "$ACTION_PANE" 30000)"
STOP_GENERATION="$(jq -r .generation <<<"$STOP_LEASE")"
ctl computer-use batch "$(drag_batch stop-cleanup "$STOP_GENERATION")" \
  >"$OUT/stop-cleanup-response.json" &
BATCH_PID=$!
for _ in $(seq 1 100); do
  STATUS="$(ctl status-json)"
  [[ "$(jq -r .computer_use_held_buttons <<<"$STATUS")" == "1" ]] && break
  sleep 0.01
done
[[ "$(jq -r .computer_use_held_buttons <<<"$STATUS")" == "1" ]]
ctl computer-use stop stop-cleanup >/dev/null
wait "$BATCH_PID"
BATCH_PID=""
jq -e '.results[-1].status == "cancelled" and .results[-1].failure == "cancelled"' \
  "$OUT/stop-cleanup-response.json" >/dev/null
STATUS="$(ctl status-json)"
jq -e '.computer_use_batch_pending == false and .computer_use_held_buttons == 0 and .computer_use_held_keys == 0' \
  <<<"$STATUS" >/dev/null
for _ in $(seq 1 100); do
  jq -e '.button_down == false' "$ACTION_STATE" >/dev/null 2>&1 && break
  sleep 0.01
done
jq -e '.button_down == false' "$ACTION_STATE" >/dev/null

KEY_LEASE="$(ctl computer-use start key-cleanup 1 "$ACTION_PANE" 30000)"
KEY_GENERATION="$(jq -r .generation <<<"$KEY_LEASE")"
KEY_BATCH="$(jq -nc \
  --argjson generation "$KEY_GENERATION" \
  --argjson pane "$ACTION_PANE" \
  '{protocol_version:1,task:"key-cleanup",lease_generation:$generation,expected_sequence:1,target_session:1,target_window:$pane,approval_proof:null,actions:[{type:"keypress",keys:["CTRL","L"]},{type:"screenshot"}]}')"
ctl computer-use batch "$KEY_BATCH" >"$OUT/key-cleanup-response.json" &
BATCH_PID=$!
for _ in $(seq 1 100); do
  STATUS="$(ctl status-json)"
  (( $(jq -r .computer_use_held_keys <<<"$STATUS") > 0 )) && break
  sleep 0.005
done
(( $(jq -r .computer_use_held_keys <<<"$STATUS") > 0 ))
ctl computer-use stop key-cleanup >/dev/null
wait "$BATCH_PID"
BATCH_PID=""
jq -e '.results[-1].status == "cancelled" and .results[-1].failure == "cancelled"' \
  "$OUT/key-cleanup-response.json" >/dev/null
for _ in $(seq 1 100); do
  jq -e '.control_down == false' "$ACTION_STATE" >/dev/null 2>&1 && break
  sleep 0.01
done
jq -e '.control_down == false' "$ACTION_STATE" >/dev/null
STATUS="$(ctl status-json)"
jq -e '.computer_use_held_buttons == 0 and .computer_use_held_keys == 0' \
  <<<"$STATUS" >/dev/null

EXPIRY_LEASE="$(ctl computer-use start expiry-cleanup 1 "$ACTION_PANE" 1000)"
EXPIRY_GENERATION="$(jq -r .generation <<<"$EXPIRY_LEASE")"
ctl computer-use batch "$(drag_batch expiry-cleanup "$EXPIRY_GENERATION")" \
  >"$OUT/expiry-cleanup-response.json"
jq -e '.results[-1].status == "failed" and .results[-1].failure == "input_rejected"' \
  "$OUT/expiry-cleanup-response.json" >/dev/null
jq -e '.state == "ready"' < <(ctl computer-use inspect expiry-cleanup) >/dev/null
STATUS="$(ctl status-json)"
jq -e '.computer_use_batch_pending == false and .computer_use_held_buttons == 0 and .computer_use_held_keys == 0' \
  <<<"$STATUS" >/dev/null

TARGET_LEASE="$(ctl computer-use start target-cleanup 1 "$ACTION_PANE" 30000)"
TARGET_GENERATION="$(jq -r .generation <<<"$TARGET_LEASE")"
ctl computer-use batch "$(drag_batch target-cleanup "$TARGET_GENERATION")" \
  >"$OUT/target-cleanup-response.json" &
BATCH_PID=$!
for _ in $(seq 1 100); do
  STATUS="$(ctl status-json)"
  [[ "$(jq -r .computer_use_held_buttons <<<"$STATUS")" == "1" ]] && break
  sleep 0.01
done
[[ "$(jq -r .computer_use_held_buttons <<<"$STATUS")" == "1" ]]
kill "$APP_PID"
wait "$APP_PID" 2>/dev/null || true
APP_PID=""
wait "$BATCH_PID"
BATCH_PID=""
jq -e '.results[-1].status == "failed" and .results[-1].failure == "target_gone"' \
  "$OUT/target-cleanup-response.json" >/dev/null
STATUS="$(ctl status-json)"
jq -e '.computer_use_batch_pending == false and .computer_use_held_buttons == 0 and .computer_use_held_keys == 0' \
  <<<"$STATUS" >/dev/null
ctl computer-use stop target-cleanup >/dev/null

HUMAN_STATE="$OUT/human-state.json"
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_DISPLAY" \
  "$OUT/computer-use-proof-client" "$HUMAN_STATE" >"$OUT/human-app.log" 2>&1 &
APP_PID=$!
for _ in $(seq 1 300); do
  ctl debug dump "$OUT/human-ready" surfaces >/dev/null 2>&1 || true
  HUMAN_PANE="$(jq -r '.windows[] | select(.application_id == "org.brood.ComputerUseProof") | .pane' "$OUT/human-ready/automation.json" 2>/dev/null || true)"
  [[ -n "$HUMAN_PANE" ]] && break
  sleep 0.05
done
[[ -n "$HUMAN_PANE" ]]
ACTION_PANE="$HUMAN_PANE"

printf '%s\n' \
  'xwayland disable' \
  'output * mode --custom 800x600@60Hz' \
  'seat seat0 fallback true' \
  'default_border none' \
  >"$OUT/sway.conf"
env XDG_RUNTIME_DIR="$PRESENT_RUNTIME_DIR" \
  WLR_BACKENDS=headless WLR_HEADLESS_OUTPUTS=1 WLR_LIBINPUT_NO_DEVICES=1 \
  sway --unsupported-gpu --config "$OUT/sway.conf" >"$OUT/sway.log" 2>&1 &
SWAY_PID=$!
for _ in $(seq 1 300); do
  PRESENT_DISPLAY="$(find "$PRESENT_RUNTIME_DIR" -maxdepth 1 -type s -name 'wayland-*' -printf '%f\n' | head -1)"
  [[ -n "$PRESENT_DISPLAY" ]] && break
  kill -0 "$SWAY_PID" 2>/dev/null || exit 1
  sleep 0.05
done
[[ -n "$PRESENT_DISPLAY" ]]
if [[ -z "$CLIENT_CONFIG" ]]; then
  printf '\n' >"$OUT/client.toml"
  CLIENT_CONFIG="$OUT/client.toml"
fi
env -u brood_session \
  XDG_RUNTIME_DIR="$PRESENT_RUNTIME_DIR" WAYLAND_DISPLAY="$PRESENT_DISPLAY" \
  BROOD_FRAME_TRANSPORT=shared-memory WGPU_BACKEND=vulkan \
  RUST_LOG="${RUST_LOG:-brood_client=info,wgpu_core=warn,wgpu_hal=warn}" \
  "$CLIENT_BIN" --config "$CLIENT_CONFIG" "${CLIENT_INIT_ARGS[@]}" \
  --dbus-destination "$DESTINATION" --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
  >"$OUT/presentation-client.log" 2>&1 &
CLIENT_PID=$!
for _ in $(seq 1 600); do
  STATUS="$(ctl status-json)"
  [[ "$(jq -r .presentation <<<"$STATUS")" == "Attached" ]] && break
  kill -0 "$CLIENT_PID" 2>/dev/null || exit 1
  sleep 0.05
done
[[ "$(jq -r .presentation <<<"$STATUS")" == "Attached" ]]

HUMAN_LEASE="$(ctl computer-use start human-takeover 1 "$HUMAN_PANE" 30000)"
HUMAN_GENERATION="$(jq -r .generation <<<"$HUMAN_LEASE")"
ctl computer-use batch "$(drag_batch human-takeover "$HUMAN_GENERATION")" \
  >"$OUT/human-takeover-response.json" &
BATCH_PID=$!
for _ in $(seq 1 100); do
  STATUS="$(ctl status-json)"
  [[ "$(jq -r .computer_use_held_buttons <<<"$STATUS")" == "1" ]] && break
  sleep 0.01
done
[[ "$(jq -r .computer_use_held_buttons <<<"$STATUS")" == "1" ]]
env XDG_RUNTIME_DIR="$PRESENT_RUNTIME_DIR" WAYLAND_DISPLAY="$PRESENT_DISPLAY" \
  "$OUT/virtual-pointer-click" 400 300 800 600
wait "$BATCH_PID"
BATCH_PID=""
jq -e '.results[-1].status == "cancelled" and .results[-1].failure == "cancelled"' \
  "$OUT/human-takeover-response.json" >/dev/null
ctl computer-use inspect human-takeover >"$OUT/human-task.json"
jq -e '.state == "paused"' "$OUT/human-task.json" >/dev/null
STATUS="$(ctl status-json)"
printf '%s\n' "$STATUS" >"$OUT/human-status.json"
jq -e '
  .presentation == "Attached"
  and .computer_use_lease_generation == null
  and .computer_use_batch_pending == false
  and .computer_use_held_buttons == 0
  and .computer_use_held_keys == 0
' <<<"$STATUS" >/dev/null

jq -n \
  --argjson pane "$ACTION_PANE" \
  --argjson width "$ACTION_WIDTH" \
  --argjson height "$ACTION_HEIGHT" \
  --argjson generation "$LEASE_GENERATION" \
  --slurpfile result "$OUT/action-response.json" \
  --slurpfile state "$OUT/action-state-final.json" \
  '{pane:$pane,width:$width,height:$height,lease_generation:$generation,sequence:$result[0].sequence,actions:($result[0].results|length),frame_sequence:$result[0].observation.settle.frame_sequence,input_sequence:$result[0].observation.settle.input_sequence,quiet_ms:$result[0].observation.settle.quiet_ms,application:$state[0]}' \
  >"$OUT/action-metrics.json"

cat >"$OUT/proof.md" <<EOF
# Server-only computer-use capture proof

- presentation: detached for capture and direct-action workloads
- SHM: two changing 1440x900 PNG captures in shm-first and shm-second
- DMA-BUF: two changing 1440x900 PNG captures in dmabuf-first and dmabuf-second
- stable window identity and capture metrics: metrics.json and each automation.json
- direct actions: action-response.json, action-state-final.json, and action-observation.png
- cleanup: stop-cleanup-response.json, key-cleanup-response.json, expiry-cleanup-response.json, and target-cleanup-response.json
- human takeover: human-takeover-response.json, human-task.json, human-status.json, and presentation-client.log
- negative live-boundary proofs: stale-lease.err, cross-session.err, and out-of-bounds.err
EOF

echo "computer-use nested capture proof passed"
echo "proof=$OUT/proof.md"
