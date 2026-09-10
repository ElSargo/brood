#!/usr/bin/env bash
set -euo pipefail

# Three-level synchronized-layout proof. The parent resize proof owns the
# headless compositor. This lane uses the production session-switcher as root
# and two proof Component actors as the middle and leaf lease holders.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

for tool in cargo cc grim jq magick pkg-config python3 rg swaymsg; do
  command -v "$tool" > /dev/null 2>&1 || {
    echo "ERR $tool is required for the resize region-tree proof" >&2
    exit 2
  }
done

: "${BROOD_RESIZE_REGION_OUT:?BROOD_RESIZE_REGION_OUT must name the artifact directory}"
: "${BROOD_RESIZE_CLIENT:?BROOD_RESIZE_CLIENT must name brood-client}"
: "${BROOD_RESIZE_WORKSPACE_MODULE:?BROOD_RESIZE_WORKSPACE_MODULE must name pointer policy WASM}"
: "${BROOD_RESIZE_PROOF_COMPONENT:?BROOD_RESIZE_PROOF_COMPONENT must name resize-proof Component}"
: "${XDG_RUNTIME_DIR:?the parent compositor runtime is required}"
: "${WAYLAND_DISPLAY:?the parent compositor Wayland socket is required}"
: "${SWAYSOCK:?the parent compositor IPC socket is required}"

OUT="$BROOD_RESIZE_REGION_OUT"
PACKAGE="$OUT/package"
FRAMES="$OUT/frames"
CLIENT_LOG="$OUT/client.log"
INPUT_LOG="$OUT/input.jsonl"
MANIFEST="$OUT/frame-manifest.jsonl"
EVIDENCE="$OUT/evidence.json"
METRICS="$OUT/metrics.json"
PROOF="$OUT/proof.json"
ANALYSIS_LOG="$OUT/analysis-errors.log"
mkdir -p "$PACKAGE" "$FRAMES"
: > "$CLIENT_LOG"
: > "$INPUT_LOG"
: > "$MANIFEST"
: > "$ANALYSIS_LOG"

CLIENT_PID=""
POINTER_PID=""
POINTER_READ_FD=""
POINTER_WRITE_FD=""
cleanup() {
  local result=$?
  set +e
  if [[ -n "$POINTER_WRITE_FD" && -n "$POINTER_PID" ]] \
    && kill -0 "$POINTER_PID" 2> /dev/null; then
    printf 'quit\n' >&"$POINTER_WRITE_FD"
  fi
  terminate_process_bounded "$POINTER_PID"
  terminate_process_bounded "$CLIENT_PID" 80
  if ((result != 0)); then
    {
      echo "resize_region_tree_proof=failed"
      echo "exit_status=$result"
      echo "artifacts=$OUT"
      echo "analysis_errors:"
      tail -n 80 "$ANALYSIS_LOG" 2> /dev/null || true
      echo "region_trace:"
      rg 'layout-region |synchronized layout actor=session-switcher |pointer-policy capture released|resize-proof-region-' \
        "$CLIENT_LOG" | tail -n 160 || true
    } > "$OUT/failure-summary.txt"
    tail -n 260 "$CLIENT_LOG" >&2 || true
  fi
  return "$result"
}
trap 'echo "ERR command=$BASH_COMMAND line=$LINENO" >&2' ERR
trap cleanup EXIT

wayland_env() {
  env XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_DISPLAY" \
    SWAYSOCK="$SWAYSOCK" "$@"
}

componentize_plugin() {
  local package="$1"
  local stem="$2"
  local core="plugins/$package/target/wasm32-unknown-unknown/release/${stem}.wasm"
  local component="$PACKAGE/${stem}.component.wasm"
  cargo build --manifest-path "plugins/$package/Cargo.toml" \
    --target wasm32-unknown-unknown --release --locked
  cargo run --quiet -p brood-wasm --example componentize -- "$core" "$component"
  [[ -s "$component" ]]
}

componentize_plugin session-switcher brood_session_switcher_plugin
componentize_plugin theme-switcher brood_theme_switcher_plugin
componentize_plugin status-bar brood_status_bar_plugin
componentize_plugin inbox brood_inbox_plugin
componentize_plugin picker brood_picker_plugin
componentize_plugin keybindings brood_keybindings_plugin

