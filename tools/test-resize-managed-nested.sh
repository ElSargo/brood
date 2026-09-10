#!/usr/bin/env bash
set -Eeuo pipefail

# Real managed-Wayland synchronized-resize lane. The parent resize proof owns
# the outer headless Sway compositor. This lane owns one nested brood-sessiond,
# two controlled XDG SHM clients, and one external presentation client.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

for tool in cargo cc dbus-run-session grim jq magick mkfifo pkg-config python3 readlink rg sha256sum swaymsg wayland-scanner; do
  command -v "$tool" > /dev/null 2>&1 || {
    echo "ERR $tool is required for the managed resize proof" >&2
    exit 2
  }
done

# The real terminal init child can emit binary actor payload diagnostics. Treat
# the client log as text when matching the surrounding structured trace lines.
rg() { command rg --text "$@"; }

: "${BROOD_RESIZE_MANAGED_OUT:?BROOD_RESIZE_MANAGED_OUT must name the artifact directory}"
: "${BROOD_RESIZE_CLIENT:?BROOD_RESIZE_CLIENT must name brood-client}"
: "${BROOD_RESIZE_WORKSPACE_MODULE:?BROOD_RESIZE_WORKSPACE_MODULE must name pointer policy WASM}"
: "${BROOD_RESIZE_PROOF_COMPONENT:?BROOD_RESIZE_PROOF_COMPONENT must name resize proof Component}"
: "${XDG_RUNTIME_DIR:?the parent compositor runtime is required}"
: "${WAYLAND_DISPLAY:?the parent compositor Wayland socket is required}"
: "${SWAYSOCK:?the parent compositor IPC socket is required}"

if [[ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]]; then
  exec dbus-run-session -- "$0" "$@"
fi

OUT="$BROOD_RESIZE_MANAGED_OUT"
PACKAGE="$OUT/package"
FRAMES="$OUT/frames"
CLIENT_LOG="$OUT/client.log"
SERVER_LOG="$OUT/server.log"
FIXTURE_A_LOG="$OUT/managed-a.log"
FIXTURE_B_LOG="$OUT/managed-b.log"
INPUT_LOG="$OUT/input.jsonl"
MANIFEST="$OUT/frame-manifest.jsonl"
EVIDENCE="$OUT/evidence.json"
METRICS="$OUT/metrics.json"
PROOF="$OUT/proof.json"
TOPOLOGY_PROOF="$OUT/managed-topology.json"
ACTOR_FRAME_PROOF="$OUT/actor-frame-startup.json"
ANALYSIS_LOG="$OUT/analysis-errors.log"
CONFIG="$OUT/brood.toml"
DATA_HOME="$OUT/data"
DBUS_DESTINATION="org.brood.ManagedResizeTest.p$$"
FIFO_A="$OUT/managed-a.fifo"
FIFO_B="$OUT/managed-b.fifo"
mkdir -p "$PACKAGE" "$FRAMES" "$DATA_HOME"
: > "$CLIENT_LOG"
: > "$SERVER_LOG"
: > "$FIXTURE_A_LOG"
: > "$FIXTURE_B_LOG"
: > "$INPUT_LOG"
: > "$MANIFEST"
: > "$ANALYSIS_LOG"
mkfifo "$FIFO_A" "$FIFO_B"

CLIENT_PID=""
SERVER_PID=""
POINTER_PID=""
POINTER_READ_FD=""
POINTER_WRITE_FD=""
ORIGINAL_SCALE=""
cleanup() {
  local result=$?
  set +e
  if [[ -n "$POINTER_WRITE_FD" && -n "$POINTER_PID" ]] && kill -0 "$POINTER_PID" 2>/dev/null; then
    printf 'cancel\n' >&"$POINTER_WRITE_FD"
  fi
  terminate_process_bounded "$POINTER_PID"
  terminate_process_bounded "$CLIENT_PID"
  terminate_process_bounded "$SERVER_PID"
  if [[ -n "$ORIGINAL_SCALE" ]]; then
    wayland_env swaymsg output "$OUTPUT_NAME" scale "$ORIGINAL_SCALE" >/dev/null 2>&1 || true
  fi
  if ((result != 0)); then
    {
      echo "resize_managed_proof=failed"
      echo "exit_status=$result"
      echo "artifacts=$OUT"
      echo "analysis_errors:"
      tail -n 80 "$ANALYSIS_LOG" 2>/dev/null || true
      echo "client_trace:"
      rg 'synchronized layout|managed resize|resize-present|window-resize-present' "$CLIENT_LOG" | tail -n 180 || true
      echo "server_trace:"
      rg 'managed resize' "$SERVER_LOG" | tail -n 120 || true
    } > "$OUT/failure-summary.txt"
    tail -n 220 "$CLIENT_LOG" >&2 || true
    tail -n 160 "$SERVER_LOG" >&2 || true
  fi
  return "$result"
}
trap 'echo "ERR command=$BASH_COMMAND line=$LINENO functions=${FUNCNAME[*]} status=$?" >> "$ANALYSIS_LOG"' ERR
trap cleanup EXIT

wayland_env() {
  env XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_DISPLAY" SWAYSOCK="$SWAYSOCK" "$@"
}

componentize_plugin() {
  local package="$1"
  local stem="$2"
  local core="plugins/$package/target/wasm32-unknown-unknown/release/${stem}.wasm"
  local component="$PACKAGE/${stem}.component.wasm"
  cargo build --manifest-path "plugins/$package/Cargo.toml" --target wasm32-unknown-unknown --release --locked
  cargo run --quiet -p brood-wasm --example componentize -- "$core" "$component"
  [[ -s "$component" ]]
}

componentize_session_init() {
  local core="examples/wasm-init-actor/target/wasm32-unknown-unknown/release/brood_wasm_init_actor_example.wasm"
  local component="$PACKAGE/brood_wasm_init_actor_example.component.wasm"
  cargo build --manifest-path examples/wasm-init-actor/Cargo.toml \
    --target wasm32-unknown-unknown --release --locked
  cargo run --quiet -p brood-wasm --example componentize -- "$core" "$component"
  [[ -s "$component" ]]
}

build_fixture() {
  local generated="$OUT/generated"
  local protocols
  local -a wayland_build_flags
  mkdir -p "$generated"
  protocols="$(pkg-config --variable=pkgdatadir wayland-protocols)"
  for spec in \
    stable/xdg-shell/xdg-shell \
    stable/viewporter/viewporter \
    staging/fractional-scale/fractional-scale-v1; do
    local stem="${spec##*/}"
    wayland-scanner client-header "$protocols/$spec.xml" "$generated/$stem-client-protocol.h"
    wayland-scanner private-code "$protocols/$spec.xml" "$generated/$stem-protocol.c"
  done
  read -r -a wayland_build_flags <<< "$(pkg-config --cflags --libs wayland-client)"
  cc -std=c11 -Wall -Wextra -Werror -I "$generated" \
    tools/managed-resize-shm-client.c \
    "$generated/xdg-shell-protocol.c" \
    "$generated/viewporter-protocol.c" \
    "$generated/fractional-scale-v1-protocol.c" \
    -o "$OUT/managed-resize-shm-client" \
    "${wayland_build_flags[@]}"
}

bash "$ROOT/tools/brood-build.sh" --host-only
componentize_plugin session-switcher brood_session_switcher_plugin
componentize_plugin status-bar brood_status_bar_plugin
componentize_plugin inbox brood_inbox_plugin
componentize_plugin picker brood_picker_plugin
componentize_plugin keybindings brood_keybindings_plugin
componentize_plugin terminal-ui brood_terminal_ui_plugin
componentize_plugin terminal-session brood_terminal_session_plugin
componentize_session_init
build_fixture
read -r -a WAYLAND_FLAGS <<< "$(pkg-config --cflags --libs wayland-client xkbcommon)"
cc -std=c11 -Wall -Wextra -Werror tools/virtual-pointer-input.c \
  -o "$OUT/virtual-pointer-input" "${WAYLAND_FLAGS[@]}"

