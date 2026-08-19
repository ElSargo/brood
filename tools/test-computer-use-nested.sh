#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]]; then
  exec dbus-run-session -- "$0" "$@"
fi

for tool in cc grim jq magick pkg-config python3 sway wayland-scanner weston-simple-egl weston-simple-shm wtype; do
  command -v "$tool" > /dev/null 2>&1 || {
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
  PACKAGE="$(nix build "git+file:$ROOT#brood" --no-link --print-out-paths)"
  PACKAGE="$(readlink -f "$PACKAGE")"
  BROOD_BIN="$PACKAGE/bin/brood"
  CONFIG="$PACKAGE/brood.toml"
fi
if [[ -n "${BROOD_COMPUTER_USE_CLIENT_BIN:-}" ]]; then
  CLIENT_BIN="$(readlink -f "$BROOD_COMPUTER_USE_CLIENT_BIN")"
else
  CLIENT_BIN="$(dirname "$BROOD_BIN")/brood-client"
fi
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
    if [[ -n "$pid" ]] && kill -0 "$pid" 2> /dev/null; then
      kill "$pid" 2> /dev/null
      wait "$pid" 2> /dev/null
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
read -r -a KEYBOARD_FLAGS <<< "$(pkg-config --cflags --libs wayland-client xkbcommon)"
cc -std=c11 -Wall -Wextra -Werror \
  tools/virtual-keyboard-key.c \
  -o "$OUT/virtual-keyboard-key" \
  "${KEYBOARD_FLAGS[@]}"

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
  > "$SERVER_LOG" 2>&1 &
SERVER_PID=$!

ctl() {
  env -u brood_session XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    "$BROOD_BIN" ctl \
    --dbus-destination "$DESTINATION" \
    --dbus-address "$DBUS_SESSION_BUS_ADDRESS" "$@"
}

managed_resize_count() {
  local count
  count="$(rg -c 'requested managed window resize ' "$OUT/presentation-client.log" 2> /dev/null || true)"
  printf '%s\n' "${count:-0}"
}

for _ in $(seq 1 300); do
  STATUS="$(ctl status-json 2> /dev/null || true)"
  [[ -n "$STATUS" ]] && break
  kill -0 "$SERVER_PID" 2> /dev/null || exit 1
  sleep 0.05
done
[[ "$(jq -r .presentation <<< "$STATUS")" == "Detached" ]]
WAYLAND_DISPLAY="$(jq -r .wayland_display <<< "$STATUS")"

capture_pair() {
  local kind="$1"
  local first="$OUT/${kind}-first"
  local second="$OUT/${kind}-second"
  local first_hash
  local second_hash=""
  for _ in $(seq 1 300); do
    STATUS="$(ctl status-json)"
    if (($(jq -r .computer_use_capture_attempts <<< "$STATUS") > 0)) &&
      (($(jq -r .total_windows <<< "$STATUS") > 0)); then
      break
    fi
    sleep 0.05
  done
  ctl debug dump "$first" surfaces > /dev/null
  first_hash="$(sha256sum "$first/automation.png" | cut -d' ' -f1)"
  for _ in $(seq 1 100); do
    sleep 0.05
    ctl debug dump "$second" surfaces > /dev/null
    second_hash="$(sha256sum "$second/automation.png" | cut -d' ' -f1)"
    [[ "$second_hash" != "$first_hash" ]] && break
  done
  test -s "$first/automation.png"
  test -s "$second/automation.png"
  jq -e '.viewport_width == 1440 and .viewport_height == 900 and .active_window != null and (.windows | length) >= 1' \
    "$second/automation.json" > /dev/null
  [[ "$(magick identify -format '%wx%h' "$second/automation.png")" == "1440x900" ]]
  [[ "$second_hash" != "$first_hash" ]]
}

env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_DISPLAY" \
  weston-simple-shm > "$OUT/shm-client.log" 2>&1 &
APP_PID=$!
capture_pair shm
kill "$APP_PID"
wait "$APP_PID" 2> /dev/null || true
APP_PID=""

SHM_FRAME_SEQUENCE="$(jq -r .frame_sequence "$OUT/shm-second/automation.json")"
DMA_CAPTURED=false
for attempt in $(seq 1 3); do
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_DISPLAY" \
    weston-simple-egl > "$OUT/dmabuf-client.log" 2>&1 &
  APP_PID=$!
  if capture_pair dmabuf; then
    DMA_CAPTURED=true
    break
  fi
  kill "$APP_PID"
  wait "$APP_PID" 2> /dev/null || true
  APP_PID=""
  rm -rf -- "$OUT/dmabuf-first" "$OUT/dmabuf-second"
done
[[ "$DMA_CAPTURED" == "true" ]]
DMA_FRAME_SEQUENCE="$(jq -r .frame_sequence "$OUT/dmabuf-second/automation.json")"
kill "$APP_PID"
wait "$APP_PID" 2> /dev/null || true
APP_PID=""
STATUS="$(ctl status-json)"
printf '%s\n' "$STATUS" > "$OUT/status.json"

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
  }' > "$OUT/metrics.json"

jq -e '
  .presentation == "Detached"
  and .dmabuf_frame_sequence > .shm_frame_sequence
  and .capture_attempts >= 4
  and .capture_failures == 0
  and .dmabuf_imports_accepted >= 1
' "$OUT/metrics.json" > /dev/null

ACTION_STATE="$OUT/action-state.json"
env GDK_BACKEND=wayland GTK_A11Y=none NO_AT_BRIDGE=1 \
  XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_DISPLAY" \
  "$OUT/computer-use-proof-client" "$ACTION_STATE" > "$OUT/action-client.log" 2>&1 &
APP_PID=$!
for _ in $(seq 1 300); do
  ctl debug dump "$OUT/action-ready" surfaces > /dev/null 2>&1 || true
  if [[ -s "$OUT/action-ready/automation.json" ]] &&
    jq -e 'any(.windows[]; .application_id == "org.brood.ComputerUseProof")' \
      "$OUT/action-ready/automation.json" > /dev/null; then
    break
  fi
  sleep 0.05
done
ACTION_PANE="$(jq -r '.windows[] | select(.application_id == "org.brood.ComputerUseProof") | .pane' "$OUT/action-ready/automation.json")"
LEASE="$(ctl computer-use start phase2-proof 1 "$ACTION_PANE" 30000)"
LEASE_GENERATION="$(jq -r .generation <<< "$LEASE")"
for _ in $(seq 1 300); do
  ctl debug dump "$OUT/action-ready" surfaces > /dev/null
  if jq -e '.windows[] | select(.pane == '"$ACTION_PANE"' and .content_width > 0 and .content_height > 0)' \
    "$OUT/action-ready/automation.json" > /dev/null; then
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
ctl computer-use batch "$ACTION_BATCH" > "$OUT/action-response.json"
for _ in $(seq 1 100); do
  if jq -e '
    .click_count >= 4
    and .double_clicks >= 1
    and .drag_distance >= 150
    and .scroll_events >= 1
    and (.typed_text | contains("Agent42"))
    and .shortcut_count >= 1
  ' "$ACTION_STATE" > /dev/null 2>&1; then
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
' "$ACTION_STATE" > /dev/null
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
' "$OUT/action-response.json" > /dev/null

STALE_BATCH="$(jq -nc --argjson generation "$((LEASE_GENERATION + 1))" --argjson pane "$ACTION_PANE" '{protocol_version:1,task:"phase2-proof",lease_generation:$generation,expected_sequence:2,target_session:1,target_window:$pane,approval_proof:null,actions:[{type:"screenshot"}]}')"
if ctl computer-use batch "$STALE_BATCH" > "$OUT/stale-lease.out" 2> "$OUT/stale-lease.err"; then
  echo "ERR stale lease was accepted" >&2
  exit 1
fi
CROSS_SESSION_BATCH="$(jq -nc --argjson generation "$LEASE_GENERATION" --argjson pane "$ACTION_PANE" '{protocol_version:1,task:"phase2-proof",lease_generation:$generation,expected_sequence:2,target_session:2,target_window:$pane,approval_proof:null,actions:[{type:"screenshot"}]}')"
if ctl computer-use batch "$CROSS_SESSION_BATCH" > "$OUT/cross-session.out" 2> "$OUT/cross-session.err"; then
  echo "ERR cross-session batch was accepted" >&2
  exit 1
fi
OUT_OF_BOUNDS_X="$ACTION_WIDTH"
OUT_OF_BOUNDS_BATCH="$(jq -nc --argjson generation "$LEASE_GENERATION" --argjson pane "$ACTION_PANE" --argjson x "$OUT_OF_BOUNDS_X" '{protocol_version:1,task:"phase2-proof",lease_generation:$generation,expected_sequence:2,target_session:1,target_window:$pane,approval_proof:null,actions:[{type:"click",x:$x,y:10,button:"left"}]}')"
if ctl computer-use batch "$OUT_OF_BOUNDS_BATCH" > "$OUT/out-of-bounds.out" 2> "$OUT/out-of-bounds.err"; then
  echo "ERR out-of-bounds batch was accepted" >&2
  exit 1