read -r -a WAYLAND_FLAGS <<< "$(pkg-config --cflags --libs wayland-client xkbcommon)"
cc -std=c11 -Wall -Wextra -Werror tools/virtual-pointer-input.c \
  -o "$OUT/virtual-pointer-input" "${WAYLAND_FLAGS[@]}"

CLIENT_BIN="$(readlink -f "$BROOD_RESIZE_CLIENT")"
WORKSPACE_MODULE="$(readlink -f "$BROOD_RESIZE_WORKSPACE_MODULE")"
RESIZE_PROOF_COMPONENT="$(readlink -f "$BROOD_RESIZE_PROOF_COMPONENT")"
INIT_COMPONENT="$(readlink -f "$PACKAGE/brood_session_switcher_plugin.component.wasm")"
[[ -x "$CLIENT_BIN" && -s "$WORKSPACE_MODULE" && -s "$RESIZE_PROOF_COMPONENT" \
  && -s "$INIT_COMPONENT" ]]

OUTPUTS="$(wayland_env swaymsg -t get_outputs -r)"
OUTPUT_NAME="$(jq -r 'map(select(.active))[0].name // empty' <<< "$OUTPUTS")"
OUTPUT_SCALE="$(jq -r 'map(select(.active))[0].scale // 1' <<< "$OUTPUTS")"
LOGICAL_WIDTH="$(jq -r 'map(select(.active))[0].rect.width // 0' <<< "$OUTPUTS")"
LOGICAL_HEIGHT="$(jq -r 'map(select(.active))[0].rect.height // 0' <<< "$OUTPUTS")"
[[ -n "$OUTPUT_NAME" && "$LOGICAL_WIDTH" -gt 0 && "$LOGICAL_HEIGHT" -gt 0 ]]

env -u brood_session -u BROOD_SURFACE_SESSION_ID -u BROOD_PANE_ID \
  XDG_DATA_HOME="$OUT/data" \
  BROOD_KEYBINDINGS_PATH="$OUT/keybindings.json" \
  XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_DISPLAY" \
  BROOD_TRACE_PLUGIN_GRAPHICS=1 \
  BROOD_TRACE_PLUGIN_INPUT=1 \
  BROOD_TRACE_RESIZE=1 \
  WGPU_BACKEND=vulkan \
  BROOD_DMABUF_HAPPY_PATH=1 \
  RUST_LOG="${RUST_LOG:-brood_client=info,smithay=warn,wgpu_core=warn,wgpu_hal=warn}" \
  "$CLIENT_BIN" --no-server --init-actor "$INIT_COMPONENT" \
  --wasm-actor "workspace=$RESIZE_PROOF_COMPONENT" \
  --wasm-actor "resize-proof-leaf-layout=$RESIZE_PROOF_COMPONENT" \
  --wasm-key-policy workspace "$WORKSPACE_MODULE" \
  > "$CLIENT_LOG" 2>&1 &
CLIENT_PID=$!

wait_for_pattern() {
  local after="$1"
  local pattern="$2"
  local result=""
  for _ in $(seq 1 1200); do
    result="$(awk -v start="$after" -v pattern="$pattern" '
      NR > start && $0 ~ pattern { print NR "\t" $0; exit }
    ' "$CLIENT_LOG")"
    if [[ -n "$result" ]]; then
      WAIT_LINE="${result%%$'\t'*}"
      WAIT_TEXT="${result#*$'\t'}"
      return 0
    fi
    kill -0 "$CLIENT_PID" 2> /dev/null || return 1
    sleep 0.01
  done
  echo "ERR timed out waiting for pattern=$pattern after=$after" >&2
  return 1
}

ROOT_GRANT_LINE=0
MIDDLE_GRANT_LINE=0
LEAF_GRANT_LINE=0
SETTLED_LINE=0
PAINT_LINE=0
SETTLED_REVISION=0
SETTLED_DIVIDER=0
SETTLED_WIDTH=0
SETTLED_HEIGHT=0
for _ in $(seq 1 1200); do
  REGION_READY="$(python3 - "$CLIENT_LOG" <<'PY'
import re, sys
lines = open(sys.argv[1], encoding="utf-8", errors="replace").read().splitlines()

def latest(pattern):
    found = [index for index, text in enumerate(lines, 1) if re.search(pattern, text)]
    return found[-1] if found else 0

root = latest(r"granted claimed root layout lease actor=session-switcher ")
middle_grant = latest(r"granted rest layout lease .* actor=workspace ")
leaf_grant = latest(r"granted rest layout lease .* actor=resize-proof-leaf-layout ")
if not (0 < root < middle_grant < leaf_grant):
    raise SystemExit