CLIENT_BIN="$(readlink -f "$BROOD_RESIZE_CLIENT")"
WORKSPACE_MODULE="$(readlink -f "$BROOD_RESIZE_WORKSPACE_MODULE")"
RESIZE_PROOF_COMPONENT="$(readlink -f "$BROOD_RESIZE_PROOF_COMPONENT")"
INIT_COMPONENT="$(readlink -f "$PACKAGE/brood_session_switcher_plugin.component.wasm")"
FIXTURE_BIN="$(readlink -f "$OUT/managed-resize-shm-client")"

cat > "$CONFIG" <<EOF
[init]
session = "$PACKAGE/brood_wasm_init_actor_example.component.wasm"

[client.key-policy]
provider = "workspace"
path = "$WORKSPACE_MODULE"
EOF

OUTPUTS="$(wayland_env swaymsg -t get_outputs -r)"
OUTPUT_NAME="$(jq -r 'map(select(.active))[0].name // empty' <<< "$OUTPUTS")"
ORIGINAL_SCALE="$(jq -r 'map(select(.active))[0].scale // 1' <<< "$OUTPUTS")"
wayland_env swaymsg output "$OUTPUT_NAME" scale 1.5 >/dev/null
for _ in $(seq 1 200); do
  OUTPUTS="$(wayland_env swaymsg -t get_outputs -r)"
  OUTPUT_SCALE="$(jq -r 'map(select(.active))[0].scale // 0' <<< "$OUTPUTS")"
  [[ "$OUTPUT_SCALE" == "1.5" ]] && break
  sleep 0.01
done
[[ "$OUTPUT_SCALE" == "1.5" ]]
LOGICAL_WIDTH="$(jq -r 'map(select(.active))[0].rect.width' <<< "$OUTPUTS")"
LOGICAL_HEIGHT="$(jq -r 'map(select(.active))[0].rect.height' <<< "$OUTPUTS")"
MODE_WIDTH="$(jq -r 'map(select(.active))[0].current_mode.width' <<< "$OUTPUTS")"
MODE_HEIGHT="$(jq -r 'map(select(.active))[0].current_mode.height' <<< "$OUTPUTS")"

env -u brood_session -u BROOD_SURFACE_SESSION_ID -u BROOD_PANE_ID \
  XDG_DATA_HOME="$DATA_HOME" \
  BROOD_TRACE_RESIZE=1 \
  RUST_LOG="${BROOD_MANAGED_SERVER_RUST_LOG:-broodd=info,smithay=warn}" \
  target/debug/brood-sessiond --headless --no-default-modules \
  --config "$CONFIG" --dbus-destination "$DBUS_DESTINATION" \
  --dbus-address "$DBUS_SESSION_BUS_ADDRESS" > "$SERVER_LOG" 2>&1 &
SERVER_PID=$!

ctl() {
  env -u brood_session -u BROOD_SURFACE_SESSION_ID -u BROOD_PANE_ID \
    target/debug/brood ctl --dbus-destination "$DBUS_DESTINATION" \
    --dbus-address "$DBUS_SESSION_BUS_ADDRESS" "$@"
}

for _ in $(seq 1 300); do
  ctl status >/dev/null 2>&1 && break
  kill -0 "$SERVER_PID" 2>/dev/null || exit 1
  sleep 0.02
done
ctl status >/dev/null
SPAWN_A="$(ctl spawn "$FIXTURE_BIN" managed-proof-a 1 "$FIFO_A" "$FIXTURE_A_LOG")"
SPAWN_B="$(ctl spawn "$FIXTURE_BIN" managed-proof-b 2 "$FIFO_B" "$FIXTURE_B_LOG")"
BACKEND_PANE_A="$(sed -n 's/.*pane_id=\([0-9][0-9]*\).*/\1/p' <<< "$SPAWN_A")"
BACKEND_PANE_B="$(sed -n 's/.*pane_id=\([0-9][0-9]*\).*/\1/p' <<< "$SPAWN_B")"
[[ -n "$BACKEND_PANE_A" && -n "$BACKEND_PANE_B" && "$BACKEND_PANE_A" != "$BACKEND_PANE_B" ]]

for _ in $(seq 1 500); do
  if rg -q 'event=commit .*kind=exact' "$FIXTURE_A_LOG" \
    && rg -q 'event=commit .*kind=exact' "$FIXTURE_B_LOG"; then
    break
  fi
  sleep 0.01
done
rg -q 'event=commit .*kind=exact' "$FIXTURE_A_LOG"
rg -q 'event=commit .*kind=exact' "$FIXTURE_B_LOG"

env -u brood_session -u BROOD_SURFACE_SESSION_ID -u BROOD_PANE_ID \
  XDG_DATA_HOME="$DATA_HOME" \
  BROOD_KEYBINDINGS_PATH="$OUT/keybindings.json" \
  XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_DISPLAY" \
  BROOD_TRACE_PLUGIN_GRAPHICS=1 BROOD_TRACE_PLUGIN_INPUT=1 BROOD_TRACE_RESIZE=1 \
  BROOD_MANAGED_RESIZE_PROOF_HOLD_FIRST_TAGGED=1 \
  BROOD_FRAME_TRANSPORT=shared-memory WGPU_BACKEND=vulkan BROOD_DMABUF_HAPPY_PATH=0 \
  RUST_LOG="${RUST_LOG:-brood_client=info,smithay=warn,wgpu_core=warn,wgpu_hal=warn}" \
  "$CLIENT_BIN" --config "$CONFIG" --dbus-destination "$DBUS_DESTINATION" \
  --dbus-address "$DBUS_SESSION_BUS_ADDRESS" --init-actor "$INIT_COMPONENT" \
  --wasm-actor "resize-proof-slow=$RESIZE_PROOF_COMPONENT" \
  --wasm-key-policy workspace "$WORKSPACE_MODULE" > "$CLIENT_LOG" 2>&1 &
CLIENT_PID=$!

for _ in $(seq 1 1200); do
  TERMINAL_UI_PANE="$(sed -n 's/.*started presentation WASM actor id=terminal-ui .* pane=\([0-9][0-9]*\) .*/\1/p' "$CLIENT_LOG" | tail -n 1)"
  SESSION_SWITCHER_PANE="$(sed -n 's/.*started presentation WASM actor id=session-switcher .* pane=\([0-9][0-9]*\) .*/\1/p' "$CLIENT_LOG" | tail -n 1)"
  TOPOLOGY_SETUP_JSON="$(python3 tools/check-managed-resize-topology.py --setup "$CLIENT_LOG" 2>/dev/null || true)"
  TOPOLOGY_REST_LINE=""
  TOPOLOGY_QUIESCENT_LINE=""
  if [[ -n "$TOPOLOGY_SETUP_JSON" ]] && jq -e '.verified == true and .phase == "setup"' <<< "$TOPOLOGY_SETUP_JSON" >/dev/null; then
    TOPOLOGY_REST_LINE="$(jq -r '.rest_grant_line' <<< "$TOPOLOGY_SETUP_JSON")"
    TOPOLOGY_QUIESCENT_LINE="$(jq -r '.quiescent_line' <<< "$TOPOLOGY_SETUP_JSON")"
  fi
  ACTOR_FRAME_JSON="$(python3 tools/check-actor-frame-startup.py "$CLIENT_LOG" 2>/dev/null || true)"
  ACTOR_FRAMES_READY=""
  if [[ -n "$ACTOR_FRAME_JSON" ]] && jq -e '.verified == true' <<< "$ACTOR_FRAME_JSON" >/dev/null; then
    ACTOR_FRAMES_READY=1
  fi
  # Setup accepts either an exact stable leased layout or a fully validated
  # native-bootstrap transaction. Interactive synchronization starts only
  # after that explicit setup anchor is quiescent.
  READY_DIVIDER=""
  if [[ -n "$TOPOLOGY_REST_LINE" ]]; then
    READY_DIVIDER="$(jq -r '[.divider_line,.ready_divider] | @tsv' <<< "$TOPOLOGY_SETUP_JSON")"
  fi
  if rg -q 'granted claimed root layout lease actor=session-switcher ' "$CLIENT_LOG" \
    && [[ -n "$TOPOLOGY_REST_LINE" ]] \
    && [[ -n "$TOPOLOGY_QUIESCENT_LINE" ]] \
    && rg -q 'started component plugin actor actor=brood[.]session[.][0-9]+[.]terminal-session ' "$SERVER_LOG" \
    && [[ -n "$SESSION_SWITCHER_PANE" ]] \
    && [[ -n "$TERMINAL_UI_PANE" ]] \
    && [[ -n "$ACTOR_FRAMES_READY" ]] \
    && [[ "$(rg -c 'uploaded window shared-memory image' "$CLIENT_LOG" || true)" -ge 2 ]] \
    && [[ -n "$READY_DIVIDER" ]]; then
    break
  fi
  kill -0 "$CLIENT_PID" 2>/dev/null || exit 1
  sleep 0.02