fi
ctl computer-use stop phase2-proof > /dev/null

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
STOP_GENERATION="$(jq -r .generation <<< "$STOP_LEASE")"
ctl computer-use batch "$(drag_batch stop-cleanup "$STOP_GENERATION")" \
  > "$OUT/stop-cleanup-response.json" &
BATCH_PID=$!
for _ in $(seq 1 100); do
  STATUS="$(ctl status-json)"
  [[ "$(jq -r .computer_use_held_buttons <<< "$STATUS")" == "1" ]] && break
  sleep 0.01
done
[[ "$(jq -r .computer_use_held_buttons <<< "$STATUS")" == "1" ]]
ctl computer-use stop stop-cleanup > /dev/null
if ! wait "$BATCH_PID"; then
  test -s "$OUT/stop-cleanup-response.json"
fi
BATCH_PID=""
jq -e '.results[-1].status == "cancelled" and .results[-1].failure == "cancelled"' \
  "$OUT/stop-cleanup-response.json" > /dev/null
STATUS="$(ctl status-json)"
jq -e '.computer_use_batch_pending == false and .computer_use_held_buttons == 0 and .computer_use_held_keys == 0' \
  <<< "$STATUS" > /dev/null
for _ in $(seq 1 100); do
  jq -e '.button_down == false' "$ACTION_STATE" > /dev/null 2>&1 && break
  sleep 0.01
done
jq -e '.button_down == false' "$ACTION_STATE" > /dev/null

KEY_LEASE="$(ctl computer-use start key-cleanup 1 "$ACTION_PANE" 30000)"
KEY_GENERATION="$(jq -r .generation <<< "$KEY_LEASE")"
KEY_BATCH="$(jq -nc \
  --argjson generation "$KEY_GENERATION" \
  --argjson pane "$ACTION_PANE" \
  '{protocol_version:1,task:"key-cleanup",lease_generation:$generation,expected_sequence:1,target_session:1,target_window:$pane,approval_proof:null,actions:[{type:"keypress",keys:["CTRL","L"]},{type:"screenshot"}]}')"
ctl computer-use batch "$KEY_BATCH" > "$OUT/key-cleanup-response.json" &
BATCH_PID=$!
for _ in $(seq 1 100); do
  STATUS="$(ctl status-json)"
  (($(jq -r .computer_use_held_keys <<< "$STATUS") > 0)) && break
  sleep 0.005
done
(($(jq -r .computer_use_held_keys <<< "$STATUS") > 0))
ctl computer-use stop key-cleanup > /dev/null
if ! wait "$BATCH_PID"; then
  test -s "$OUT/key-cleanup-response.json"
fi
BATCH_PID=""
jq -e '.results[-1].status == "cancelled" and .results[-1].failure == "cancelled"' \
  "$OUT/key-cleanup-response.json" > /dev/null
for _ in $(seq 1 100); do
  jq -e '.control_down == false' "$ACTION_STATE" > /dev/null 2>&1 && break
  sleep 0.01
done
jq -e '.control_down == false' "$ACTION_STATE" > /dev/null
STATUS="$(ctl status-json)"
jq -e '.computer_use_held_buttons == 0 and .computer_use_held_keys == 0' \
  <<< "$STATUS" > /dev/null

EXPIRY_LEASE="$(ctl computer-use start expiry-cleanup 1 "$ACTION_PANE" 1000)"
EXPIRY_GENERATION="$(jq -r .generation <<< "$EXPIRY_LEASE")"
ctl computer-use batch "$(drag_batch expiry-cleanup "$EXPIRY_GENERATION")" \
  > "$OUT/expiry-cleanup-response.json"
jq -e '.results[-1].status == "failed" and .results[-1].failure == "input_rejected"' \
  "$OUT/expiry-cleanup-response.json" > /dev/null
jq -e '.state == "ready"' < <(ctl computer-use inspect expiry-cleanup) > /dev/null
STATUS="$(ctl status-json)"
jq -e '.computer_use_batch_pending == false and .computer_use_held_buttons == 0 and .computer_use_held_keys == 0' \
  <<< "$STATUS" > /dev/null

TARGET_LEASE="$(ctl computer-use start target-cleanup 1 "$ACTION_PANE" 30000)"
TARGET_GENERATION="$(jq -r .generation <<< "$TARGET_LEASE")"
ctl computer-use batch "$(drag_batch target-cleanup "$TARGET_GENERATION")" \
  > "$OUT/target-cleanup-response.json" &
BATCH_PID=$!
for _ in $(seq 1 100); do
  STATUS="$(ctl status-json)"
  [[ "$(jq -r .computer_use_held_buttons <<< "$STATUS")" == "1" ]] && break
  sleep 0.01
done
[[ "$(jq -r .computer_use_held_buttons <<< "$STATUS")" == "1" ]]
kill "$APP_PID"
wait "$APP_PID" 2> /dev/null || true
APP_PID=""
if ! wait "$BATCH_PID"; then
  test -s "$OUT/target-cleanup-response.json"
fi
BATCH_PID=""
jq -e '.results[-1].status == "failed" and .results[-1].failure == "target_gone"' \
  "$OUT/target-cleanup-response.json" > /dev/null
STATUS="$(ctl status-json)"
jq -e '.computer_use_batch_pending == false and .computer_use_held_buttons == 0 and .computer_use_held_keys == 0' \
  <<< "$STATUS" > /dev/null
ctl computer-use stop target-cleanup > /dev/null

HUMAN_STATE="$OUT/human-state.json"
env GDK_BACKEND=wayland GTK_A11Y=none NO_AT_BRIDGE=1 \
  XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_DISPLAY" \
  "$OUT/computer-use-proof-client" "$HUMAN_STATE" > "$OUT/human-app.log" 2>&1 &
APP_PID=$!
for _ in $(seq 1 300); do
  ctl debug dump "$OUT/human-ready" surfaces > /dev/null 2>&1 || true
  HUMAN_PANE="$(jq -r '.windows[] | select(.application_id == "org.brood.ComputerUseProof") | .pane' "$OUT/human-ready/automation.json" 2> /dev/null || true)"
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
  > "$OUT/sway.conf"
env XDG_RUNTIME_DIR="$PRESENT_RUNTIME_DIR" \
  WLR_BACKENDS=headless WLR_HEADLESS_OUTPUTS=1 WLR_LIBINPUT_NO_DEVICES=1 \
  sway --unsupported-gpu --config "$OUT/sway.conf" > "$OUT/sway.log" 2>&1 &
SWAY_PID=$!
for _ in $(seq 1 300); do
  PRESENT_DISPLAY="$(find "$PRESENT_RUNTIME_DIR" -maxdepth 1 -type s -name 'wayland-*' -printf '%f\n' | head -1)"
  [[ -n "$PRESENT_DISPLAY" ]] && break
  kill -0 "$SWAY_PID" 2> /dev/null || exit 1
  sleep 0.05
done
[[ -n "$PRESENT_DISPLAY" ]]
if [[ -z "$CLIENT_CONFIG" ]]; then
  printf '\n' > "$OUT/client.toml"
  CLIENT_CONFIG="$OUT/client.toml"
fi
env -u brood_session \
  XDG_RUNTIME_DIR="$PRESENT_RUNTIME_DIR" WAYLAND_DISPLAY="$PRESENT_DISPLAY" \
  BROOD_FRAME_TRANSPORT=shared-memory WGPU_BACKEND=vulkan \
  RUST_LOG="${RUST_LOG:-brood_client=info,wgpu_core=warn,wgpu_hal=warn}" \
  "$CLIENT_BIN" --config "$CLIENT_CONFIG" "${CLIENT_INIT_ARGS[@]}" \
  --dbus-destination "$DESTINATION" --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
  > "$OUT/presentation-client.log" 2>&1 &
CLIENT_PID=$!
for _ in $(seq 1 600); do
  STATUS="$(ctl status-json)"
  [[ "$(jq -r .presentation <<< "$STATUS")" == "Attached" ]] && break
  kill -0 "$CLIENT_PID" 2> /dev/null || exit 1
  sleep 0.05
done
[[ "$(jq -r .presentation <<< "$STATUS")" == "Attached" ]]
for _ in $(seq 1 300); do
  AUTOMATION_RESIZES="$(managed_resize_count)"
  ((AUTOMATION_RESIZES > 0)) && break
  kill -0 "$CLIENT_PID" 2> /dev/null || exit 1
  sleep 0.05