commits = []
for index, text in enumerate(lines, 1):
    match = re.search(r"presented committed layout revision=(\d+)", text)
    if index > leaf_grant and match:
        commits.append((index, int(match.group(1))))
for commit_line, revision in reversed(commits):
    middle = leaf = 0
    revision_field = f'"presented_layout_revision":{revision}'
    for index, text in enumerate(lines[commit_line:], commit_line + 1):
        if revision_field not in text or '"kind":"resize-proof-layout-state"' not in text:
            continue
        if '"mode":"middle-layout"' in text:
            middle = index
        elif '"mode":"leaf-layout"' in text:
            leaf = index
    after_states = max(middle, leaf)
    if not (middle > commit_line and leaf > commit_line):
        continue
    for settled, text in enumerate(lines[after_states:], after_states + 1):
        if not (
            "resize-present " in text
            and " interaction=0 " in text
            and " epoch=0 " in text
            and " phase=settled " in text
            and " staged_count=0 " in text
            and " managed_expectations=0 " in text
            and " managed_staged_resources=0 " in text
            and " degraded=false " in text
        ):
            continue
        divider = re.search(r" divider_x=(\d+)", text)
        size = re.search(r" composition_size=(\d+)x(\d+)", text)
        if divider is None or size is None:
            continue
        paint = settled + 1
        if paint > len(lines) or not re.search(
            r"resize-paint-order ordinary_panes=4 dividers=1 ", lines[paint - 1]
        ):
            continue
        print(
            root, middle_grant, leaf_grant, revision, middle, leaf, settled, paint,
            divider.group(1), size.group(1), size.group(2)
        )
        raise SystemExit
PY
)"
  if [[ -n "$REGION_READY" ]]; then
    read -r ROOT_GRANT_LINE MIDDLE_GRANT_LINE LEAF_GRANT_LINE SETTLED_REVISION \
      MIDDLE_STATE_LINE LEAF_STATE_LINE SETTLED_LINE PAINT_LINE SETTLED_DIVIDER \
      SETTLED_WIDTH SETTLED_HEIGHT <<< "$REGION_READY"
    break
  fi
  kill -0 "$CLIENT_PID" 2> /dev/null || exit 1
  sleep 0.025
done
[[ "$SETTLED_REVISION" -gt 0 && "$SETTLED_LINE" -gt "$MIDDLE_STATE_LINE" \
  && "$SETTLED_LINE" -gt "$LEAF_STATE_LINE" && "$PAINT_LINE" -eq $((SETTLED_LINE + 1)) ]]

MIDDLE_PANE="$(sed -n 's/.*started presentation WASM actor id=workspace .* pane=\([0-9][0-9]*\) .*/\1/p' \
  "$CLIENT_LOG" | tail -1)"
LEAF_PANE="$(sed -n 's/.*started presentation WASM actor id=resize-proof-leaf-layout .* pane=\([0-9][0-9]*\) .*/\1/p' \
  "$CLIENT_LOG" | tail -1)"
ROOT_PANE="$(sed -n 's/.*started presentation WASM actor id=session-switcher .* pane=\([0-9][0-9]*\) .*/\1/p' \
  "$CLIENT_LOG" | tail -1)"
[[ -n "$MIDDLE_PANE" && -n "$LEAF_PANE" && -n "$ROOT_PANE" ]]

start_pointer() {
  coproc REGION_POINTER {
    wayland_env "$OUT/virtual-pointer-input" sequence "$LOGICAL_WIDTH" "$LOGICAL_HEIGHT"
  }
  POINTER_PID="$REGION_POINTER_PID"
  POINTER_READ_FD="${REGION_POINTER[0]}"
  POINTER_WRITE_FD="${REGION_POINTER[1]}"
  IFS= read -r -t 5 -u "$POINTER_READ_FD" POINTER_READY
  [[ "$POINTER_READY" == ready* ]]
}

send_pointer() {
  local action="$1"
  local command="$2"
  local before ack
  before="$(wc -l < "$CLIENT_LOG")"
  printf '%s\n' "$command" >&"$POINTER_WRITE_FD"
  IFS= read -r -t 5 -u "$POINTER_READ_FD" ack
  [[ "$ack" == ack*"action=${command%% *}"* ]]
  jq -cn --arg action "$action" --arg command "$command" --arg ack "$ack" \
    --argjson before "$before" \
    '{action:$action,command:$command,ack:$ack,log_line_before:$before}' >> "$INPUT_LOG"
  INPUT_BEFORE="$before"
}