done
[[ -n "$TOPOLOGY_SETUP_JSON" ]]
jq -e '.verified == true and .phase == "setup" and (.ordinary_routes | length) == 2' \
  <<< "$TOPOLOGY_SETUP_JSON" >/dev/null
[[ "$TOPOLOGY_QUIESCENT_LINE" =~ ^[1-9][0-9]*$ ]]
[[ -n "$ACTOR_FRAME_JSON" ]]
jq -e '.verified == true' <<< "$ACTOR_FRAME_JSON" >/dev/null
printf '%s\n' "$ACTOR_FRAME_JSON" > "$ACTOR_FRAME_PROOF"
rg -q 'started component plugin actor actor=brood[.]session[.][0-9]+[.]terminal-session ' "$SERVER_LOG"
[[ -n "$SESSION_SWITCHER_PANE" ]]
[[ -n "$TERMINAL_UI_PANE" ]]
[[ -n "$READY_DIVIDER" ]]

start_pointer() {
  coproc MANAGED_POINTER { wayland_env "$OUT/virtual-pointer-input" sequence "$LOGICAL_WIDTH" "$LOGICAL_HEIGHT"; }
  POINTER_PID="$MANAGED_POINTER_PID"
  POINTER_READ_FD="${MANAGED_POINTER[0]}"
  POINTER_WRITE_FD="${MANAGED_POINTER[1]}"
  IFS= read -r -t 5 -u "$POINTER_READ_FD" ready
  [[ "$ready" == ready* ]]
}

send_pointer() {
  local action="$1" command="$2" before ack
  before="$(wc -l < "$CLIENT_LOG")"
  printf '%s\n' "$command" >&"$POINTER_WRITE_FD"
  IFS= read -r -t 5 -u "$POINTER_READ_FD" ack
  jq -cn --arg action "$action" --arg command "$command" --arg ack "$ack" \
    --argjson before "$before" '{action:$action,command:$command,ack:$ack,client_line_before:$before}' >> "$INPUT_LOG"
}

wait_pattern() {
  local file="$1" after="$2" pattern="$3" limit="${4:-1000}" result=""
  for _ in $(seq 1 "$limit"); do
    result="$(awk -v start="$after" -v pattern="$pattern" 'NR > start && $0 ~ pattern {print NR "\t" $0; exit}' "$file")"
    if [[ -n "$result" ]]; then
      WAIT_LINE="${result%%$'\t'*}"
      WAIT_TEXT="${result#*$'\t'}"
      return 0
    fi
    kill -0 "$CLIENT_PID" 2>/dev/null || return 1
    sleep 0.005
  done
  echo "ERR timeout file=$file after=$after pattern=$pattern" >&2
  return 1
}

field() {
  sed -n "s/.* $1=\\([^ ]*\\).*/\\1/p" <<< "$2"
}

# A post-grant startup/native transaction is optional. The product-owned setup
# watermark proves native/synchronized/presentation state is fully quiescent;
# the setup validator covers the complete selected setup-anchor-to-watermark
# interval and rejects ambiguity rather than silently draining only one of
# multiple acceptances.
SETUP_ANCHOR_KIND="$(jq -r '.anchor_kind' <<< "$TOPOLOGY_SETUP_JSON")"
STARTUP_SCAN_LINE="$(jq -r '.anchor_start_line' <<< "$TOPOLOGY_SETUP_JSON")"
STARTUP_QUIESCENT_LINE="$(jq -r '.quiescent_line' <<< "$TOPOLOGY_SETUP_JSON")"
STARTUP_ACCEPT_COUNT="$(jq -r '.startup_acceptances | length' <<< "$TOPOLOGY_SETUP_JSON")"
[[ "$SETUP_ANCHOR_KIND" == leased || "$SETUP_ANCHOR_KIND" == native-bootstrap ]]
[[ "$STARTUP_SCAN_LINE" =~ ^[1-9][0-9]*$ ]]
[[ "$STARTUP_ACCEPT_COUNT" =~ ^[01]$ ]]
if [[ "$SETUP_ANCHOR_KIND" == native-bootstrap ]]; then
  [[ "$STARTUP_ACCEPT_COUNT" == 1 ]]
  [[ "$(jq -r '.anchor_accept_line' <<< "$TOPOLOGY_SETUP_JSON")" == \
    "$(jq -r '.startup_acceptances[0].line' <<< "$TOPOLOGY_SETUP_JSON")" ]]