done
((AUTOMATION_RESIZES > 0))
# Let the presentation compositor finish its startup cursor placement before
# creating the lease. That synthetic motion is not the human takeover event
# this proof intends to exercise.
sleep 0.5

HUMAN_LEASE="$(ctl computer-use start human-takeover 1 "$HUMAN_PANE" 30000)"
HUMAN_GENERATION="$(jq -r .generation <<< "$HUMAN_LEASE")"
for _ in $(seq 1 300); do
  rg -q 'computer-use presentation task=human-takeover .*locked=true' \
    "$OUT/presentation-client.log" 2> /dev/null && break
  sleep 0.05
done
rg -q 'computer-use presentation task=human-takeover .*locked=true' \
  "$OUT/presentation-client.log"
ctl computer-use batch "$(drag_batch human-takeover "$HUMAN_GENERATION")" \
  > "$OUT/human-takeover-response.json" &
BATCH_PID=$!
for _ in $(seq 1 1000); do
  STATUS="$(ctl status-json)"
  [[ "$(jq -r .computer_use_held_buttons <<< "$STATUS")" == "1" ]] && break
  sleep 0.01
done
[[ "$(jq -r .computer_use_held_buttons <<< "$STATUS")" == "1" ]]
AUTOMATION_RESIZES="$(managed_resize_count)"
sleep 0.1
[[ "$(managed_resize_count)" == "$AUTOMATION_RESIZES" ]]
for _ in $(seq 1 200); do
  env XDG_RUNTIME_DIR="$PRESENT_RUNTIME_DIR" WAYLAND_DISPLAY="$PRESENT_DISPLAY" \
    grim "$OUT/automation-locked.png"
  LOCKED_COLORS="$(magick identify -format '%k' "$OUT/automation-locked.png")"
  ((LOCKED_COLORS > 16)) && break
  sleep 0.05
done
((LOCKED_COLORS > 16))
env XDG_RUNTIME_DIR="$PRESENT_RUNTIME_DIR" WAYLAND_DISPLAY="$PRESENT_DISPLAY" \
  "$OUT/virtual-pointer-click" 400 300 800 600
sleep 0.1
STATUS="$(ctl status-json)"
[[ "$(jq -r .computer_use_held_buttons <<< "$STATUS")" == "1" ]]
jq -e '.state == "running"' < <(ctl computer-use inspect human-takeover) > /dev/null
env XDG_RUNTIME_DIR="$PRESENT_RUNTIME_DIR" WAYLAND_DISPLAY="$PRESENT_DISPLAY" \
  swaymsg -s "$(find "$PRESENT_RUNTIME_DIR" -maxdepth 1 -type s -name 'sway-ipc.*.sock' -print -quit)" \
  '[title="Brood"] focus' > /dev/null
env XDG_RUNTIME_DIR="$PRESENT_RUNTIME_DIR" WAYLAND_DISPLAY="$PRESENT_DISPLAY" \
  "$OUT/virtual-keyboard-key" --none 1
for _ in $(seq 1 300); do
  rg -q 'computer-use interaction unlock requested' "$OUT/presentation-client.log" 2> /dev/null && break
  sleep 0.05
done
rg -q 'computer-use interaction unlock requested' "$OUT/presentation-client.log"
for _ in $(seq 1 300); do
  rg -q 'computer-use presentation task=human-takeover .*locked=false' \
    "$OUT/presentation-client.log" 2> /dev/null && break
  sleep 0.05
done
rg -q 'computer-use presentation task=human-takeover .*locked=false' \
  "$OUT/presentation-client.log"
env XDG_RUNTIME_DIR="$PRESENT_RUNTIME_DIR" WAYLAND_DISPLAY="$PRESENT_DISPLAY" \
  grim "$OUT/automation-unlocked.png"
[[ "$(sha256sum "$OUT/automation-locked.png" | cut -d' ' -f1)" != \
  "$(sha256sum "$OUT/automation-unlocked.png" | cut -d' ' -f1)" ]]
env XDG_RUNTIME_DIR="$PRESENT_RUNTIME_DIR" WAYLAND_DISPLAY="$PRESENT_DISPLAY" \
  "$OUT/virtual-pointer-click" 400 300 800 600
wait "$BATCH_PID"
BATCH_PID=""
jq -e 'all(.results[]; .status == "completed")' \
  "$OUT/human-takeover-response.json" > /dev/null
ctl computer-use inspect human-takeover > "$OUT/human-task.json"
jq -e '.state == "running"' "$OUT/human-task.json" > /dev/null
ctl computer-use stop human-takeover > /dev/null
for _ in $(seq 1 300); do
  TAKEOVER_RESIZES="$(managed_resize_count)"
  ((TAKEOVER_RESIZES > AUTOMATION_RESIZES)) && break
  kill -0 "$CLIENT_PID" 2> /dev/null || exit 1
  sleep 0.05
done
((TAKEOVER_RESIZES > AUTOMATION_RESIZES))
STATUS="$(ctl status-json)"
printf '%s\n' "$STATUS" > "$OUT/human-status.json"
jq -e '
  .presentation == "Attached"
  and .computer_use_lease_generation == null
  and .computer_use_batch_pending == false
  and .computer_use_held_buttons == 0
  and .computer_use_held_keys == 0
' <<< "$STATUS" > /dev/null