arm_region_proof() {
  local action_prefix="$1"
  local press_cursor
  send_pointer "${action_prefix}-hover" "motion $ARM_X $ARM_Y"
  send_pointer "${action_prefix}-press" press
  press_cursor="$INPUT_BEFORE"
  wait_for_pattern "$press_cursor" 'resize-proof-region-armed'
  ARMED_LINE="$WAIT_LINE"
  send_pointer "${action_prefix}-release" release
}

stop_pointer() {
  printf 'quit\n' >&"$POINTER_WRITE_FD"
  local completion
  IFS= read -r -t 5 -u "$POINTER_READ_FD" completion
  [[ "$completion" == done*"reason=quit"* ]]
  wait "$POINTER_PID"
  POINTER_PID=""
  POINTER_READ_FD=""
  POINTER_WRITE_FD=""
}

# The leaf occupies the right 60 percent of the delegated main region. Derive
# its center from the correlated settled composition and root divider.
start_pointer
read -r ARM_X ARM_Y < <(python3 - "$SETTLED_DIVIDER" "$SETTLED_HEIGHT" \
  "$OUTPUT_SCALE" "$LOGICAL_WIDTH" "$LOGICAL_HEIGHT" <<'PY'
import sys
divider, height = map(int, sys.argv[1:3])
scale = float(sys.argv[3])
logical_width, logical_height = map(int, sys.argv[4:6])
x = round((divider * 0.7) / scale)
y = round((height * 0.5) / scale)
print(min(max(x, 2), logical_width - 2), min(max(y, 2), logical_height - 2))
PY
)
arm_region_proof arm
stop_pointer

latest_divider_logical() {
  local divider
  divider="$(rg 'resize-present .* phase=settled ' "$CLIENT_LOG" | tail -1 \
    | sed -n 's/.* divider_x=\([0-9][0-9]*\) .*/\1/p')"
  python3 - "$divider" "$OUTPUT_SCALE" "$LOGICAL_WIDTH" <<'PY'
import sys
divider = int(sys.argv[1])
scale = float(sys.argv[2])
width = int(sys.argv[3])
print(min(max(round(divider / scale), 2), width - 2))
PY
}

wait_acceptance() {
  local after="$1"
  wait_for_pattern "$after" 'accepted synchronized layout actor=session-switcher .*request=[0-9]+.* interaction=[0-9]+.* event_sequence=[0-9]+.* epoch=[0-9]+.*target_rects=[[][^]]*[]]$'
  ACCEPT_LINE="$WAIT_LINE"
  ACCEPT_TEXT="$WAIT_TEXT"
  read -r ACCEPT_REQUEST ACCEPT_INTERACTION ACCEPT_SEQUENCE ACCEPT_EPOCH < <(
    python3 - "$ACCEPT_TEXT" <<'PY'
import re
import sys
text = sys.argv[1]
def field(name):
    found = re.search(rf"(?:^| ){name}=([^ ]+)", text)
    if found is None:
        raise SystemExit(f"missing {name}: {text}")
    return found.group(1)
print(field("request"), field("interaction"), field("event_sequence"), field("epoch"))
PY
  )
}

wait_resize_phase() {
  local epoch="$1"
  local phase="$2"
  local after="$3"
  wait_for_pattern "$after" "resize-present .*epoch=${epoch} .*phase=${phase} "
}

capture_frame() {
  local label="$1"
  local epoch="$2"
  local log_line="$3"
  local path="$FRAMES/$label.png"
  wayland_env grim -l 0 -o "$OUTPUT_NAME" "$path"
  jq -cn --arg label "$label" --arg file "frames/$label.png" \
    --argjson epoch "$epoch" --argjson log_line "$log_line" \
    '{label:$label,file:$file,epoch:$epoch,log_line:$log_line}' >> "$MANIFEST"
}

DIVIDER_X="$(latest_divider_logical)"
POINTER_Y=$((LOGICAL_HEIGHT / 2))
FIRST_X=$((DIVIDER_X - 36))
SECOND_X=$((DIVIDER_X - 72))
((FIRST_X > 2 && SECOND_X > 2))