fi
if [[ "$STARTUP_ACCEPT_COUNT" == 1 ]]; then
  read -r STARTUP_ACCEPT_LINE STARTUP_EPOCH < <(
    jq -r '.startup_acceptances[0] | [.line,.epoch] | @tsv' <<< "$TOPOLOGY_SETUP_JSON"
  )
  [[ "$STARTUP_ACCEPT_LINE" =~ ^[1-9][0-9]*$ && "$STARTUP_EPOCH" =~ ^[1-9][0-9]*$ ]]
  ((STARTUP_SCAN_LINE < STARTUP_ACCEPT_LINE && STARTUP_ACCEPT_LINE < STARTUP_QUIESCENT_LINE))
  wait_pattern "$CLIENT_LOG" "$STARTUP_ACCEPT_LINE" "^(completed|degraded) synchronized layout actor=session-switcher .* epoch=$STARTUP_EPOCH " 1600
  STARTUP_TERMINAL_LINE="$WAIT_LINE"
  STARTUP_TERMINAL_TEXT="$WAIT_TEXT"
  if [[ "$STARTUP_TERMINAL_TEXT" == degraded\ synchronized\ layout* ]]; then
  STARTUP_DEGRADED_LINE="$STARTUP_TERMINAL_LINE"
  STARTUP_DEGRADED_EPOCH="$STARTUP_EPOCH"
  wait_pattern "$SERVER_LOG" 0 "accepted and armed ordinary managed recovery .*pane=$BACKEND_PANE_A .*recover_through_epoch=$STARTUP_DEGRADED_EPOCH .*recovery_floor=[0-9]+ .*retired=" 1600
  STARTUP_RECOVERY_ARM_A_LINE="$WAIT_LINE"; STARTUP_RECOVERY_ARM_A_TEXT="$WAIT_TEXT"
  wait_pattern "$SERVER_LOG" 0 "accepted and armed ordinary managed recovery .*pane=$BACKEND_PANE_B .*recover_through_epoch=$STARTUP_DEGRADED_EPOCH .*recovery_floor=[0-9]+ .*retired=" 1600
  STARTUP_RECOVERY_ARM_B_LINE="$WAIT_LINE"; STARTUP_RECOVERY_ARM_B_TEXT="$WAIT_TEXT"
  wait_pattern "$SERVER_LOG" 0 "ordinary managed recovery configure issued .*pane=$BACKEND_PANE_A .*recover_through_epoch=$STARTUP_DEGRADED_EPOCH .*serial=[0-9]+ " 1600
  STARTUP_RECOVERY_CONFIGURE_A_LINE="$WAIT_LINE"; STARTUP_RECOVERY_CONFIGURE_A_TEXT="$WAIT_TEXT"
  wait_pattern "$SERVER_LOG" 0 "ordinary managed recovery configure issued .*pane=$BACKEND_PANE_B .*recover_through_epoch=$STARTUP_DEGRADED_EPOCH .*serial=[0-9]+ " 1600
  STARTUP_RECOVERY_CONFIGURE_B_LINE="$WAIT_LINE"; STARTUP_RECOVERY_CONFIGURE_B_TEXT="$WAIT_TEXT"
  STARTUP_RECOVERY_SERIAL_A="$(field serial "$STARTUP_RECOVERY_CONFIGURE_A_TEXT")"
  STARTUP_RECOVERY_SERIAL_B="$(field serial "$STARTUP_RECOVERY_CONFIGURE_B_TEXT")"
  STARTUP_RECOVERY_ARM_SERIAL_A="$(field serial "$STARTUP_RECOVERY_ARM_A_TEXT")"
  STARTUP_RECOVERY_ARM_SERIAL_B="$(field serial "$STARTUP_RECOVERY_ARM_B_TEXT")"
  [[ "$STARTUP_RECOVERY_SERIAL_A" =~ ^[1-9][0-9]*$ && "$STARTUP_RECOVERY_SERIAL_B" =~ ^[1-9][0-9]*$ ]]
  [[ "$STARTUP_RECOVERY_ARM_SERIAL_A" == "$STARTUP_RECOVERY_SERIAL_A" ]]
  [[ "$STARTUP_RECOVERY_ARM_SERIAL_B" == "$STARTUP_RECOVERY_SERIAL_B" ]]
  ((STARTUP_RECOVERY_CONFIGURE_A_LINE < STARTUP_RECOVERY_ARM_A_LINE))
  ((STARTUP_RECOVERY_CONFIGURE_B_LINE < STARTUP_RECOVERY_ARM_B_LINE))
  wait_pattern "$FIXTURE_A_LOG" 0 "event=configure-ack .*serial=$STARTUP_RECOVERY_SERIAL_A " 1600
  STARTUP_RECOVERY_ACK_A_LINE="$WAIT_LINE"
  wait_pattern "$FIXTURE_A_LOG" "$STARTUP_RECOVERY_ACK_A_LINE" "event=commit .*serial=$STARTUP_RECOVERY_SERIAL_A .*kind=exact " 1600
  STARTUP_RECOVERY_FIXTURE_COMMIT_A_LINE="$WAIT_LINE"
  wait_pattern "$FIXTURE_B_LOG" 0 "event=configure-ack .*serial=$STARTUP_RECOVERY_SERIAL_B " 1600
  STARTUP_RECOVERY_ACK_B_LINE="$WAIT_LINE"
  wait_pattern "$FIXTURE_B_LOG" "$STARTUP_RECOVERY_ACK_B_LINE" "event=commit .*serial=$STARTUP_RECOVERY_SERIAL_B .*kind=exact " 1600
  STARTUP_RECOVERY_FIXTURE_COMMIT_B_LINE="$WAIT_LINE"
  STARTUP_RECOVERY_FLOOR_A="$(field recovery_floor "$STARTUP_RECOVERY_ARM_A_TEXT")"
  STARTUP_RECOVERY_FLOOR_B="$(field recovery_floor "$STARTUP_RECOVERY_ARM_B_TEXT")"
  [[ "$STARTUP_RECOVERY_FLOOR_A" =~ ^[0-9]+$ && "$STARTUP_RECOVERY_FLOOR_B" =~ ^[0-9]+$ ]]
  wait_pattern "$SERVER_LOG" "$STARTUP_RECOVERY_ARM_A_LINE" "ordinary managed recovery commit observed .*pane=$BACKEND_PANE_A .*recovery_floor=$STARTUP_RECOVERY_FLOOR_A .*surface_commit=[0-9]+" 1600
  STARTUP_RECOVERY_COMMIT_A_LINE="$WAIT_LINE"; STARTUP_RECOVERY_COMMIT_A_TEXT="$WAIT_TEXT"
  wait_pattern "$SERVER_LOG" "$STARTUP_RECOVERY_ARM_B_LINE" "ordinary managed recovery commit observed .*pane=$BACKEND_PANE_B .*recovery_floor=$STARTUP_RECOVERY_FLOOR_B .*surface_commit=[0-9]+" 1600
  STARTUP_RECOVERY_COMMIT_B_LINE="$WAIT_LINE"; STARTUP_RECOVERY_COMMIT_B_TEXT="$WAIT_TEXT"
  STARTUP_RECOVERY_SURFACE_A="$(field surface_commit "$STARTUP_RECOVERY_COMMIT_A_TEXT")"
  STARTUP_RECOVERY_SURFACE_B="$(field surface_commit "$STARTUP_RECOVERY_COMMIT_B_TEXT")"
  [[ "$STARTUP_RECOVERY_FLOOR_A" =~ ^[0-9]+$ && "$STARTUP_RECOVERY_SURFACE_A" =~ ^[1-9][0-9]*$ ]]
  [[ "$STARTUP_RECOVERY_FLOOR_B" =~ ^[0-9]+$ && "$STARTUP_RECOVERY_SURFACE_B" =~ ^[1-9][0-9]*$ ]]
  ((STARTUP_RECOVERY_SURFACE_A > STARTUP_RECOVERY_FLOOR_A))
  ((STARTUP_RECOVERY_SURFACE_B > STARTUP_RECOVERY_FLOOR_B))
  STARTUP_RECOVERY_COMMIT_LINE="$STARTUP_RECOVERY_COMMIT_A_LINE"
  if ((STARTUP_RECOVERY_COMMIT_B_LINE > STARTUP_RECOVERY_COMMIT_LINE)); then
    STARTUP_RECOVERY_COMMIT_LINE="$STARTUP_RECOVERY_COMMIT_B_LINE"
  fi
  STARTUP_ROUTE_A="$(jq -r --argjson pane "$BACKEND_PANE_A" \
    '.ordinary_routes[] | select(.backend_pane == $pane) |
      [.backend,.connection_generation,.pane,.backend_pane] | join(":")' \
    <<< "$TOPOLOGY_SETUP_JSON")"
  STARTUP_ROUTE_B="$(jq -r --argjson pane "$BACKEND_PANE_B" \
    '.ordinary_routes[] | select(.backend_pane == $pane) |
      [.backend,.connection_generation,.pane,.backend_pane] | join(":")' \
    <<< "$TOPOLOGY_SETUP_JSON")"
  [[ "$STARTUP_ROUTE_A" =~ ^[1-9][0-9]*:[1-9][0-9]*:[1-9][0-9]*:[1-9][0-9]*$ ]]
  [[ "$STARTUP_ROUTE_B" =~ ^[1-9][0-9]*:[1-9][0-9]*:[1-9][0-9]*:[1-9][0-9]*$ ]]
  STARTUP_RECOVERY_ACCEPTANCE=""
  for _ in $(seq 1 1600); do
    STARTUP_RECOVERY_ACCEPTANCE="$(python3 tools/check-managed-recovery-acceptance.py \
      "$CLIENT_LOG" "$SERVER_LOG" "$STARTUP_RECOVERY_COMMIT_LINE" \
      "$STARTUP_DEGRADED_EPOCH" "$STARTUP_DEGRADED_LINE" \
      "$STARTUP_ROUTE_A" "$STARTUP_ROUTE_B")"
    jq -e '.verified == true' <<< "$STARTUP_RECOVERY_ACCEPTANCE" >/dev/null && break
    kill -0 "$CLIENT_PID" 2>/dev/null || exit 1
    sleep 0.005
  done
  jq -e '.verified == true' <<< "$STARTUP_RECOVERY_ACCEPTANCE" >/dev/null
  STARTUP_ACCEPTED_A_LINE="$(jq -r '.line_a' <<< "$STARTUP_RECOVERY_ACCEPTANCE")"
  STARTUP_ACCEPTED_B_LINE="$(jq -r '.line_b' <<< "$STARTUP_RECOVERY_ACCEPTANCE")"
  STARTUP_RECOVERY_SCENE_LINE="$(jq -r '.scene_line' <<< "$STARTUP_RECOVERY_ACCEPTANCE")"
  STARTUP_RECOVERY_SCENE_SEQUENCE="$(jq -r '.sequence' <<< "$STARTUP_RECOVERY_ACCEPTANCE")"
  [[ "$STARTUP_ACCEPTED_A_LINE" =~ ^[1-9][0-9]*$ && "$STARTUP_ACCEPTED_B_LINE" =~ ^[1-9][0-9]*$ ]]
  printf 'startup managed recovery drained epoch=%s backend_panes=%s,%s recovery_configures=%s:%s,%s:%s recovery_arms=%s,%s fixture_ack_commits=%s:%s,%s:%s recovery_commits=%s,%s scene_line=%s scene_sequence=%s accepted_lines=%s,%s\n' \
    "$STARTUP_DEGRADED_EPOCH" "$BACKEND_PANE_A" "$BACKEND_PANE_B" \
    "$STARTUP_RECOVERY_CONFIGURE_A_LINE" "$STARTUP_RECOVERY_SERIAL_A" \
    "$STARTUP_RECOVERY_CONFIGURE_B_LINE" "$STARTUP_RECOVERY_SERIAL_B" \
    "$STARTUP_RECOVERY_ARM_A_LINE" "$STARTUP_RECOVERY_ARM_B_LINE" \
    "$STARTUP_RECOVERY_ACK_A_LINE" "$STARTUP_RECOVERY_FIXTURE_COMMIT_A_LINE" \
    "$STARTUP_RECOVERY_ACK_B_LINE" "$STARTUP_RECOVERY_FIXTURE_COMMIT_B_LINE" \
    "$STARTUP_RECOVERY_COMMIT_A_LINE" "$STARTUP_RECOVERY_COMMIT_B_LINE" \
    "$STARTUP_RECOVERY_SCENE_LINE" "$STARTUP_RECOVERY_SCENE_SEQUENCE" \
    "$STARTUP_ACCEPTED_A_LINE" "$STARTUP_ACCEPTED_B_LINE"
  else
    printf 'startup managed recovery drained epoch=%s backend_panes=%s,%s\n' \
      "$STARTUP_EPOCH" "$BACKEND_PANE_A" "$BACKEND_PANE_B"
  fi