if [[ "${BROOD_COMPUTER_USE_SPEEDCRUNCH:-0}" == "1" ]]; then
  for pid in "$CLIENT_PID" "$SWAY_PID" "$APP_PID"; do
    if [[ -n "$pid" ]] && kill -0 "$pid" 2> /dev/null; then
      kill "$pid"
      wait "$pid" 2> /dev/null || true
    fi
  done
  CLIENT_PID=""
  SWAY_PID=""
  APP_PID=""
  command -v speedcrunch > /dev/null 2>&1 || {
    echo "ERR speedcrunch is required for the requested workload" >&2
    exit 2
  }
  MCP_BIN="$(dirname "$BROOD_BIN")/brood-computer-use-mcp"
  [[ -x "$MCP_BIN" ]] || {
    echo "ERR brood-computer-use-mcp is required beside $BROOD_BIN" >&2
    exit 2
  }
  mkdir -p "$OUT/speedcrunch-home/.config" "$OUT/speedcrunch-home/.local/share"
  env HOME="$OUT/speedcrunch-home" \
    XDG_CONFIG_HOME="$OUT/speedcrunch-home/.config" \
    XDG_DATA_HOME="$OUT/speedcrunch-home/.local/share" \
    XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_DISPLAY" \
    QT_QPA_PLATFORM=wayland speedcrunch > "$OUT/speedcrunch.log" 2>&1 &
  APP_PID=$!
  for _ in $(seq 1 300); do
    ctl debug dump "$OUT/speedcrunch-ready" surfaces > /dev/null 2>&1 || true
    SPEEDCRUNCH_PANE="$(jq -r '.windows[] | select(((.application_id | ascii_downcase) | contains("speedcrunch")) and .content_width > 0 and .content_height > 0) | .pane' "$OUT/speedcrunch-ready/automation.json" 2> /dev/null || true)"
    [[ -n "$SPEEDCRUNCH_PANE" ]] && break
    sleep 0.05
  done
  [[ -n "$SPEEDCRUNCH_PANE" ]]
  sleep 0.2
  ctl debug dump "$OUT/speedcrunch-ready" surfaces > /dev/null
  SPEEDCRUNCH_START="$(jq -nc \
    --argjson pane "$SPEEDCRUNCH_PANE" \
    '{protocol_version:1,task:"speedcrunch-proof",principal:"codex-direct-adapter",scope:{session:1,windows:[$pane]},policy:{human_input:"pause",require_approval:["keyboard"]},limits:{max_actions_per_batch:64,max_script_wall_ms:30000,max_script_cpu_ms:5000,max_script_memory_bytes:67108864,max_output_bytes:1048576,max_display_images:16,max_queued_host_calls:64},lease_duration_ms:30000,active_window:$pane}')"
  SPEEDCRUNCH_LEASE="$(ctl computer-use start-request "$SPEEDCRUNCH_START")"
  SPEEDCRUNCH_GENERATION="$(jq -r .generation <<< "$SPEEDCRUNCH_LEASE")"
  ctl computer-use observe speedcrunch-proof 0 0 > "$OUT/speedcrunch-observer-initial.json"
  jq -e '.task.state == "running" and .lease.generation != null' \
    "$OUT/speedcrunch-observer-initial.json" > /dev/null
  ctl computer-use pause speedcrunch-proof > /dev/null
  ctl computer-use observe speedcrunch-proof 0 0 > "$OUT/speedcrunch-observer-paused.json"
  jq -e '.task.state == "paused" and .lease == null' \
    "$OUT/speedcrunch-observer-paused.json" > /dev/null
  SPEEDCRUNCH_LEASE="$(ctl computer-use resume speedcrunch-proof 30000)"
  SPEEDCRUNCH_GENERATION="$(jq -r .generation <<< "$SPEEDCRUNCH_LEASE")"
  ctl computer-use observe speedcrunch-proof 0 0 > "$OUT/speedcrunch-observer-resumed.json"
  jq -e --argjson generation "$SPEEDCRUNCH_GENERATION" \
    '.task.state == "running" and .lease.generation == $generation' \
    "$OUT/speedcrunch-observer-resumed.json" > /dev/null

  codex_call() {
    local call_id="$1"
    local batch="$2"
    local output="$3"
    local call
    call="$(jq -c --arg call_id "$call_id" '. + {call_id:$call_id}' <<< "$batch")"
    ctl computer-use call "$call" > "$output"
    jq -e --arg call_id "$call_id" '.call_id == $call_id' "$output" > /dev/null
  }
  SPEEDCRUNCH_MENU_BATCH="$(jq -nc \
    --argjson generation "$SPEEDCRUNCH_GENERATION" \
    --argjson pane "$SPEEDCRUNCH_PANE" \
    '{protocol_version:1,task:"speedcrunch-proof",lease_generation:$generation,expected_sequence:1,target_session:1,target_window:$pane,approval_proof:null,actions:[{type:"click",x:160,y:15,button:"left"},{type:"wait",duration_ms:150},{type:"screenshot"}]}')"
  codex_call speedcrunch-menu-settings "$SPEEDCRUNCH_MENU_BATCH" \
    "$OUT/speedcrunch-settings-menu.json"
  python3 -c \
    'import json, pathlib, sys; pathlib.Path(sys.argv[2]).write_bytes(bytes(json.load(open(sys.argv[1]))["observation"]["png"]))' \
    "$OUT/speedcrunch-settings-menu.json" "$OUT/speedcrunch-settings-menu.png"
  SPEEDCRUNCH_DISPLAY_BATCH="$(jq -nc \
    --argjson generation "$SPEEDCRUNCH_GENERATION" \
    --argjson pane "$SPEEDCRUNCH_PANE" \
    '{protocol_version:1,task:"speedcrunch-proof",lease_generation:$generation,expected_sequence:2,target_session:1,target_window:$pane,approval_proof:null,actions:[{type:"click",x:210,y:127,button:"left"},{type:"wait",duration_ms:150},{type:"screenshot"}]}')"
  codex_call speedcrunch-menu-display "$SPEEDCRUNCH_DISPLAY_BATCH" \
    "$OUT/speedcrunch-display-menu.json"
  python3 -c \
    'import json, pathlib, sys; pathlib.Path(sys.argv[2]).write_bytes(bytes(json.load(open(sys.argv[1]))["observation"]["png"]))' \
    "$OUT/speedcrunch-display-menu.json" "$OUT/speedcrunch-display-menu.png"
  SPEEDCRUNCH_COLOR_BATCH="$(jq -nc \
    --argjson generation "$SPEEDCRUNCH_GENERATION" \
    --argjson pane "$SPEEDCRUNCH_PANE" \
    '{protocol_version:1,task:"speedcrunch-proof",lease_generation:$generation,expected_sequence:3,target_session:1,target_window:$pane,approval_proof:null,actions:[{type:"click",x:310,y:127,button:"left"},{type:"wait",duration_ms:150},{type:"screenshot"}]}')"
  codex_call speedcrunch-menu-color "$SPEEDCRUNCH_COLOR_BATCH" \
    "$OUT/speedcrunch-color-menu.json"
  python3 -c \
    'import json, pathlib, sys; pathlib.Path(sys.argv[2]).write_bytes(bytes(json.load(open(sys.argv[1]))["observation"]["png"]))' \
    "$OUT/speedcrunch-color-menu.json" "$OUT/speedcrunch-color-menu.png"
  SPEEDCRUNCH_CONFIGURE_BATCH="$(jq -nc \
    --argjson generation "$SPEEDCRUNCH_GENERATION" \
    --argjson pane "$SPEEDCRUNCH_PANE" \
    '{protocol_version:1,task:"speedcrunch-proof",lease_generation:$generation,expected_sequence:4,target_session:1,target_window:$pane,approval_proof:null,actions:[{type:"click",x:450,y:202,button:"left"},{type:"wait",duration_ms:250},{type:"screenshot"}]}')"
  codex_call speedcrunch-menu-configure "$SPEEDCRUNCH_CONFIGURE_BATCH" \
    "$OUT/speedcrunch-configured.json"
  python3 -c \
    'import json, pathlib, sys; pathlib.Path(sys.argv[2]).write_bytes(bytes(json.load(open(sys.argv[1]))["observation"]["png"]))' \
    "$OUT/speedcrunch-configured.json" "$OUT/speedcrunch-configured.png"
  [[ "$(sha256sum "$OUT/speedcrunch-ready/automation.png" | cut -d' ' -f1)" != "$(sha256sum "$OUT/speedcrunch-configured.png" | cut -d' ' -f1)" ]]
  [[ "$(magick "$OUT/speedcrunch-configured.png" -format '%[pixel:p{720,450}]' info:)" == "srgba(253,246,227,1)" ]]

  speedcrunch_calculation() {
    local sequence="$1"
    local name="$2"
    local expression="$3"
    local first=false
    if [[ "$sequence" == "5" ]]; then
      first=true
    fi
    local batch call approval_id
    batch="$(jq -nc \
      --argjson generation "$SPEEDCRUNCH_GENERATION" \
      --argjson pane "$SPEEDCRUNCH_PANE" \
      --argjson sequence "$sequence" \
      --argjson first "$first" \
      --arg expression "$expression" \
      '{protocol_version:1,task:"speedcrunch-proof",lease_generation:$generation,expected_sequence:$sequence,target_session:1,target_window:$pane,approval_proof:null,actions:((if $first then [{type:"click",x:240,y:880,button:"left"}] else [] end) + [{type:"keypress",keys:["CTRL","A"]},{type:"type",text:$expression},{type:"wait",duration_ms:250},{type:"screenshot"}])}')"
    call="$(jq -c --arg call_id "speedcrunch-$name" '. + {call_id:$call_id}' <<< "$batch")"
    if ctl computer-use call "$call" > "$OUT/speedcrunch-$name-unapproved.out" \
      2> "$OUT/speedcrunch-$name-unapproved.err"; then
      echo "ERR keyboard call was accepted without approval" >&2
      exit 1
    fi
    ctl computer-use observe speedcrunch-proof 0 0 \
      > "$OUT/speedcrunch-$name-approval.json"
    approval_id="$(jq -r '.approvals[] | select(.kind == "keyboard") | .id' \
      "$OUT/speedcrunch-$name-approval.json")"
    [[ -n "$approval_id" ]]
    jq -e '.task.state == "waiting_approval"' \
      "$OUT/speedcrunch-$name-approval.json" > /dev/null
    ctl computer-use approve "$approval_id" approve nested-operator > /dev/null
    call="$(jq -c --arg proof "$approval_id" '.approval_proof = $proof' <<< "$call")"
    ctl computer-use call "$call" > "$OUT/speedcrunch-$name.json"
    python3 -c \
      'import json, pathlib, sys; pathlib.Path(sys.argv[2]).write_bytes(bytes(json.load(open(sys.argv[1]))["observation"]["png"]))' \
      "$OUT/speedcrunch-$name.json" "$OUT/speedcrunch-$name.png"
    jq -e --argjson sequence "$sequence" --arg name "$name" '
      .sequence == $sequence
      and .call_id == ("speedcrunch-" + $name)
      and all(.results[]; .status == "completed")
      and .observation.settle.settled
    ' "$OUT/speedcrunch-$name.json" > /dev/null
  }
  SPEEDCRUNCH_DIRECT_STARTED_NS="$(date +%s%N)"
  speedcrunch_calculation 5 power-and-product "sqrt(2)^10 + 17*23"
  speedcrunch_calculation 6 trigonometry "sin(pi/6) + cos(pi/3)"
  speedcrunch_calculation 7 integer-arithmetic "(12345*6789)-987654"
  SPEEDCRUNCH_DIRECT_FINISHED_NS="$(date +%s%N)"
  SPEEDCRUNCH_DIRECT_ROUND_TRIP_MS="$(((SPEEDCRUNCH_DIRECT_FINISHED_NS - SPEEDCRUNCH_DIRECT_STARTED_NS) / 1000000))"
  ctl computer-use observe speedcrunch-proof 0 0 \
    > "$OUT/speedcrunch-observer-detached-final.json"
  jq -e '
    .task.state == "running"
    and .task.next_sequence == 8
    and .lease.generation != null
    and (.events | map(.event.type) | index("batch_committed")) != null
    and ((.observations | length) >= 2)
    and ((.observations | length) <= 8)
  ' "$OUT/speedcrunch-observer-detached-final.json" > /dev/null
  jq -n \
    --argjson pane "$SPEEDCRUNCH_PANE" \
    --argjson generation "$SPEEDCRUNCH_GENERATION" \
    --argjson direct_round_trip_ms "$SPEEDCRUNCH_DIRECT_ROUND_TRIP_MS" \
    --slurpfile power "$OUT/speedcrunch-power-and-product.json" \
    --slurpfile trig "$OUT/speedcrunch-trigonometry.json" \
    --slurpfile integer "$OUT/speedcrunch-integer-arithmetic.json" \
    '{pane:$pane,lease_generation:$generation,viewport:{width:1440,height:900},configuration:{menu:"Settings > Display > Color Scheme",value:"Solarized Light",background_rgba:[253,246,227,255]},direct_round_trip_ms:$direct_round_trip_ms,calculations:[{expression:"sqrt(2)^10 + 17*23",expected:"423",sequence:$power[0].sequence,frame_sequence:$power[0].observation.settle.frame_sequence},{expression:"sin(pi/6) + cos(pi/3)",expected:"1",sequence:$trig[0].sequence,frame_sequence:$trig[0].observation.settle.frame_sequence},{expression:"(12345*6789)-987654",expected:"82822551",sequence:$integer[0].sequence,frame_sequence:$integer[0].observation.settle.frame_sequence}]}' \
    > "$OUT/speedcrunch-metrics.json"

  ctl computer-use pause speedcrunch-proof > /dev/null
  SCRIPT_START="$(jq -nc \
    --argjson pane "$SPEEDCRUNCH_PANE" \
    '{protocol_version:1,task:"speedcrunch-script",principal:"quickjs-wasm",scope:{session:1,windows:[$pane]},policy:{human_input:"pause",require_approval:[]},limits:{max_actions_per_batch:64,max_script_wall_ms:30000,max_script_cpu_ms:5000,max_script_memory_bytes:67108864,max_output_bytes:1048576,max_display_images:16,max_queued_host_calls:64},lease_duration_ms:30000,active_window:$pane}')"
  SCRIPT_LEASE="$(ctl computer-use start-request "$SCRIPT_START")"
  SCRIPT_GENERATION="$(jq -r .generation <<< "$SCRIPT_LEASE")"
  ctl debug dump "$OUT/speedcrunch-script-ready" surfaces > /dev/null
  jq -e --argjson pane "$SPEEDCRUNCH_PANE" '
    .windows[] | select(.pane == $pane and .content_width == 1440 and .content_height == 900)
  ' "$OUT/speedcrunch-script-ready/automation.json" > /dev/null
  SCRIPT_SOURCE='globalThis.scriptRuns = (globalThis.scriptRuns || 0) + 1; computer.perform([computer.click(240, 880), computer.keypress(["CTRL", "A"]), computer.type("sqrt(2)^10 + 17*23"), computer.wait(250), computer.keypress(["CTRL", "A"]), computer.type("sin(pi/6) + cos(pi/3)"), computer.wait(250), computer.keypress(["CTRL", "A"]), computer.type("(12345*6789)-987654"), computer.wait(250)]); display(computer.screenshot()); console.log("script run", scriptRuns); ({run: scriptRuns})'
  SCRIPT_REQUEST="$(jq -nc \
    --argjson generation "$SCRIPT_GENERATION" \
    --arg source "$SCRIPT_SOURCE" \
    '{protocol_version:1,task:"speedcrunch-script",lease_generation:$generation,expected_sequence:1,source:$source,approval_proof:null}')"
  SCRIPT_STARTED_NS="$(date +%s%N)"
  ctl computer-use exec-js "$SCRIPT_REQUEST" > "$OUT/speedcrunch-script-first.json"
  SCRIPT_FINISHED_NS="$(date +%s%N)"
  SCRIPT_ROUND_TRIP_MS="$(((SCRIPT_FINISHED_NS - SCRIPT_STARTED_NS) / 1000000))"
  jq -e '
    .task == "speedcrunch-script"
    and .sequence == 1
    and .console == ["script run 1"]
    and .value_json == "{\"run\":1}"
    and (.displayed_pngs | length) == 1
    and .exception == null
    and .metrics.host_calls == 3
    and .metrics.peak_memory_bytes > 0
  ' "$OUT/speedcrunch-script-first.json" > /dev/null
  python3 -c \
    'import json, pathlib, sys; pathlib.Path(sys.argv[2]).write_bytes(bytes(json.load(open(sys.argv[1]))["displayed_pngs"][0]))' \
    "$OUT/speedcrunch-script-first.json" "$OUT/speedcrunch-script-display.png"
  [[ "$(magick identify -format '%wx%h' "$OUT/speedcrunch-script-display.png")" == "1440x900" ]]

  SCRIPT_SECOND_SOURCE='scriptRuns += 1; console.log("persisted run", scriptRuns); ({run: scriptRuns})'
  SCRIPT_SECOND_REQUEST="$(jq -nc \
    --argjson generation "$SCRIPT_GENERATION" \
    --arg source "$SCRIPT_SECOND_SOURCE" \
    '{protocol_version:1,task:"speedcrunch-script",lease_generation:$generation,expected_sequence:2,source:$source,approval_proof:null}')"
  ctl computer-use exec-js "$SCRIPT_SECOND_REQUEST" > "$OUT/speedcrunch-script-second.json"
  jq -e '
    .sequence == 2
    and .console == ["persisted run 2"]
    and .value_json == "{\"run\":2}"
    and .exception == null
  ' "$OUT/speedcrunch-script-second.json" > /dev/null
  SCRIPT_CAPABILITY_SOURCE='({filesystem:typeof require,network:typeof fetch,process:typeof process,dbus:typeof DBus,hostDesktop:typeof window,windows:computer.windows(),activeWindow:computer.activeWindow()})'
  SCRIPT_CAPABILITY_REQUEST="$(jq -nc \
    --argjson generation "$SCRIPT_GENERATION" \
    --arg source "$SCRIPT_CAPABILITY_SOURCE" \
    '{protocol_version:1,task:"speedcrunch-script",lease_generation:$generation,expected_sequence:3,source:$source,approval_proof:null}')"
  ctl computer-use exec-js "$SCRIPT_CAPABILITY_REQUEST" > "$OUT/speedcrunch-script-capabilities.json"
  jq -e --argjson pane "$SPEEDCRUNCH_PANE" '
    .sequence == 3
    and ((.value_json | fromjson) as $value
      | $value.filesystem == "undefined"
      and $value.network == "undefined"
      and $value.process == "undefined"
      and $value.dbus == "undefined"
      and $value.hostDesktop == "undefined"
      and ($value.windows | length) == 1
      and $value.windows[0].pane == $pane
      and $value.windows[0].session == 1
      and $value.activeWindow.pane == $pane)
    and .exception == null
  ' "$OUT/speedcrunch-script-capabilities.json" > /dev/null

  SCRIPT_ASK_SOURCE='globalThis.askRuns = (globalThis.askRuns || 0) + 1; askUser("Allow the verified SpeedCrunch script to continue?"); computer.perform([computer.wait(50)]); ({askRuns: askRuns})'
  SCRIPT_ASK_REQUEST="$(jq -nc \
    --argjson generation "$SCRIPT_GENERATION" \
    --arg source "$SCRIPT_ASK_SOURCE" \
    '{protocol_version:1,task:"speedcrunch-script",lease_generation:$generation,expected_sequence:4,source:$source,approval_proof:null}')"
  if ctl computer-use exec-js "$SCRIPT_ASK_REQUEST" \
    > "$OUT/speedcrunch-script-ask-pending.out" \
    2> "$OUT/speedcrunch-script-ask-pending.err"; then
    echo "ERR askUser script continued without approval" >&2
    exit 1
  fi
  ctl computer-use observe speedcrunch-script 0 0 \
    > "$OUT/speedcrunch-script-ask-observer.json"
  SCRIPT_ASK_APPROVAL="$(jq -r '
    .approvals[]
    | select(.kind == "script" and .prompt == "Allow the verified SpeedCrunch script to continue?")
    | .id
  ' "$OUT/speedcrunch-script-ask-observer.json")"
  [[ -n "$SCRIPT_ASK_APPROVAL" ]]
  jq -e --arg id "$SCRIPT_ASK_APPROVAL" '
    .task.state == "waiting_approval"
    and any(.approvals[]; .id == $id and .sequence == 4 and .kind == "script")
  ' "$OUT/speedcrunch-script-ask-observer.json" > /dev/null
  ctl computer-use approve "$SCRIPT_ASK_APPROVAL" approve nested-operator > /dev/null
  SCRIPT_ASK_APPROVED_REQUEST="$(jq -c --arg proof "$SCRIPT_ASK_APPROVAL" \
    '.approval_proof = $proof' <<< "$SCRIPT_ASK_REQUEST")"
  ctl computer-use exec-js "$SCRIPT_ASK_APPROVED_REQUEST" \
    > "$OUT/speedcrunch-script-ask-approved.json"
  jq -e '
    .sequence == 4
    and .value_json == "{\"askRuns\":1}"
    and .exception == null
  ' "$OUT/speedcrunch-script-ask-approved.json" > /dev/null
  SCRIPT_ASK_VERIFY_SOURCE='({askRuns: askRuns})'
  SCRIPT_ASK_VERIFY_REQUEST="$(jq -nc \
    --argjson generation "$SCRIPT_GENERATION" \
    --arg source "$SCRIPT_ASK_VERIFY_SOURCE" \
    '{protocol_version:1,task:"speedcrunch-script",lease_generation:$generation,expected_sequence:5,source:$source,approval_proof:null}')"
  ctl computer-use exec-js "$SCRIPT_ASK_VERIFY_REQUEST" \
    > "$OUT/speedcrunch-script-ask-verify.json"
  jq -e '.sequence == 5 and .value_json == "{\"askRuns\":1}"' \
    "$OUT/speedcrunch-script-ask-verify.json" > /dev/null

  SCRIPT_CANCEL_SOURCE='globalThis.cancelledRuns = (globalThis.cancelledRuns || 0) + 1; askUser("Cancel this queued script by denying its lease?"); ({cancelledRuns: cancelledRuns})'
  SCRIPT_CANCEL_REQUEST="$(jq -nc \
    --argjson generation "$SCRIPT_GENERATION" \
    --arg source "$SCRIPT_CANCEL_SOURCE" \
    '{protocol_version:1,task:"speedcrunch-script",lease_generation:$generation,expected_sequence:6,source:$source,approval_proof:null}')"
  if ctl computer-use exec-js "$SCRIPT_CANCEL_REQUEST" \
    > "$OUT/speedcrunch-script-cancel-pending.out" \
    2> "$OUT/speedcrunch-script-cancel-pending.err"; then
    echo "ERR cancellation checkpoint continued without approval" >&2
    exit 1
  fi
  ctl status-json > "$OUT/speedcrunch-script-checkpoint-pending-status.json"
  jq -e '.computer_use_script_checkpoints == 1' \
    "$OUT/speedcrunch-script-checkpoint-pending-status.json" > /dev/null
  ctl computer-use observe speedcrunch-script 0 0 \
    > "$OUT/speedcrunch-script-cancel-observer.json"
  SCRIPT_CANCEL_APPROVAL="$(jq -r '
    .approvals[] | select(.prompt == "Cancel this queued script by denying its lease?") | .id
  ' "$OUT/speedcrunch-script-cancel-observer.json")"
  ctl computer-use approve "$SCRIPT_CANCEL_APPROVAL" deny nested-operator > /dev/null
  ctl status-json > "$OUT/speedcrunch-script-checkpoint-denied-status.json"
  jq -e '
    .computer_use_script_checkpoints == 0
    and .computer_use_lease_generation == null
    and .computer_use_held_keys == 0
    and .computer_use_held_buttons == 0
  ' "$OUT/speedcrunch-script-checkpoint-denied-status.json" > /dev/null

  SCRIPT_GENERATION="$(ctl computer-use resume speedcrunch-script 30000 | jq -r .generation)"
  SCRIPT_STOP_SOURCE='globalThis.stopQueued = (globalThis.stopQueued || 0) + 1; askUser("Stop this queued script with its task?"); ({stopQueued: stopQueued})'
  SCRIPT_STOP_REQUEST="$(jq -nc \
    --argjson generation "$SCRIPT_GENERATION" \
    --arg source "$SCRIPT_STOP_SOURCE" \
    '{protocol_version:1,task:"speedcrunch-script",lease_generation:$generation,expected_sequence:6,source:$source,approval_proof:null}')"
  if ctl computer-use exec-js "$SCRIPT_STOP_REQUEST" \
    > "$OUT/speedcrunch-script-stop-pending.out" \
    2> "$OUT/speedcrunch-script-stop-pending.err"; then
    echo "ERR stop checkpoint continued without approval" >&2
    exit 1
  fi
  ctl status-json > "$OUT/speedcrunch-script-before-stop-status.json"
  jq -e '
    .computer_use_script_isolates == 1
    and .computer_use_script_checkpoints == 1
  ' "$OUT/speedcrunch-script-before-stop-status.json" > /dev/null
  ctl computer-use stop speedcrunch-script > /dev/null
  ctl status-json > "$OUT/speedcrunch-script-after-stop-status.json"
  jq -e '
    .computer_use_script_isolates == 0
    and .computer_use_script_checkpoints == 0
    and .computer_use_held_keys == 0
    and .computer_use_held_buttons == 0
  ' \
    "$OUT/speedcrunch-script-after-stop-status.json" > /dev/null

  SPEEDCRUNCH_LEASE="$(ctl computer-use resume speedcrunch-proof 30000)"
  SPEEDCRUNCH_GENERATION="$(jq -r .generation <<< "$SPEEDCRUNCH_LEASE")"

  PRESENT_DISPLAY=""
  env XDG_RUNTIME_DIR="$PRESENT_RUNTIME_DIR" \
    WLR_BACKENDS=headless WLR_HEADLESS_OUTPUTS=1 WLR_LIBINPUT_NO_DEVICES=1 \
    sway --unsupported-gpu --config "$OUT/sway.conf" \
    > "$OUT/replacement-sway.log" 2>&1 &
  SWAY_PID=$!
  for _ in $(seq 1 300); do
    PRESENT_DISPLAY="$(find "$PRESENT_RUNTIME_DIR" -maxdepth 1 -type s -name 'wayland-*' -printf '%f\n' | head -1)"
    [[ -n "$PRESENT_DISPLAY" ]] && break
    kill -0 "$SWAY_PID" 2> /dev/null || exit 1
    sleep 0.05
  done
  [[ -n "$PRESENT_DISPLAY" ]]
  env -u brood_session \
    XDG_RUNTIME_DIR="$PRESENT_RUNTIME_DIR" WAYLAND_DISPLAY="$PRESENT_DISPLAY" \
    BROOD_FRAME_TRANSPORT=shared-memory WGPU_BACKEND=vulkan \
    RUST_LOG="${RUST_LOG:-brood_client=info,wgpu_core=warn,wgpu_hal=warn}" \
    "$CLIENT_BIN" --config "$CLIENT_CONFIG" "${CLIENT_INIT_ARGS[@]}" \
    --dbus-destination "$DESTINATION" --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
    > "$OUT/replacement-presentation-client.log" 2>&1 &
  CLIENT_PID=$!
  for _ in $(seq 1 600); do
    STATUS="$(ctl status-json)"
    [[ "$(jq -r .presentation <<< "$STATUS")" == "Attached" ]] && break
    kill -0 "$CLIENT_PID" 2> /dev/null || exit 1
    sleep 0.05
  done
  [[ "$(jq -r .presentation <<< "$STATUS")" == "Attached" ]]
  for _ in $(seq 1 300); do
    rg -q 'computer-use observer update .*observations=[2-8] .*"id":"speedcrunch-proof".*"next_sequence":8.*"state":"running"' \
      "$OUT/replacement-presentation-client.log" 2> /dev/null && break
    kill -0 "$CLIENT_PID" 2> /dev/null || exit 1
    sleep 0.05
  done
  rg -q 'computer-use observer update .*observations=[2-8] .*"id":"speedcrunch-proof".*"next_sequence":8.*"state":"running"' \
    "$OUT/replacement-presentation-client.log"
  for _ in $(seq 1 300); do
    rg -q 'computer-use spectator frame task=speedcrunch-proof .*size=1440x900' \
      "$OUT/replacement-presentation-client.log" 2> /dev/null && break
    kill -0 "$CLIENT_PID" 2> /dev/null || exit 1
    sleep 0.05
  done
  rg -q 'computer-use spectator frame task=speedcrunch-proof .*size=1440x900' \
    "$OUT/replacement-presentation-client.log"
  INITIAL_SPECTATOR_OBSERVATION="$(
    rg -o 'computer-use spectator frame task=speedcrunch-proof observation_sequence=[0-9]+' \
      "$OUT/replacement-presentation-client.log" |
      head -1 | sed 's/.*=//'
  )"
  ATTACHED_TASK_STATE="$(ctl computer-use observe speedcrunch-proof 0 0)"
  if [[ "$(jq -r .task.state <<< "$ATTACHED_TASK_STATE")" == "paused" ]]; then
    SPEEDCRUNCH_LEASE="$(ctl computer-use resume speedcrunch-proof 30000)"
    SPEEDCRUNCH_GENERATION="$(jq -r .generation <<< "$SPEEDCRUNCH_LEASE")"
  fi
  LIVE_SPECTATOR_BATCH="$(jq -nc \
    --argjson generation "$SPEEDCRUNCH_GENERATION" \
    --argjson pane "$SPEEDCRUNCH_PANE" \
    '{protocol_version:1,task:"speedcrunch-proof",lease_generation:$generation,expected_sequence:8,target_session:1,target_window:$pane,approval_proof:null,actions:[{type:"click",x:360,y:880,button:"left"},{type:"wait",duration_ms:100},{type:"screenshot"}]}')"
  LIVE_SPECTATOR_CALL="$(jq -c '. + {call_id:"attached-spectator-action"}' \
    <<< "$LIVE_SPECTATOR_BATCH")"
  ctl computer-use call "$LIVE_SPECTATOR_CALL" > "$OUT/automation-spectator-action.json"
  jq -e '
    .call_id == "attached-spectator-action"
    and .sequence == 8
    and .observation.viewport_width == 1440
    and .observation.viewport_height == 900
  ' "$OUT/automation-spectator-action.json" > /dev/null
  for _ in $(seq 1 300); do
    LATEST_SPECTATOR_OBSERVATION="$(
      rg -o 'computer-use spectator frame task=speedcrunch-proof observation_sequence=[0-9]+' \
        "$OUT/replacement-presentation-client.log" 2> /dev/null |
        tail -1 | sed 's/.*=//' || true
    )"
    if [[ -n "$LATEST_SPECTATOR_OBSERVATION" ]] &&
      ((LATEST_SPECTATOR_OBSERVATION > INITIAL_SPECTATOR_OBSERVATION)); then
      break
    fi
    sleep 0.05
  done
  ((LATEST_SPECTATOR_OBSERVATION > INITIAL_SPECTATOR_OBSERVATION))
  for _ in $(seq 1 100); do
    env XDG_RUNTIME_DIR="$PRESENT_RUNTIME_DIR" WAYLAND_DISPLAY="$PRESENT_DISPLAY" \
      grim "$OUT/automation-spectator.png"
    if [[ "$(magick "$OUT/automation-spectator.png" -format '%k' info:)" -gt 16 ]]; then
      break
    fi
    sleep 0.05
  done
  test -s "$OUT/automation-spectator.png"
  [[ "$(magick identify -format '%wx%h' "$OUT/automation-spectator.png")" == "800x600" ]]
  [[ "$(magick "$OUT/automation-spectator.png" -format '%k' info:)" -gt 16 ]]
  read -r SPECTATOR_TOP_RED SPECTATOR_BOTTOM_RED <<< "$(
    magick "$OUT/automation-spectator.png" \
      -format '%[fx:p{400,53}.r] %[fx:p{400,547}.r]' info:
  )"
  awk -v top="$SPECTATOR_TOP_RED" -v bottom="$SPECTATOR_BOTTOM_RED" \
    'BEGIN { exit !(top < bottom) }'
  ctl debug dump "$OUT/spectator-server-viewport" surfaces > /dev/null
  jq -e --argjson pane "$SPEEDCRUNCH_PANE" '
    .viewport_width == 1440
    and .viewport_height == 900
    and any(.windows[]; .pane == $pane and .content_width == 1440 and .content_height == 900)
  ' "$OUT/spectator-server-viewport/automation.json" > /dev/null
  env XDG_RUNTIME_DIR="$PRESENT_RUNTIME_DIR" WAYLAND_DISPLAY="$PRESENT_DISPLAY" \
    "$OUT/virtual-pointer-click" 400 300 800 600
  INITIAL_EVENT_CURSOR="$(jq -r .event_cursor "$OUT/speedcrunch-observer-initial.json")"
  ctl computer-use observe speedcrunch-proof "$INITIAL_EVENT_CURSOR" 0 \
    > "$OUT/speedcrunch-observer-replacement.json"
  jq -e '
    .task.state == "running"
    and .task.next_sequence == 9
    and .lease != null
    and (.events | length) >= 8
    and (.events | map(.event.type) | index("approval_requested")) != null
    and (.events | map(.event.type) | index("approval_decided")) != null
    and ((.observations | length) >= 2)
    and ((.observations | length) <= 8)
  ' "$OUT/speedcrunch-observer-replacement.json" > /dev/null

  env XDG_RUNTIME_DIR="$PRESENT_RUNTIME_DIR" WAYLAND_DISPLAY="$PRESENT_DISPLAY" \
    "$OUT/virtual-keyboard-key" --none 1
  for _ in $(seq 1 300); do
    rg -q 'computer-use presentation task=speedcrunch-proof .*locked=false' \
      "$OUT/replacement-presentation-client.log" 2> /dev/null && break
    sleep 0.05
  done
  rg -q 'computer-use presentation task=speedcrunch-proof .*locked=false' \
    "$OUT/replacement-presentation-client.log"
  env XDG_RUNTIME_DIR="$PRESENT_RUNTIME_DIR" WAYLAND_DISPLAY="$PRESENT_DISPLAY" \
    grim "$OUT/automation-spectator-unlocked.png"
  env XDG_RUNTIME_DIR="$PRESENT_RUNTIME_DIR" WAYLAND_DISPLAY="$PRESENT_DISPLAY" \
    "$OUT/virtual-pointer-click" 400 300 800 600
  jq -e '.state == "running"' < <(ctl computer-use inspect speedcrunch-proof) > /dev/null

  MCP_GENERATION="$SPEEDCRUNCH_GENERATION"
  MCP_ACTION_ARGUMENTS="$(jq -nc \
    --argjson generation "$MCP_GENERATION" \
    --argjson pane "$SPEEDCRUNCH_PANE" \
    '{tool_call_id:"mcp-screenshot",arguments:{protocol_version:1,call_id:"mcp-screenshot",task:"speedcrunch-proof",lease_generation:$generation,expected_sequence:9,target_session:1,target_window:$pane,actions:[{type:"screenshot"}],approval_proof:null}}')"
  {
    jq -nc '{jsonrpc:"2.0",id:1,method:"initialize",params:{}}'
    jq -nc '{jsonrpc:"2.0",id:2,method:"tools/list",params:{}}'
    jq -nc --argjson arguments "$MCP_ACTION_ARGUMENTS" \
      '{jsonrpc:"2.0",id:3,method:"tools/call",params:{name:"computer_action",arguments:$arguments}}'
    jq -nc '{jsonrpc:"2.0",id:4,method:"tools/call",params:{name:"computer_observe",arguments:{task:"speedcrunch-proof",event_cursor:0,observation_sequence:0}}}'
  } | env -u brood_session \
    BROODD_DBUS_DESTINATION="$DESTINATION" BROODD_DBUS_ADDRESS="$DBUS_SESSION_BUS_ADDRESS" \
    "$MCP_BIN" > "$OUT/speedcrunch-mcp.jsonl"
  jq -s -e '
    length == 4
    and .[0].result.serverInfo.name == "brood-computer-use"
    and (.[1].result.tools | map(.name) | index("computer_action")) != null
    and (.[2].result.content[0].text | fromjson | .call_id) == "mcp-screenshot"
    and (.[2].result.content[0].text | fromjson | .sequence) == 9
    and .[2].result.content[1].type == "image"
    and .[2].result.content[1].mimeType == "image/png"
    and (.[2].result.content[1].data | length) > 1000
    and (.[3].result.content[0].text | fromjson | .task.next_sequence) == 10
    and (.[3].result.content | map(.type) | index("image")) != null
  ' "$OUT/speedcrunch-mcp.jsonl" > /dev/null
  ctl computer-use stop speedcrunch-proof > /dev/null

  for pid in "$CLIENT_PID" "$SWAY_PID"; do
    if [[ -n "$pid" ]] && kill -0 "$pid" 2> /dev/null; then
      kill "$pid"
      wait "$pid" 2> /dev/null || true
    fi
  done
  CLIENT_PID=""
  SWAY_PID=""

  script_limit_failure() {
    local name="$1"
    local wall_ms="$2"
    local cpu_ms="$3"
    local memory_bytes="$4"
    local output_bytes="$5"
    local display_images="$6"
    local host_calls="$7"
    local source="$8"
    local pattern="$9"
    local task="speedcrunch-limit-$name"
    local start lease generation request started_ns finished_ns round_trip_ms
    start="$(jq -nc \
      --arg task "$task" \
      --argjson pane "$SPEEDCRUNCH_PANE" \
      --argjson wall_ms "$wall_ms" \
      --argjson cpu_ms "$cpu_ms" \
      --argjson memory_bytes "$memory_bytes" \
      --argjson output_bytes "$output_bytes" \
      --argjson display_images "$display_images" \
      --argjson host_calls "$host_calls" \
      '{protocol_version:1,task:$task,principal:"quickjs-limit-proof",scope:{session:1,windows:[$pane]},policy:{human_input:"pause",require_approval:[]},limits:{max_actions_per_batch:64,max_script_wall_ms:$wall_ms,max_script_cpu_ms:$cpu_ms,max_script_memory_bytes:$memory_bytes,max_output_bytes:$output_bytes,max_display_images:$display_images,max_queued_host_calls:$host_calls},lease_duration_ms:30000,active_window:$pane}')"
    lease="$(ctl computer-use start-request "$start")"
    generation="$(jq -r .generation <<< "$lease")"
    request="$(jq -nc \
      --arg task "$task" \
      --argjson generation "$generation" \
      --arg source "$source" \
      '{protocol_version:1,task:$task,lease_generation:$generation,expected_sequence:1,source:$source,approval_proof:null}')"
    started_ns="$(date +%s%N)"
    if ctl computer-use exec-js "$request" \
      > "$OUT/speedcrunch-limit-$name.out" 2> "$OUT/speedcrunch-limit-$name.err"; then
      echo "ERR $name limit script was accepted" >&2
      exit 1
    fi
    finished_ns="$(date +%s%N)"
    round_trip_ms="$(((finished_ns - started_ns) / 1000000))"
    rg -q "$pattern" "$OUT/speedcrunch-limit-$name.err"
    ((round_trip_ms < 10000))
    jq -n --arg limit "$name" --argjson round_trip_ms "$round_trip_ms" \
      '{limit:$limit,result:"rejected",round_trip_ms:$round_trip_ms}' \
      > "$OUT/speedcrunch-limit-$name.json"
    ctl computer-use stop "$task" > /dev/null
  }

  script_limit_failure cpu 30000 500 67108864 1048576 16 64 \
    'while (true) {}' 'execute QuickJS source: error while executing'
  script_limit_failure wall 5 30000 67108864 1048576 16 64 \
    'while (true) {}' 'execute QuickJS source: error while executing'
  script_limit_failure memory 30000 5000 1048576 1048576 16 64 \
    '1' '(memory|resource limit|instantiate QuickJS)'
  script_limit_failure output 30000 5000 67108864 1024 16 64 \
    'console.log("x".repeat(2048))' 'guest response exceeds 1024 byte limit'
  script_limit_failure image 30000 5000 67108864 1048576 1 64 \
    'display(computer.screenshot()); display(computer.screenshot())' \
    'script displayed more than 1 images'
  script_limit_failure host-call 30000 5000 67108864 1048576 16 1 \
    'computer.screenshot(); computer.screenshot()' \
    'script queued more than 1 host calls'

  jq -n \
    --argjson generation "$SCRIPT_GENERATION" \
    --argjson round_trip_ms "$SCRIPT_ROUND_TRIP_MS" \
    --slurpfile first "$OUT/speedcrunch-script-first.json" \
    --slurpfile second "$OUT/speedcrunch-script-second.json" \
    --slurpfile capabilities "$OUT/speedcrunch-script-capabilities.json" \
    --slurpfile direct "$OUT/speedcrunch-metrics.json" \
    --slurpfile cpu "$OUT/speedcrunch-limit-cpu.json" \
    --slurpfile wall "$OUT/speedcrunch-limit-wall.json" \
    --slurpfile memory "$OUT/speedcrunch-limit-memory.json" \
    --slurpfile output "$OUT/speedcrunch-limit-output.json" \
    --slurpfile image "$OUT/speedcrunch-limit-image.json" \
    --slurpfile host_call "$OUT/speedcrunch-limit-host-call.json" \
    '{lease_generation:$generation,presentation:"Detached",round_trip_ms:$round_trip_ms,direct_round_trip_ms:$direct[0].direct_round_trip_ms,first:$first[0].metrics,persistent_value:$second[0].value_json,script_context:(($capabilities[0].value_json|fromjson)|{windows,activeWindow}),denied_capabilities:(($capabilities[0].value_json|fromjson)|{filesystem,network,process,dbus,hostDesktop}),isolate_destroyed_on_stop:true,limits:[$cpu[0],$wall[0],$memory[0],$output[0],$image[0],$host_call[0]]}' \
    > "$OUT/speedcrunch-script-metrics.json"