start_pointer
send_pointer supersession-hover "motion $DIVIDER_X $POINTER_Y"
send_pointer supersession-press press
send_pointer supersession-first "motion $FIRST_X $POINTER_Y"
wait_acceptance "$INPUT_BEFORE"
FIRST_REQUEST="$ACCEPT_REQUEST"
FIRST_INTERACTION="$ACCEPT_INTERACTION"
FIRST_SEQUENCE="$ACCEPT_SEQUENCE"
FIRST_EPOCH="$ACCEPT_EPOCH"
FIRST_ACCEPT_LINE="$ACCEPT_LINE"

send_pointer supersession-second "motion $SECOND_X $POINTER_Y"
wait_acceptance "$INPUT_BEFORE"
SECOND_REQUEST="$ACCEPT_REQUEST"
SECOND_INTERACTION="$ACCEPT_INTERACTION"
SECOND_SEQUENCE="$ACCEPT_SEQUENCE"
SECOND_EPOCH="$ACCEPT_EPOCH"
SECOND_ACCEPT_LINE="$ACCEPT_LINE"
wait_resize_phase "$SECOND_EPOCH" projected "$SECOND_ACCEPT_LINE"
PROJECTED_LINE="$WAIT_LINE"
capture_frame normal-projected "$SECOND_EPOCH" "$PROJECTED_LINE"
wait_resize_phase "$SECOND_EPOCH" promoted "$PROJECTED_LINE"
PROMOTED_LINE="$WAIT_LINE"
capture_frame normal-promoted "$SECOND_EPOCH" "$PROMOTED_LINE"
wait_for_pattern "$PROMOTED_LINE" "completed synchronized layout actor=session-switcher request=${SECOND_REQUEST} "
SECOND_COMPLETE_LINE="$WAIT_LINE"
wait_for_pattern "$SECOND_ACCEPT_LINE" "layout-region response-rejected epoch=${FIRST_EPOCH} .*actor=workspace .*reason=StaleEpoch"
STALE_REPLY_LINE="$WAIT_LINE"

send_pointer normal-release release
wait_acceptance "$INPUT_BEFORE"
RELEASE_REQUEST="$ACCEPT_REQUEST"
RELEASE_INTERACTION="$ACCEPT_INTERACTION"
RELEASE_SEQUENCE="$ACCEPT_SEQUENCE"
RELEASE_EPOCH="$ACCEPT_EPOCH"
RELEASE_ACCEPT_LINE="$ACCEPT_LINE"
wait_resize_phase "$RELEASE_EPOCH" promoted "$RELEASE_ACCEPT_LINE"
RELEASE_PROMOTED_LINE="$WAIT_LINE"
wait_for_pattern "$RELEASE_PROMOTED_LINE" "completed synchronized layout actor=session-switcher request=${RELEASE_REQUEST} "
RELEASE_COMPLETE_LINE="$WAIT_LINE"
wait_for_pattern "$SECOND_ACCEPT_LINE" "pointer-policy capture released interaction=${RELEASE_INTERACTION} event_sequence=${RELEASE_SEQUENCE} phase=ButtonRelease"
CAPTURE_RELEASE_LINE="$WAIT_LINE"
stop_pointer

# Re-arm so the timeout transaction is response index four. Three small
# motions in one captured interaction converge before the fourth is withheld.
start_pointer
arm_region_proof final-rearm
stop_pointer

# Advance to response four inside one captured interaction: three motions
# converge, then the fourth motion is withheld and times out.
DIVIDER_X="$(latest_divider_logical)"
start_pointer
send_pointer timeout-hover "motion $DIVIDER_X $POINTER_Y"
send_pointer timeout-press press
for step in 1 2 3; do
  TARGET_X=$((DIVIDER_X - step * 12))
  send_pointer "timeout-prelude-$step" "motion $TARGET_X $POINTER_Y"
  wait_acceptance "$INPUT_BEFORE"
  PRELUDE_EPOCH="$ACCEPT_EPOCH"
  PRELUDE_REQUEST="$ACCEPT_REQUEST"
  wait_resize_phase "$PRELUDE_EPOCH" promoted "$ACCEPT_LINE"
  wait_for_pattern "$WAIT_LINE" "completed synchronized layout actor=session-switcher request=${PRELUDE_REQUEST} "