else
  [[ "$STARTUP_QUIESCENT_LINE" =~ ^[1-9][0-9]*$ ]]
  printf 'startup managed recovery skipped: no post-grant synchronized transaction before pointer\n'
fi

capture_now() {
  local label="$1" trace_line="$2" trace_text="$3"
  local crop_x crop_y crop_width crop_height
  CAPTURE_LABEL="$label"
  CAPTURE_PATH="$FRAMES/$label.png"
  CAPTURE_TRACE_LINE="$trace_line"
  CAPTURE_TRACE_TEXT="$trace_text"
  read -r crop_x crop_y crop_width crop_height < <(
    wayland_env swaymsg -t get_tree -r \
      | python3 tools/analyze-resize-managed.py --sway-content-geometry "$CLIENT_PID"
  )
  [[ "$crop_width" -gt 0 && "$crop_height" -gt 0 ]]
  wayland_env grim -l 0 -g "$crop_x,$crop_y ${crop_width}x${crop_height}" "$CAPTURE_PATH"
  local width height digest
  read -r width height < <(magick "$CAPTURE_PATH" -format '%w %h\n' info:)
  digest="$(sha256sum "$CAPTURE_PATH" | awk '{print $1}')"
  jq -cn --arg label "$CAPTURE_LABEL" --arg file "frames/$CAPTURE_LABEL.png" \
    --arg sha256 "$digest" --arg trace "$CAPTURE_TRACE_TEXT" \
    --argjson trace_line "$CAPTURE_TRACE_LINE" --argjson width "$width" --argjson height "$height" \
    --argjson crop_x "$crop_x" --argjson crop_y "$crop_y" \
    --argjson crop_width "$crop_width" --argjson crop_height "$crop_height" \
    '{label:$label,file:$file,sha256:$sha256,image_size:[$width,$height],capture_geometry:{logical_origin:[$crop_x,$crop_y],logical_size:[$crop_width,$crop_height]},trace:{line:$trace_line,text:$trace}}' >> "$MANIFEST"
}

arm_fixture() { printf '%s\n' "$2" > "$1"; }

INITIAL_LINE="$(wc -l < "$CLIENT_LOG")"
DIVIDER_DEVICE="${READY_DIVIDER#*$'\t'}"
DIVIDER_X="$(python3 - "$DIVIDER_DEVICE" "$OUTPUT_SCALE" "$LOGICAL_WIDTH" <<'PY'
import sys
print(min(max(round(int(sys.argv[1]) / float(sys.argv[2])), 2), int(sys.argv[3]) - 2))
PY
)"
DIVIDER_Y=$((LOGICAL_HEIGHT / 2))

# Pointer lane. The second motion supersedes the first request in the same
# captured interaction. The proof-only transport gate makes the late A scene
# deterministic. B's 120 ms response remains below the protocol deadline.
arm_fixture "$FIFO_A" normal
arm_fixture "$FIFO_B" normal
start_pointer
send_pointer pointer-hover "motion $DIVIDER_X $DIVIDER_Y"
send_pointer pointer-press press
send_pointer pointer-motion-a "motion $((DIVIDER_X - 20)) $DIVIDER_Y"
wait_pattern "$CLIENT_LOG" "$INITIAL_LINE" 'accepted synchronized layout actor=session-switcher .* interaction=[1-9][0-9]*.* native_resize_revision=0 epoch=[1-9][0-9]*.*target_rects=[[][^]]*[]]$'
POINTER_OLD_ACCEPT_LINE="$WAIT_LINE"; POINTER_OLD_ACCEPT_TEXT="$WAIT_TEXT"; POINTER_OLD_EPOCH="$(field epoch "$WAIT_TEXT")"

# Bind topology to this exact pointer acceptance before waiting for any proof
# gate record. The checker may briefly wait for the acceptance's route records,
# but it cannot select a startup/native transaction or a later epoch.
POINTER_TOPOLOGY_AFTER_LINE=$((POINTER_OLD_ACCEPT_LINE - 1))
TOPOLOGY_JSON=""
for _ in $(seq 1 800); do
  TOPOLOGY_JSON="$(python3 tools/check-managed-resize-topology.py \
    --after-line "$POINTER_TOPOLOGY_AFTER_LINE" \
    --accepted-line "$POINTER_OLD_ACCEPT_LINE" \
    --epoch "$POINTER_OLD_EPOCH" \
    "$CLIENT_LOG" 2>/dev/null || true)"
  if [[ -n "$TOPOLOGY_JSON" ]] && jq -e '.verified == true and .phase == "transaction"' <<< "$TOPOLOGY_JSON" >/dev/null; then
    break
  fi
  kill -0 "$CLIENT_PID" 2>/dev/null || exit 1
  sleep 0.005