fi

jq -n \
  --argjson pane "$ACTION_PANE" \
  --argjson width "$ACTION_WIDTH" \
  --argjson height "$ACTION_HEIGHT" \
  --argjson generation "$LEASE_GENERATION" \
  --slurpfile result "$OUT/action-response.json" \
  --slurpfile state "$OUT/action-state-final.json" \
  '{pane:$pane,width:$width,height:$height,lease_generation:$generation,sequence:$result[0].sequence,actions:($result[0].results|length),frame_sequence:$result[0].observation.settle.frame_sequence,input_sequence:$result[0].observation.settle.input_sequence,quiet_ms:$result[0].observation.settle.quiet_ms,application:$state[0]}' \
  > "$OUT/action-metrics.json"

cat > "$OUT/proof.md" << EOF
# Server-only computer-use capture proof

- presentation: detached for capture and direct-action workloads
- SHM: two changing 1440x900 PNG captures in shm-first and shm-second
- DMA-BUF: two changing 1440x900 PNG captures in dmabuf-first and dmabuf-second
- stable window identity and capture metrics: metrics.json and each automation.json
- direct actions: action-response.json, action-state-final.json, and action-observation.png
- cleanup: stop-cleanup-response.json, key-cleanup-response.json, expiry-cleanup-response.json, and target-cleanup-response.json
- automation interaction: automation-locked.png and automation-unlocked.png prove the lock overlay changes without pane resize; human-takeover-response.json, human-task.json, human-status.json, and presentation-client.log prove locked input suppression, Escape unlock, continued automation, and normal sizing after stop
- negative live-boundary proofs: stale-lease.err, cross-session.err, and out-of-bounds.err
EOF