done
TIMEOUT_X=$((DIVIDER_X - 60))
send_pointer timeout-motion "motion $TIMEOUT_X $POINTER_Y"
wait_acceptance "$INPUT_BEFORE"
TIMEOUT_REQUEST="$ACCEPT_REQUEST"
TIMEOUT_INTERACTION="$ACCEPT_INTERACTION"
TIMEOUT_SEQUENCE="$ACCEPT_SEQUENCE"
TIMEOUT_EPOCH="$ACCEPT_EPOCH"
TIMEOUT_ACCEPT_LINE="$ACCEPT_LINE"
wait_resize_phase "$TIMEOUT_EPOCH" projected "$TIMEOUT_ACCEPT_LINE"
TIMEOUT_PROJECTED_LINE="$WAIT_LINE"
capture_frame timeout-projected "$TIMEOUT_EPOCH" "$TIMEOUT_PROJECTED_LINE"
wait_resize_phase "$TIMEOUT_EPOCH" degraded "$TIMEOUT_PROJECTED_LINE"
TIMEOUT_DEGRADED_LINE="$WAIT_LINE"
capture_frame timeout-degraded "$TIMEOUT_EPOCH" "$TIMEOUT_DEGRADED_LINE"
wait_for_pattern "$TIMEOUT_ACCEPT_LINE" "layout-region timeout-recovery epoch=${TIMEOUT_EPOCH} recovered_leases=[[][0-9]+,[0-9]+[]]"
TIMEOUT_RECOVERY_LINE="$WAIT_LINE"
wait_for_pattern "$TIMEOUT_DEGRADED_LINE" "degraded synchronized layout actor=session-switcher request=${TIMEOUT_REQUEST} "
TIMEOUT_TERMINAL_LINE="$WAIT_LINE"

send_pointer timeout-release release
wait_acceptance "$INPUT_BEFORE"
FINAL_REQUEST="$ACCEPT_REQUEST"
FINAL_INTERACTION="$ACCEPT_INTERACTION"
FINAL_SEQUENCE="$ACCEPT_SEQUENCE"
FINAL_EPOCH="$ACCEPT_EPOCH"
FINAL_ACCEPT_LINE="$ACCEPT_LINE"
wait_resize_phase "$FINAL_EPOCH" promoted "$FINAL_ACCEPT_LINE"
FINAL_PROMOTED_LINE="$WAIT_LINE"
wait_for_pattern "$FINAL_PROMOTED_LINE" "completed synchronized layout actor=session-switcher request=${FINAL_REQUEST} "
FINAL_COMPLETE_LINE="$WAIT_LINE"
wait_for_pattern "$TIMEOUT_ACCEPT_LINE" "pointer-policy capture released interaction=${FINAL_INTERACTION} event_sequence=${FINAL_SEQUENCE} phase=ButtonRelease"
FINAL_CAPTURE_RELEASE_LINE="$WAIT_LINE"
stop_pointer