done
[[ -n "$TOPOLOGY_JSON" ]]
jq -e --argjson after "$POINTER_TOPOLOGY_AFTER_LINE" \
  --argjson accepted "$POINTER_OLD_ACCEPT_LINE" --argjson epoch "$POINTER_OLD_EPOCH" \
  '.verified == true and .phase == "transaction"
    and .after_line == $after and .accepted_line == $accepted and .epoch == $epoch' \
  <<< "$TOPOLOGY_JSON" >/dev/null
printf '%s\n' "$TOPOLOGY_JSON" > "$TOPOLOGY_PROOF"

wait_pattern "$CLIENT_LOG" "$INITIAL_LINE" "proof armed complete tagged managed scene .*epoch=$POINTER_OLD_EPOCH .*backend_panes=" 800
POINTER_GATE_ARM_LINE="$WAIT_LINE"
wait_pattern "$CLIENT_LOG" "$POINTER_GATE_ARM_LINE" "proof held complete tagged managed scene .*epoch=$POINTER_OLD_EPOCH .*tagged_windows=2 " 800
POINTER_HELD_LINE="$WAIT_LINE"
POINTER_HOLD_PROOF="$(
  python3 tools/check-managed-proof-hold.py \
    "$CLIENT_LOG" "$SERVER_LOG" \
    "$INITIAL_LINE" "$POINTER_OLD_ACCEPT_LINE" "$POINTER_GATE_ARM_LINE" "$POINTER_HELD_LINE" \
    "$POINTER_OLD_EPOCH" "$BACKEND_PANE_A" "$BACKEND_PANE_B"
)"
jq -e '.verified == true and (.bare_requests | length) == 0
  and (.terminal_before_hold | length) == 0
  and (.tagged_requests | length) == 2
  and (.snapshots | length) == 2' <<< "$POINTER_HOLD_PROOF" >/dev/null
printf 'managed proof hold verified epoch=%s accept=%s arm=%s held=%s tagged=%s snapshots=%s\n' \
  "$POINTER_OLD_EPOCH" "$POINTER_OLD_ACCEPT_LINE" "$POINTER_GATE_ARM_LINE" \
  "$POINTER_HELD_LINE" \
  "$(jq -c '.tagged_requests' <<< "$POINTER_HOLD_PROOF")" \
  "$(jq -c '.snapshots' <<< "$POINTER_HOLD_PROOF")"

arm_fixture "$FIFO_B" 'delay 120'
send_pointer pointer-motion-b "motion $((DIVIDER_X - 42)) $DIVIDER_Y"
wait_pattern "$CLIENT_LOG" "$POINTER_OLD_ACCEPT_LINE" 'accepted synchronized layout actor=session-switcher .* interaction=[1-9][0-9]*.* native_resize_revision=0 epoch=[1-9][0-9]*.*target_rects=[[][^]]*[]]$'
POINTER_ACCEPT_LINE="$WAIT_LINE"; POINTER_ACCEPT_TEXT="$WAIT_TEXT"; POINTER_EPOCH="$(field epoch "$WAIT_TEXT")"
[[ "$POINTER_EPOCH" != "$POINTER_OLD_EPOCH" ]]

# View-pane identities are assigned by the presentation client. They become
# authoritative only when the first synchronized request admits exact backend
# routes for both managed panes.
read -r VIEW_PANE_A VIEW_PANE_B < <(python3 - "$CLIENT_LOG" "$BACKEND_PANE_A" "$BACKEND_PANE_B" "$POINTER_ACCEPT_LINE" <<'PY'
import re, sys
lines = open(sys.argv[1], encoding="utf-8", errors="replace").read().splitlines()
mapping = {}
for text in lines[:int(sys.argv[4])]:
    match = re.search(r"admitted managed resize expectation .* pane=(\d+) backend_pane=(\d+)", text)
    if match:
        mapping[int(match.group(2))] = int(match.group(1))
print(mapping.get(int(sys.argv[2]), 0), mapping.get(int(sys.argv[3]), 0))
PY
)
[[ "$VIEW_PANE_A" -gt 0 && "$VIEW_PANE_B" -gt 0 && "$VIEW_PANE_A" != "$VIEW_PANE_B" ]]
wait_pattern "$CLIENT_LOG" "$POINTER_ACCEPT_LINE" "proof released complete tagged managed scene held_epoch=$POINTER_OLD_EPOCH active_epoch=$POINTER_EPOCH" 1600
POINTER_GATE_RELEASE_LINE="$WAIT_LINE"
wait_pattern "$CLIENT_LOG" "$POINTER_GATE_RELEASE_LINE" "dropped stale or unissued tagged managed frame .*resize_epoch=Some[()]${POINTER_OLD_EPOCH}[()]" 800
POINTER_DROP_LINE="$WAIT_LINE"
wait_pattern "$CLIENT_LOG" "$WAIT_LINE" "resize-present .* epoch=$POINTER_EPOCH .* phase=promoted .* degraded=false "
POINTER_PROMOTED_LINE="$WAIT_LINE"; POINTER_PROMOTED_TEXT="$WAIT_TEXT"
POINTER_SUCCESSOR_PROOF="$(
  python3 tools/check-managed-proof-hold.py --verify-successor \
    "$CLIENT_LOG" "$SERVER_LOG" \
    "$POINTER_HELD_LINE" "$POINTER_ACCEPT_LINE" "$POINTER_GATE_RELEASE_LINE" \
    "$POINTER_PROMOTED_LINE" "$POINTER_OLD_EPOCH" "$POINTER_EPOCH" \
    "$BACKEND_PANE_A" "$BACKEND_PANE_B" "$VIEW_PANE_A" "$VIEW_PANE_B"
)"
jq -e '.verified == true and (.forbidden_recovery | length) == 0
  and (.tagged_requests | length) == 2
  and (.configures | length) == 2
  and (.drops | length) == 2' <<< "$POINTER_SUCCESSOR_PROOF" >/dev/null
printf 'managed successor verified old_epoch=%s epoch=%s tagged=%s configures=%s drops=%s\n' \
  "$POINTER_OLD_EPOCH" "$POINTER_EPOCH" \
  "$(jq -c '.tagged_requests' <<< "$POINTER_SUCCESSOR_PROOF")" \
  "$(jq -c '.configures' <<< "$POINTER_SUCCESSOR_PROOF")" \
  "$(jq -c '.drops' <<< "$POINTER_SUCCESSOR_PROOF")"
capture_now pointer-promoted "$POINTER_PROMOTED_LINE" "$POINTER_PROMOTED_TEXT"
arm_fixture "$FIFO_B" normal
RELEASE_START="$(wc -l < "$CLIENT_LOG")"
send_pointer pointer-release release
wait_pattern "$CLIENT_LOG" "$RELEASE_START" 'accepted synchronized layout actor=session-switcher .* interaction=[1-9][0-9]*.* native_resize_revision=0 epoch=[1-9][0-9]*.*target_rects=[[][^]]*[]]$'
RELEASE_ACCEPT_LINE="$WAIT_LINE"; RELEASE_ACCEPT_TEXT="$WAIT_TEXT"; RELEASE_EPOCH="$(field epoch "$WAIT_TEXT")"
wait_pattern "$CLIENT_LOG" "$RELEASE_ACCEPT_LINE" "resize-present .* epoch=$RELEASE_EPOCH .* phase=promoted " 1600
RELEASE_PROMOTED_LINE="$WAIT_LINE"; RELEASE_PROMOTED_TEXT="$WAIT_TEXT"
printf 'quit\n' >&"$POINTER_WRITE_FD"
IFS= read -r -t 5 -u "$POINTER_READ_FD" _
wait "$POINTER_PID"
POINTER_PID=""