if [[ "${BROOD_COMPUTER_USE_SPEEDCRUNCH:-0}" == "1" ]]; then
  cat >> "$OUT/proof.md" << EOF
- SpeedCrunch menu configuration: Solarized Light selected through Settings > Display > Color Scheme; speedcrunch-settings-menu.png, speedcrunch-display-menu.png, speedcrunch-color-menu.png, and speedcrunch-configured.png
- SpeedCrunch calculations: sqrt(2)^10 + 17*23 = 423, sin(pi/6) + cos(pi/3) = 1, and (12345*6789)-987654 = 82822551; corresponding speedcrunch-*.png files and speedcrunch-metrics.json
- SpeedCrunch isolation: private HOME, XDG_CONFIG_HOME, and XDG_DATA_HOME under speedcrunch-home
- Phase 3 adapter: Codex-shaped call IDs and action batches in speedcrunch-*.json; server approval checkpoints in speedcrunch-*-approval.json
- Phase 3 observer continuity: speedcrunch-observer-initial.json, speedcrunch-observer-detached-final.json, speedcrunch-observer-replacement.json, and replacement-presentation-client.log prove the replacement client received the running task and lease, ignored locked primary input, and explicitly unlocked without ending automation
- Phase 3 MCP surface: speedcrunch-mcp.jsonl records initialization, tool discovery, a real screenshot action at sequence 9, and the resulting task sequence 10
- Phase 4 QuickJS/WASM: speedcrunch-script-first.json performs ten ordered SpeedCrunch actions and displays a 1440x900 screenshot while detached; speedcrunch-script-second.json proves persistent globals; speedcrunch-script-metrics.json records isolate and round-trip metrics
- Phase 4 askUser: speedcrunch-script-ask-observer.json, speedcrunch-script-ask-approved.json, and speedcrunch-script-ask-verify.json prove an observer-visible approval checkpoint resumed without re-running script side effects
- Phase 4 queued-call teardown: speedcrunch-script-checkpoint-denied-status.json and speedcrunch-script-after-stop-status.json prove denial/lease loss and task stop remove checkpoints with no held input
- Automation presentation: automation-spectator.png and automation-spectator-unlocked.png show locked and unlocked 800x600 per-pane views of the server-owned 1440x900 frame; automation-spectator-action.json and replacement-presentation-client.log prove observation advancement while attached; spectator-server-viewport/automation.json proves presentation did not resize the automation target
- Assembled package: ${PACKAGE:-development binaries}
EOF
fi

echo "computer-use nested capture proof passed"
echo "proof=$OUT/proof.md"