jq -n \
  --argjson root_grant "$ROOT_GRANT_LINE" \
  --argjson middle_grant "$MIDDLE_GRANT_LINE" \
  --argjson leaf_grant "$LEAF_GRANT_LINE" \
  --argjson settled_revision "$SETTLED_REVISION" \
  --argjson middle_state "$MIDDLE_STATE_LINE" \
  --argjson leaf_state "$LEAF_STATE_LINE" \
  --argjson settled "$SETTLED_LINE" \
  --argjson paint "$PAINT_LINE" \
  --argjson armed "$ARMED_LINE" \
  --argjson arm_x "$ARM_X" \
  --argjson arm_y "$ARM_Y" \
  --argjson root_pane "$ROOT_PANE" \
  --argjson middle_pane "$MIDDLE_PANE" \
  --argjson leaf_pane "$LEAF_PANE" \
  --argjson first_request "$FIRST_REQUEST" \
  --argjson first_interaction "$FIRST_INTERACTION" \
  --argjson first_sequence "$FIRST_SEQUENCE" \
  --argjson first_epoch "$FIRST_EPOCH" \
  --argjson first_accept "$FIRST_ACCEPT_LINE" \
  --argjson second_request "$SECOND_REQUEST" \
  --argjson second_interaction "$SECOND_INTERACTION" \
  --argjson second_sequence "$SECOND_SEQUENCE" \
  --argjson second_epoch "$SECOND_EPOCH" \
  --argjson second_accept "$SECOND_ACCEPT_LINE" \
  --argjson projected "$PROJECTED_LINE" \
  --argjson promoted "$PROMOTED_LINE" \
  --argjson completed "$SECOND_COMPLETE_LINE" \
  --argjson stale_reply "$STALE_REPLY_LINE" \
  --argjson release_request "$RELEASE_REQUEST" \
  --argjson release_sequence "$RELEASE_SEQUENCE" \
  --argjson release_epoch "$RELEASE_EPOCH" \
  --argjson release_accept "$RELEASE_ACCEPT_LINE" \
  --argjson release_capture "$CAPTURE_RELEASE_LINE" \
  --argjson release_promoted "$RELEASE_PROMOTED_LINE" \
  --argjson release_completed "$RELEASE_COMPLETE_LINE" \
  --argjson timeout_request "$TIMEOUT_REQUEST" \
  --argjson timeout_interaction "$TIMEOUT_INTERACTION" \
  --argjson timeout_sequence "$TIMEOUT_SEQUENCE" \
  --argjson timeout_epoch "$TIMEOUT_EPOCH" \
  --argjson timeout_accept "$TIMEOUT_ACCEPT_LINE" \
  --argjson timeout_projected "$TIMEOUT_PROJECTED_LINE" \
  --argjson timeout_recovery "$TIMEOUT_RECOVERY_LINE" \
  --argjson timeout_degraded "$TIMEOUT_DEGRADED_LINE" \
  --argjson timeout_terminal "$TIMEOUT_TERMINAL_LINE" \
  --argjson final_request "$FINAL_REQUEST" \
  --argjson final_sequence "$FINAL_SEQUENCE" \
  --argjson final_epoch "$FINAL_EPOCH" \
  --argjson final_accept "$FINAL_ACCEPT_LINE" \
  --argjson final_capture "$FINAL_CAPTURE_RELEASE_LINE" \
  --argjson final_promoted "$FINAL_PROMOTED_LINE" \
  --argjson final_completed "$FINAL_COMPLETE_LINE" \
  '{topology:{root_grant:$root_grant,middle_grant:$middle_grant,leaf_grant:$leaf_grant,
      settled_revision:$settled_revision,middle_state:$middle_state,leaf_state:$leaf_state,
      settled:$settled,paint:$paint,armed:$armed,arm:{x:$arm_x,y:$arm_y},
      panes:{root:$root_pane,middle:$middle_pane,leaf:$leaf_pane}},
    supersession:{first:{request:$first_request,interaction:$first_interaction,
        sequence:$first_sequence,epoch:$first_epoch,accepted:$first_accept},
      second:{request:$second_request,interaction:$second_interaction,
        sequence:$second_sequence,epoch:$second_epoch,accepted:$second_accept,
        projected:$projected,promoted:$promoted,completed:$completed},
      stale_reply:$stale_reply},
    release:{request:$release_request,sequence:$release_sequence,epoch:$release_epoch,
      accepted:$release_accept,capture_released:$release_capture,
      promoted:$release_promoted,completed:$release_completed},
    timeout:{request:$timeout_request,interaction:$timeout_interaction,
      sequence:$timeout_sequence,epoch:$timeout_epoch,accepted:$timeout_accept,
      projected:$timeout_projected,recovery:$timeout_recovery,
      degraded:$timeout_degraded,terminal:$timeout_terminal},
    final_release:{request:$final_request,sequence:$final_sequence,epoch:$final_epoch,
      accepted:$final_accept,capture_released:$final_capture,
      promoted:$final_promoted,completed:$final_completed}}' > "$EVIDENCE"

python3 tools/analyze-resize-region-tree.py \
  "$OUT" "$MANIFEST" "$EVIDENCE" "$CLIENT_LOG" "$OUTPUT_SCALE" \
  > "$METRICS" 2>> "$ANALYSIS_LOG"
jq -e '.verified == true and all(.assertions[]; . == true)' "$METRICS" > /dev/null
jq -n --slurpfile metrics "$METRICS" \
  '{verified:($metrics[0].verified and all($metrics[0].assertions[]; . == true)),
    assertions:$metrics[0].assertions,
    artifacts:{metrics:"metrics.json",evidence:"evidence.json",
      client_log:"client.log",frame_manifest:"frame-manifest.jsonl"}}' > "$PROOF"
jq -e '.verified == true and all(.assertions[]; . == true)' "$PROOF" > /dev/null

echo "resize_region_tree_proof=$PROOF"
echo "resize_region_tree_metrics=$METRICS"