# Native mixed lane with one ACK/new-configure then old-size buffer. It cannot
# satisfy the barrier. An explicit exact commit supplies the final snapshot.
NATIVE_PREP_LOGICAL_WIDTH=760
NATIVE_PREP_START="$(wc -l < "$CLIENT_LOG")"
arm_fixture "$FIFO_A" normal
arm_fixture "$FIFO_B" normal
wayland_env swaymsg "[pid=$CLIENT_PID] floating enable, resize set $NATIVE_PREP_LOGICAL_WIDTH 500" >/dev/null
wait_pattern "$CLIENT_LOG" "$NATIVE_PREP_START" 'accepted synchronized layout actor=session-switcher .* native_resize_revision=[1-9][0-9]* epoch=[1-9][0-9]*.*target_rects=[[][^]]*[]]$'
NATIVE_PREP_ACCEPT_LINE="$WAIT_LINE"; NATIVE_PREP_ACCEPT_TEXT="$WAIT_TEXT"; NATIVE_PREP_EPOCH="$(field epoch "$WAIT_TEXT")"
wait_pattern "$CLIENT_LOG" "$NATIVE_PREP_ACCEPT_LINE" "window-resize-present .* epoch=$NATIVE_PREP_EPOCH phase=promoted .* managed_expectations=0 managed_staged_resources=0 degraded=false$" 1600
NATIVE_PREP_PROMOTED_LINE="$WAIT_LINE"; NATIVE_PREP_PROMOTED_TEXT="$WAIT_TEXT"
wait_pattern "$CLIENT_LOG" "$NATIVE_PREP_ACCEPT_LINE" "resize-present .* epoch=$NATIVE_PREP_EPOCH .* phase=promoted .* staged_count=0 managed_expectations=0 managed_staged_resources=0 degraded=false " 1600
NATIVE_PREP_RESIZE_LINE="$WAIT_LINE"; NATIVE_PREP_RESIZE_TEXT="$WAIT_TEXT"

NATIVE_LOGICAL_WIDTH=820
read -r NATIVE_CURRENT_COMPOSITION NATIVE_CURRENT_DIVIDER NATIVE_CURRENT_B_WIDTH \
  NATIVE_EXPECTED_COMPOSITION NATIVE_EXPECTED_B_WIDTH < <(
  python3 - "$NATIVE_PREP_RESIZE_TEXT" "$NATIVE_PREP_ACCEPT_TEXT" \
    "$VIEW_PANE_B" "$SESSION_SWITCHER_PANE" \
    "$NATIVE_LOGICAL_WIDTH" "$OUTPUT_SCALE" <<'PY'
import re, sys
promoted, accepted = sys.argv[1:3]
pane = int(sys.argv[3])
shell_pane = int(sys.argv[4])
logical_width, scale = int(sys.argv[5]), float(sys.argv[6])
composition = int(re.search(r" composition_size=(\d+)x", promoted).group(1))
target = re.search(rf"(?:\[|,){pane}:-?\d+:-?\d+:(\d+):\d+", accepted)
shell = re.search(
    rf"(?:\[|,){shell_pane}:(-?\d+):-?\d+:(\d+):\d+", accepted
)
if target is None or shell is None:
    raise SystemExit(1)
divider, sidebar = map(int, shell.groups())
if divider <= 0 or divider + sidebar != composition:
    raise SystemExit(1)
current_b = int(target.group(1))
expected_composition = round(logical_width * scale)
expected_rest = expected_composition - sidebar
expected_b = round(expected_rest * current_b / divider)
print(composition, divider, current_b, expected_composition, expected_b)
PY
)
[[ "$NATIVE_EXPECTED_COMPOSITION" -gt "$NATIVE_CURRENT_COMPOSITION" ]]
[[ "$NATIVE_EXPECTED_B_WIDTH" -gt "$NATIVE_CURRENT_B_WIDTH" ]]
jq -cn --arg action native-old-buffer-growth \
  --argjson current_composition "$NATIVE_CURRENT_COMPOSITION" \
  --argjson current_divider "$NATIVE_CURRENT_DIVIDER" \
  --argjson current_b_width "$NATIVE_CURRENT_B_WIDTH" \
  --argjson expected_composition "$NATIVE_EXPECTED_COMPOSITION" \
  --argjson expected_b_width "$NATIVE_EXPECTED_B_WIDTH" \
  '{action:$action,current_composition:$current_composition,current_divider:$current_divider,current_b_width:$current_b_width,expected_composition:$expected_composition,expected_b_width:$expected_b_width}' >> "$INPUT_LOG"
NATIVE_START="$(wc -l < "$CLIENT_LOG")"
arm_fixture "$FIFO_A" normal
arm_fixture "$FIFO_B" old-once
wayland_env swaymsg "[pid=$CLIENT_PID] floating enable, resize set $NATIVE_LOGICAL_WIDTH 500" >/dev/null
wait_pattern "$CLIENT_LOG" "$NATIVE_START" 'accepted synchronized layout actor=session-switcher .* native_resize_revision=[1-9][0-9]* epoch=[1-9][0-9]*.*target_rects=[[][^]]*[]]$'
NATIVE_ACCEPT_LINE="$WAIT_LINE"; NATIVE_ACCEPT_TEXT="$WAIT_TEXT"; NATIVE_EPOCH="$(field epoch "$WAIT_TEXT")"
wait_pattern "$SERVER_LOG" 0 "managed resize commit is not suitable.*epoch=$NATIVE_EPOCH|epoch=$NATIVE_EPOCH.*managed resize commit is not suitable" 1600
NATIVE_UNSUITABLE_SERVER_LINE="$WAIT_LINE"
arm_fixture "$FIFO_B" exact
wait_pattern "$CLIENT_LOG" "$NATIVE_ACCEPT_LINE" "window-resize-present .* epoch=$NATIVE_EPOCH phase=promoted " 1600
NATIVE_PROMOTED_LINE="$WAIT_LINE"; NATIVE_PROMOTED_TEXT="$WAIT_TEXT"
capture_now native-promoted "$NATIVE_PROMOTED_LINE" "$NATIVE_PROMOTED_TEXT"

# Wrong physical size uses the correct acknowledged configure and viewport,
# but cannot satisfy the exact pixel contract.
WRONG_START="$(wc -l < "$CLIENT_LOG")"
arm_fixture "$FIFO_A" normal
arm_fixture "$FIFO_B" wrong-once
wayland_env swaymsg "[pid=$CLIENT_PID] resize set 800 510" >/dev/null
wait_pattern "$CLIENT_LOG" "$WRONG_START" 'accepted synchronized layout actor=session-switcher .* native_resize_revision=[1-9][0-9]* epoch=[1-9][0-9]*.*target_rects=[[][^]]*[]]$'
WRONG_ACCEPT_LINE="$WAIT_LINE"; WRONG_ACCEPT_TEXT="$WAIT_TEXT"; WRONG_EPOCH="$(field epoch "$WAIT_TEXT")"
wait_pattern "$SERVER_LOG" 0 "managed resize commit is not suitable.*epoch=$WRONG_EPOCH|epoch=$WRONG_EPOCH.*managed resize commit is not suitable" 1600
WRONG_UNSUITABLE_SERVER_LINE="$WAIT_LINE"
arm_fixture "$FIFO_B" exact
wait_pattern "$CLIENT_LOG" "$WRONG_ACCEPT_LINE" "window-resize-present .* epoch=$WRONG_EPOCH phase=promoted " 1600
WRONG_PROMOTED_LINE="$WAIT_LINE"; WRONG_PROMOTED_TEXT="$WAIT_TEXT"

# Timeout lane. B withholds. The terminal path must send an untagged restart,
# retire old server authority, and later accept ordinary content.
TIMEOUT_START="$(wc -l < "$CLIENT_LOG")"
arm_fixture "$FIFO_A" normal
arm_fixture "$FIFO_B" stall
wayland_env swaymsg "[pid=$CLIENT_PID] resize set 820 520" >/dev/null
wait_pattern "$CLIENT_LOG" "$TIMEOUT_START" 'accepted synchronized layout actor=session-switcher .* native_resize_revision=[1-9][0-9]* epoch=[1-9][0-9]*.*target_rects=[[][^]]*[]]$'
TIMEOUT_ACCEPT_LINE="$WAIT_LINE"; TIMEOUT_ACCEPT_TEXT="$WAIT_TEXT"; TIMEOUT_EPOCH="$(field epoch "$WAIT_TEXT")"
wait_pattern "$CLIENT_LOG" "$TIMEOUT_ACCEPT_LINE" "synchronized layout ready .* epoch=$TIMEOUT_EPOCH .*degraded=true" 2200
TIMEOUT_READY_LINE="$WAIT_LINE"; TIMEOUT_READY_TEXT="$WAIT_TEXT"
wait_pattern "$CLIENT_LOG" "$WAIT_LINE" "window-resize-present .* epoch=$TIMEOUT_EPOCH phase=degraded .* degraded=true" 1200
TIMEOUT_PROMOTED_LINE="$WAIT_LINE"; TIMEOUT_PROMOTED_TEXT="$WAIT_TEXT"
capture_now timeout-degraded "$TIMEOUT_PROMOTED_LINE" "$TIMEOUT_PROMOTED_TEXT"
ORDINARY_START="$(wc -l < "$CLIENT_LOG")"
arm_fixture "$FIFO_B" ordinary
wait_pattern "$SERVER_LOG" 0 "accepted and armed ordinary managed recovery .*pane=$BACKEND_PANE_B .*recover_through_epoch=$TIMEOUT_EPOCH .*recovery_floor=[0-9]+" 1200
wait_pattern "$FIXTURE_B_LOG" 0 'event=commit .*kind=ordinary-command' 1200
wait_pattern "$CLIENT_LOG" "$ORDINARY_START" "accepted ordinary managed frame .* pane=${VIEW_PANE_B}[[:space:]]+backend_pane=${BACKEND_PANE_B}[[:space:]]" 1200
TIMEOUT_ORDINARY_LINE="$WAIT_LINE"

# Inventory authority lane. Close B after acceptance; the exact target pane
# must appear in the accepted abort and must not promote.
ABORT_START="$(wc -l < "$CLIENT_LOG")"
arm_fixture "$FIFO_A" normal
arm_fixture "$FIFO_B" stall
wayland_env swaymsg "[pid=$CLIENT_PID] resize set 800 510" >/dev/null
wait_pattern "$CLIENT_LOG" "$ABORT_START" 'accepted synchronized layout actor=session-switcher .* native_resize_revision=[1-9][0-9]* epoch=[1-9][0-9]*.*target_rects=[[][^]]*[]]$'
ABORT_ACCEPT_LINE="$WAIT_LINE"; ABORT_ACCEPT_TEXT="$WAIT_TEXT"; ABORT_EPOCH="$(field epoch "$WAIT_TEXT")"
arm_fixture "$FIFO_B" close
wait_pattern "$CLIENT_LOG" "$ABORT_ACCEPT_LINE" "aborted synchronized layout actor=session-switcher .* epoch=$ABORT_EPOCH .*unavailable_panes=.*$VIEW_PANE_B" 1600
ABORT_LINE="$WAIT_LINE"; ABORT_TEXT="$WAIT_TEXT"
# The abort is terminal for the invalid target. Wait for the actor to consume
# the later inventory and present its replacement before the stored analyzer
# reads the append-only client log.
wait_pattern "$CLIENT_LOG" "$ABORT_LINE" 'queued layout-state revision=[0-9]+ lease=2 actor=workspace$' 1600
ABORT_INVENTORY_LINE="$WAIT_LINE"; ABORT_INVENTORY_REVISION="$(field revision "$WAIT_TEXT")"
wait_pattern "$CLIENT_LOG" "$ABORT_INVENTORY_LINE" "accepted leased actor layout items=[0-9]+ panes=5 revision=$ABORT_INVENTORY_REVISION pending_presented_layout_revision=[0-9]+ pane_rects=\\[[^]]*\\]$" 1600
ABORT_REPLACEMENT_LINE="$WAIT_LINE"; ABORT_PRESENTED_REVISION="$(field pending_presented_layout_revision "$WAIT_TEXT")"
wait_pattern "$CLIENT_LOG" "$ABORT_REPLACEMENT_LINE" "presented committed layout revision=$ABORT_PRESENTED_REVISION$" 1600

python3 - "$OUT" "$CLIENT_LOG" "$SERVER_LOG" "$FIXTURE_A_LOG" "$FIXTURE_B_LOG" \
  "$INPUT_LOG" "$MANIFEST" "$VIEW_PANE_A" "$VIEW_PANE_B" \
  "$OUTPUT_SCALE" "${MODE_WIDTH}x${MODE_HEIGHT}" \
  "$POINTER_OLD_ACCEPT_LINE" "$POINTER_OLD_EPOCH" "$POINTER_HELD_LINE" \
  "$POINTER_ACCEPT_LINE" "$POINTER_EPOCH" "$POINTER_GATE_RELEASE_LINE" "$POINTER_DROP_LINE" \
  "$POINTER_PROMOTED_LINE" "$RELEASE_ACCEPT_LINE" "$RELEASE_EPOCH" "$RELEASE_PROMOTED_LINE" \
  "$NATIVE_ACCEPT_LINE" "$NATIVE_EPOCH" "$NATIVE_UNSUITABLE_SERVER_LINE" "$NATIVE_PROMOTED_LINE" \
  "$WRONG_ACCEPT_LINE" "$WRONG_EPOCH" "$WRONG_UNSUITABLE_SERVER_LINE" "$WRONG_PROMOTED_LINE" \
  "$TIMEOUT_ACCEPT_LINE" "$TIMEOUT_EPOCH" "$TIMEOUT_READY_LINE" "$TIMEOUT_ORDINARY_LINE" \
  "$ABORT_ACCEPT_LINE" "$ABORT_EPOCH" "$ABORT_LINE" > "$EVIDENCE" <<'PY'
import json, pathlib, sys
out, client, server, a, b, inputs, manifest = map(pathlib.Path, sys.argv[1:8])
values = list(map(int, sys.argv[12:]))
print(json.dumps({
  "client_log": str(client), "server_log": str(server),
  "fixtures": [str(a), str(b)], "inputs": str(inputs), "manifest": str(manifest),
  "panes": {"managed_a": int(sys.argv[8]), "managed_b": int(sys.argv[9])},
  "scale": float(sys.argv[10]), "mode": sys.argv[11],
  "transactions": {
    "pointer_old": {"accepted": values[0], "epoch": values[1], "held": values[2]},
    "pointer": {"accepted": values[3], "epoch": values[4], "gate_release": values[5], "old_drop": values[6], "promoted": values[7]},
    "release": {"accepted": values[8], "epoch": values[9], "promoted": values[10]},
    "native": {"accepted": values[11], "epoch": values[12], "unsuitable_server": values[13], "promoted": values[14]},
    "wrong": {"accepted": values[15], "epoch": values[16], "unsuitable_server": values[17], "promoted": values[18]},
    "timeout": {"accepted": values[19], "epoch": values[20], "ready": values[21], "ordinary": values[22]},
    "abort": {"accepted": values[23], "epoch": values[24], "terminal": values[25]},
  },
}, sort_keys=True))
PY

python3 tools/analyze-resize-managed.py "$OUT" "$EVIDENCE" > "$METRICS" 2>> "$ANALYSIS_LOG"
cp "$METRICS" "$PROOF"
jq -e '.verified == true and ([.assertions[]] | all(. == true))' "$PROOF" >/dev/null
echo "managed_resize_proof=passed"
echo "artifacts=$OUT"
