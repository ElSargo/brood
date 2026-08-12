#!/usr/bin/env bash
set -Eeuo pipefail

# This is the recursive/root-layout continuation of test-resize-overhaul-nested.sh.
# The parent proof owns the headless compositor and the deterministic proof
# artifacts; this lane replaces the small root-actor shim with the production
# session-switcher tree, then exercises compositor configure and shell-owned
# layout changes without weakening the original 44-capture drag proof.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

check_gated_capture_release_structure() {
  local actual expected
  actual="$(awk '
    /^capture_window_gated\(\)/ { in_function = 1 }
    in_function && /^  wait "\$CAPTURE_PID"$/ { in_window = 1 }
    in_window { print }
    in_window && /^  release_window_capture_gate "\$epoch"$/ { exit }
  ' "${BASH_SOURCE[0]}")"
  expected='  wait "$CAPTURE_PID"
  grim_completed_ns="$(date +%s%N)"
  release_window_capture_gate "$epoch"'
  if [[ "$actual" != "$expected" ]]; then
    echo "ERR gated capture must release immediately after grim completion" >&2
    printf 'expected:\n%s\nactual:\n%s\n' "$expected" "$actual" >&2
    return 1
  fi
}

check_gated_capture_release_structure
if [[ "${1:-}" == --check-window-gate-structure ]]; then
  exit 0
fi

for tool in cargo cc grim jq magick pkg-config python3 rg sha256sum swaymsg; do
  if ! command -v "$tool" > /dev/null 2>&1; then
    echo "ERR $tool is required for the window/shell resize proof" >&2
    exit 2
  fi
done

: "${BROOD_RESIZE_EXTENDED_OUT:?BROOD_RESIZE_EXTENDED_OUT must name the artifact directory}"
: "${BROOD_RESIZE_CLIENT:?BROOD_RESIZE_CLIENT must name the built brood-client}"
: "${BROOD_RESIZE_WORKSPACE_MODULE:?BROOD_RESIZE_WORKSPACE_MODULE must name default-workspace WASM}"
: "${BROOD_RESIZE_PROOF_COMPONENT:?BROOD_RESIZE_PROOF_COMPONENT must name resize-proof Component}"
: "${XDG_RUNTIME_DIR:?the parent nested compositor runtime is required}"
: "${WAYLAND_DISPLAY:?the parent nested compositor Wayland socket is required}"
: "${SWAYSOCK:?the parent nested compositor IPC socket is required}"

OUT="$BROOD_RESIZE_EXTENDED_OUT"
PACKAGE="$OUT/package"
WINDOW_FRAMES="$OUT/window-frames"
SIDEBAR_FRAMES="$OUT/sidebar-frames"
CLIENT_LOG="$OUT/client.log"
INPUT_LOG="$OUT/input.jsonl"
WINDOW_MANIFEST="$OUT/window-frame-manifest.jsonl"
SIDEBAR_MANIFEST="$OUT/sidebar-frame-manifest.jsonl"
EVENTS="$OUT/resize-events.jsonl"
METRICS="$OUT/metrics.json"
PROOF="$OUT/proof.json"
ANALYSIS_LOG="$OUT/analysis-errors.log"
STARTUP_READINESS="$OUT/startup-readiness.json"
OVERLAY_METRICS="$OUT/overlay-pane-metrics.json"
WINDOW_GATE_EVENTS="$OUT/window-gate-events.jsonl"
OVERLAY_FRAMES="$OUT/overlay-frames"
KEYBINDINGS_PATH="$OUT/keybindings.json"
DATA_HOME="$OUT/data"
mkdir -p "$PACKAGE" "$WINDOW_FRAMES" "$SIDEBAR_FRAMES" "$OVERLAY_FRAMES" "$DATA_HOME"
: > "$CLIENT_LOG"
: > "$INPUT_LOG"
: > "$WINDOW_MANIFEST"
: > "$SIDEBAR_MANIFEST"
: > "$EVENTS"
: > "$ANALYSIS_LOG"
: > "$WINDOW_GATE_EVENTS"

CLIENT_PID=""
POINTER_PID=""
CAPTURE_PID=""
WINDOW_CAPTURE_GATE_ACTIVE=false
cleanup() {
  local result=$?
  set +e
  if [[ -n "$POINTER_PID" ]] && kill -0 "$POINTER_PID" 2> /dev/null; then
    kill "$POINTER_PID" > /dev/null 2>&1 || true
    wait "$POINTER_PID" > /dev/null 2>&1 || true
  fi
  if [[ -n "$CAPTURE_PID" ]] && kill -0 "$CAPTURE_PID" 2> /dev/null; then
    kill "$CAPTURE_PID" > /dev/null 2>&1 || true
    wait "$CAPTURE_PID" > /dev/null 2>&1 || true
  fi
  if [[ -n "$CLIENT_PID" ]] && kill -0 "$CLIENT_PID" 2> /dev/null; then
    kill "$CLIENT_PID" > /dev/null 2>&1 || true
    for _ in $(seq 1 80); do
      kill -0 "$CLIENT_PID" 2> /dev/null || break
      sleep 0.025
    done
    kill -KILL "$CLIENT_PID" > /dev/null 2>&1 || true
    wait "$CLIENT_PID" > /dev/null 2>&1 || true
  fi
  if ((result != 0)); then
    {
      echo "window_shell_resize_proof=failed"
      echo "exit_status=$result"
      echo "artifacts=$OUT"
      echo "window_frames=$(find "$WINDOW_FRAMES" -maxdepth 1 -name '*.png' -type f | wc -l)"
      echo "sidebar_frames=$(find "$SIDEBAR_FRAMES" -maxdepth 1 -name '*.png' -type f | wc -l)"
      echo "analysis_errors:"
      tail -n 60 "$ANALYSIS_LOG" 2> /dev/null || true
      echo "last_window_traces:"
      rg 'window-resize-present |resize-present |synchronized layout actor=session-switcher ' \
        "$CLIENT_LOG" | tail -n 80 || true
      echo "inputs:"
      tail -n 40 "$INPUT_LOG" || true
    } > "$OUT/failure-summary.txt"
    echo "extended_resize_artifacts=$OUT" >&2
    tail -n 240 "$CLIENT_LOG" >&2 || true
  fi
  return "$result"
}
report_shell_error() {
  local status="$1"
  local command="$2"
  local line="$3"
  local functions="$4"
  local bash_lines="$5"
  local pipeline_statuses="$6"
  local record="ERR command=$command line=$line status=$status"
  record+=" functions=[$functions] bash_lines=[$bash_lines]"
  record+=" pipeline=[$pipeline_statuses]"
  printf '%s\n' "$record" >&2
  if [[ -n "${ANALYSIS_LOG:-}" ]]; then
    printf '%s\n' "$record" >> "$ANALYSIS_LOG"
  fi
  return "$status"
}
trap 'report_shell_error "$?" "$BASH_COMMAND" "$LINENO" "${FUNCNAME[*]}" "${BASH_LINENO[*]}" "${PIPESTATUS[*]}"' ERR
trap cleanup EXIT

wayland_env() {
  env \
    XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_DISPLAY" \
    SWAYSOCK="$SWAYSOCK" \
    "$@"
}

componentize_plugin() {
  local package="$1"
  local stem="$2"
  local core="plugins/$package/target/wasm32-unknown-unknown/release/${stem}.wasm"
  local component="$PACKAGE/${stem}.component.wasm"
  cargo build \
    --manifest-path "plugins/$package/Cargo.toml" \
    --target wasm32-unknown-unknown \
    --release \
    --locked
  cargo run --quiet -p brood-wasm --example componentize -- "$core" "$component"
  [[ -s "$component" ]]
}

componentize_plugin session-switcher brood_session_switcher_plugin
componentize_plugin status-bar brood_status_bar_plugin
componentize_plugin inbox brood_inbox_plugin
componentize_plugin picker brood_picker_plugin
componentize_plugin keybindings brood_keybindings_plugin

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

CLIENT_BIN="$(readlink -f "$BROOD_RESIZE_CLIENT")"
WORKSPACE_MODULE="$(readlink -f "$BROOD_RESIZE_WORKSPACE_MODULE")"
RESIZE_PROOF_COMPONENT="$(readlink -f "$BROOD_RESIZE_PROOF_COMPONENT")"
INIT_COMPONENT="$(readlink -f "$PACKAGE/brood_session_switcher_plugin.component.wasm")"
[[ -x "$CLIENT_BIN" && -s "$WORKSPACE_MODULE" && -s "$RESIZE_PROOF_COMPONENT" \
  && -s "$INIT_COMPONENT" ]]

OUTPUTS_JSON="$(wayland_env swaymsg -t get_outputs -r)"
OUTPUT_NAME="$(jq -r 'map(select(.active))[0].name // empty' <<< "$OUTPUTS_JSON")"
OUTPUT_SCALE="$(jq -r 'map(select(.active))[0].scale // 1' <<< "$OUTPUTS_JSON")"
LOGICAL_WIDTH="$(jq -r 'map(select(.active))[0].rect.width // 0' <<< "$OUTPUTS_JSON")"
LOGICAL_HEIGHT="$(jq -r 'map(select(.active))[0].rect.height // 0' <<< "$OUTPUTS_JSON")"
MODE_WIDTH="$(jq -r 'map(select(.active))[0].current_mode.width // 0' <<< "$OUTPUTS_JSON")"
MODE_HEIGHT="$(jq -r 'map(select(.active))[0].current_mode.height // 0' <<< "$OUTPUTS_JSON")"
[[ -n "$OUTPUT_NAME" && "$LOGICAL_WIDTH" -gt 0 && "$LOGICAL_HEIGHT" -gt 0 \
  && "$MODE_WIDTH" -gt 0 && "$MODE_HEIGHT" -gt 0 ]]

RUST_LOG="${RUST_LOG:-brood_client=info,smithay=warn,wgpu_core=warn,wgpu_hal=warn}"
env -u brood_session -u BROOD_SURFACE_SESSION_ID -u BROOD_PANE_ID \
  XDG_DATA_HOME="$DATA_HOME" \
  BROOD_KEYBINDINGS_PATH="$KEYBINDINGS_PATH" \
  XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_DISPLAY" \
  BROOD_TRACE_PLUGIN_GRAPHICS=1 \
  BROOD_TRACE_PLUGIN_INPUT=1 \
  BROOD_TRACE_RESIZE=1 \
  WGPU_BACKEND=vulkan \
  BROOD_DMABUF_HAPPY_PATH=1 \
  RUST_LOG="$RUST_LOG" \
  "$CLIENT_BIN" \
  --no-server \
  --init-actor "$INIT_COMPONENT" \
  --wasm-actor "resize-proof-fast=$RESIZE_PROOF_COMPONENT" \
  --wasm-actor "resize-proof-window-slow=$RESIZE_PROOF_COMPONENT" \
  --wasm-key-policy workspace "$WORKSPACE_MODULE" \
  > "$CLIENT_LOG" 2>&1 &
CLIENT_PID=$!

ROOT_TREE_READY_LINE=""
for _ in $(seq 1 1200); do
  LAST_WINDOW_STARTUP_TRACE="$(rg 'window-resize-present ' "$CLIENT_LOG" | tail -1 || true)"
  ROOT_TREE_READY_LINE="$(awk '
      index($0, "granted rest layout lease ") != 0 \
        && index($0, " actor=workspace panes=2") != 0 {
          rest = NR
          root = child = settled = 0
          next
        }
      rest > 0 && index($0, "accepted layout fragment lease=1 ") != 0 \
        && index($0, "; waiting for child") != 0 {
          root = NR
          next
        }
      root > 0 && index($0, "accepted leased actor layout items=4 panes=4 ") != 0 {
          child = NR
          next
        }
      child > 0 && index($0, "resize-present ") != 0 \
        && index($0, " phase=settled ") != 0 \
        && $0 ~ /divider_x=[0-9]+/ {
          settled = NR
        }
      END {
        if (rest < root && root < child && child < settled) print settled
      }
    ' "$CLIENT_LOG")"
  if rg -q 'started presentation WASM actor id=resize-proof-fast ' "$CLIENT_LOG" \
    && rg -q 'started presentation WASM actor id=resize-proof-window-slow ' "$CLIENT_LOG" \
    && rg -q 'started presentation WASM actor id=session-switcher ' "$CLIENT_LOG" \
    && rg -q 'started presentation WASM actor id=inbox ' "$CLIENT_LOG" \
    && rg -q 'started presentation WASM actor id=status-bar ' "$CLIENT_LOG" \
    && rg -q 'granted claimed root layout lease actor=session-switcher ' "$CLIENT_LOG" \
    && rg -q 'granted rest layout lease .* actor=workspace panes=2' "$CLIENT_LOG" \
    && rg -q 'resize-proof-untagged-resize.*"mode":"fast".*"success":true' "$CLIENT_LOG" \
    && rg -q 'resize-proof-untagged-resize.*"mode":"slow".*"success":true' "$CLIENT_LOG" \
    && [[ -n "$ROOT_TREE_READY_LINE" ]]; then
    break
  fi
  kill -0 "$CLIENT_PID" 2> /dev/null || {
    echo "ERR production shell proof client exited during startup" >&2
    exit 1
  }
  sleep 0.025
done
rg -q 'granted claimed root layout lease actor=session-switcher ' "$CLIENT_LOG"
rg -q 'granted rest layout lease .* actor=workspace panes=2' "$CLIENT_LOG"
[[ -n "$ROOT_TREE_READY_LINE" ]]

FAST_PANE="$(sed -n 's/.*started presentation WASM actor id=resize-proof-fast .* pane=\([0-9][0-9]*\) .*/\1/p' "$CLIENT_LOG" | tail -1)"
SLOW_PANE="$(sed -n 's/.*started presentation WASM actor id=resize-proof-window-slow .* pane=\([0-9][0-9]*\) .*/\1/p' "$CLIENT_LOG" | tail -1)"
SWITCHER_PANE="$(sed -n 's/.*started presentation WASM actor id=session-switcher .* pane=\([0-9][0-9]*\) .*/\1/p' "$CLIENT_LOG" | tail -1)"
INBOX_PANE="$(sed -n 's/.*started presentation WASM actor id=inbox .* pane=\([0-9][0-9]*\) .*/\1/p' "$CLIENT_LOG" | tail -1)"
STATUS_PANE="$(sed -n 's/.*started presentation WASM actor id=status-bar .* pane=\([0-9][0-9]*\) .*/\1/p' "$CLIENT_LOG" | tail -1)"
[[ -n "$FAST_PANE" && -n "$SLOW_PANE" && -n "$SWITCHER_PANE" \
  && -n "$INBOX_PANE" && -n "$STATUS_PANE" ]]

WINDOW_TRACE_CURSOR=0
RESIZE_TRACE_CURSOR=0
WINDOW_TRACE_LINE=0
WINDOW_TRACE_TEXT=""
RESIZE_TRACE_LINE=0
RESIZE_TRACE_TEXT=""
ACCEPTED_LINE=0
ACCEPTED_TEXT=""
ACCEPTED_REQUEST=0
ACCEPTED_EVENT_SEQUENCE=0
ACCEPTED_EPOCH=0
ACCEPTED_NATIVE_REVISION=0
TERMINAL_LINE=0
NATIVE_SETTLE_LINE=0
DIAGNOSTIC_LINE=0
DIAGNOSTIC_JSON="{}"

wait_window_trace() {
  local phases="$1"
  local width="${2:--1}"
  local height="${3:--1}"
  local after_line="${4:-$WINDOW_TRACE_CURSOR}"
  local result=""
  for _ in $(seq 1 800); do
    result="$(awk \
      -v start="$after_line" \
      -v wanted_phases="$phases" \
      -v wanted_width="$width" \
      -v wanted_height="$height" '
        NR <= start || index($0, "window-resize-present ") == 0 { next }
        $0 !~ / managed_staged_resources=[0-9]+ degraded=(true|false)$/ { next }
        {
          phase = ""; actual_width = -1; actual_height = -1;
          for (field_index = 1; field_index <= NF; field_index += 1) {
            split($field_index, pair, "=");
            if (pair[1] == "phase") phase = pair[2];
            if (pair[1] == "width") actual_width = pair[2] + 0;
            if (pair[1] == "height") actual_height = pair[2] + 0;
          }
          if (phase ~ ("^(" wanted_phases ")$") \
              && (wanted_width < 0 || actual_width == wanted_width) \
              && (wanted_height < 0 || actual_height == wanted_height)) {
            print NR "\t" substr($0, index($0, "window-resize-present "));
            exit;
          }
        }
      ' "$CLIENT_LOG")"
    if [[ -n "$result" ]]; then
      WINDOW_TRACE_LINE="${result%%$'\t'*}"
      WINDOW_TRACE_TEXT="${result#*$'\t'}"
      WINDOW_TRACE_CURSOR="$WINDOW_TRACE_LINE"
      return 0
    fi
    kill -0 "$CLIENT_PID" 2> /dev/null || return 1
    sleep 0.01
  done
  echo "ERR timed out waiting for window-resize-present phase=$phases size=${width}x${height} after=$after_line" >&2
  rg 'window-resize-present ' "$CLIENT_LOG" | tail -n 40 >&2 || true
  return 1
}

wait_resize_trace() {
  local event_sequence="$1"
  local epoch="$2"
  local phases="$3"
  local after_line="${4:-$RESIZE_TRACE_CURSOR}"
  local result=""
  for _ in $(seq 1 800); do
    result="$(awk \
      -v start="$after_line" \
      -v wanted_sequence="$event_sequence" \
      -v wanted_epoch="$epoch" \
      -v wanted_phases="$phases" '
        NR <= start || index($0, "resize-present ") == 0 { next }
        index($0, "window-resize-present ") != 0 { next }
        $0 !~ / managed_staged_resources=[0-9]+ degraded=(true|false) superseded_epochs=[0-9]+$/ { next }
        {
          sequence = ""; epoch = ""; phase = "";
          for (field_index = 1; field_index <= NF; field_index += 1) {
            split($field_index, pair, "=");
            if (pair[1] == "event_sequence") sequence = pair[2];
            if (pair[1] == "epoch") epoch = pair[2];
            if (pair[1] == "phase") phase = pair[2];
          }
          if (sequence == wanted_sequence && epoch == wanted_epoch \
              && phase ~ ("^(" wanted_phases ")$")) {
            print NR "\t" substr($0, index($0, "resize-present "));
            exit;
          }
        }
      ' "$CLIENT_LOG")"
    if [[ -n "$result" ]]; then
      RESIZE_TRACE_LINE="${result%%$'\t'*}"
      RESIZE_TRACE_TEXT="${result#*$'\t'}"
      RESIZE_TRACE_CURSOR="$RESIZE_TRACE_LINE"
      return 0
    fi
    kill -0 "$CLIENT_PID" 2> /dev/null || return 1
    sleep 0.01
  done
  echo "ERR timed out waiting for resize-present sequence=$event_sequence epoch=$epoch phase=$phases after=$after_line" >&2
  rg 'resize-present ' "$CLIENT_LOG" | tail -n 50 >&2 || true
  return 1
}

wait_session_request() {
  local after_line="$1"
  local native_requirement="$2"
  local reason="$3"
  local result=""
  for _ in $(seq 1 800); do
    result="$(awk -v start="$after_line" '
      NR <= start || index($0, "accepted synchronized layout actor=session-switcher ") == 0 { next }
      $0 !~ / request=[0-9]+ / || $0 !~ / event_sequence=[0-9]+ / \
        || $0 !~ / native_resize_revision=[0-9]+ / || $0 !~ / epoch=[0-9]+ / \
        || $0 !~ / target_rects=[[][^]]*[]]$/ { next }
      { print NR "\t" substr($0, index($0, "accepted synchronized layout ")); exit }
    ' "$CLIENT_LOG")"
    if [[ -n "$result" ]]; then
      ACCEPTED_LINE="${result%%$'\t'*}"
      ACCEPTED_TEXT="${result#*$'\t'}"
      read -r ACCEPTED_REQUEST ACCEPTED_EVENT_SEQUENCE ACCEPTED_EPOCH ACCEPTED_NATIVE_REVISION < <(
        python3 - "$ACCEPTED_TEXT" <<'PY'
import re
import sys

text = sys.argv[1]
def field(name, default=None):
    match = re.search(rf"(?:^| ){name}=([^ ]+)", text)
    if match is None:
        if default is not None:
            return default
        raise SystemExit(f"accepted session request has no {name}: {text}")
    return match.group(1)

print(field("request"), field("event_sequence"), field("epoch"), field("native_resize_revision", "0"))
PY
      )
      if [[ "$native_requirement" == nonzero && "$ACCEPTED_NATIVE_REVISION" -eq 0 ]]; then
        echo "ERR native window request did not echo a nonzero native_resize_revision: $ACCEPTED_TEXT" >&2
        return 1
      fi
      if [[ "$native_requirement" == zero && "$ACCEPTED_NATIVE_REVISION" -ne 0 ]]; then
        echo "ERR shell key request unexpectedly carried native_resize_revision: $ACCEPTED_TEXT" >&2
        return 1
      fi
      DIAGNOSTIC_LINE="$ACCEPTED_LINE"
      DIAGNOSTIC_JSON="$(python3 - \
        "$ACCEPTED_TEXT" \
        "$reason" \
        "$FAST_PANE" \
        "$SLOW_PANE" \
        "$SWITCHER_PANE" \
        "$INBOX_PANE" \
        "$STATUS_PANE" <<'PY'
import json
import re
import sys

text = sys.argv[1]
reason = sys.argv[2]
fast, slow, switcher, inbox, status = map(int, sys.argv[3:8])

def field(name):
    match = re.search(rf"(?:^| ){name}=([^ ]+)", text)
    if match is None:
        raise SystemExit(f"authenticated acceptance has no {name}: {text}")
    return match.group(1)

wait_text = field("wait_for_panes")
if not wait_text.startswith("[") or not wait_text.endswith("]"):
    raise SystemExit(f"invalid authenticated wait set: {wait_text}")
wait = [] if wait_text == "[]" else [int(value) for value in wait_text[1:-1].split(",")]
if len(wait) != len(set(wait)):
    raise SystemExit(f"authenticated wait set contains duplicates: {wait}")

rects_text = field("target_rects")
if not rects_text.startswith("[") or not rects_text.endswith("]"):
    raise SystemExit(f"invalid authenticated target rects: {rects_text}")
rects = {}
if rects_text != "[]":
    for encoded in rects_text[1:-1].split(","):
        pane, x, y, width, height = map(int, encoded.split(":"))
        if pane in rects or width <= 0 or height <= 0:
            raise SystemExit(f"invalid/duplicate authenticated target rect: {encoded}")
        rects[pane] = [x, y, width, height]

if fast not in rects or slow not in rects or status not in rects:
    raise SystemExit(
        f"authenticated root target omits proof/status panes: "
        f"fast={fast} slow={slow} status={status} rects={rects}"
    )
proof = [rects[fast], rects[slow]]
rest_left = min(rect[0] for rect in proof)
rest_right = max(rect[0] + rect[2] for rect in proof)
rest_top = min(rect[1] for rect in proof)
rest_bottom = max(rect[1] + rect[3] for rect in proof)

document = {
    "request_id": int(field("request")),
    "reason": reason,
    "event_sequence": int(field("event_sequence")),
    "native_resize_revision": int(field("native_resize_revision")),
    "rest_width": float(rest_right - rest_left),
    "rest_height": float(rest_bottom - rest_top),
    "sidebar_width": float(rects.get(switcher, [0, 0, 0, 0])[2]),
    "inbox_width": float(rects.get(inbox, [0, 0, 0, 0])[2]),
    "status_height": float(rects[status][3]),
    "focus": int(field("focus")),
    "wait_for_panes": wait,
    "target_rects": {str(pane): rect for pane, rect in sorted(rects.items())},
    "actor_generation": int(field("actor_generation")),
}
print(json.dumps(document, sort_keys=True))
PY
      )"
      return 0
    fi
    kill -0 "$CLIENT_PID" 2> /dev/null || return 1
    sleep 0.01
  done
  echo "ERR timed out waiting for accepted session-switcher synchronized request after=$after_line" >&2
  rg 'synchronized layout actor=session-switcher ' "$CLIENT_LOG" | tail -n 50 >&2 || true
  return 1
}

wait_session_completed() {
  local request="$1"
  local after_line="$2"
  local result=""
  for _ in $(seq 1 800); do
    result="$(awk -v start="$after_line" -v request="$request" '
      NR <= start { next }
      index($0, "completed synchronized layout actor=session-switcher ") == 0 { next }
      index($0, "request=" request " ") != 0 && $0 ~ / duration_us=[0-9]+$/ { print NR; exit }
    ' "$CLIENT_LOG")"
    if [[ -n "$result" ]]; then
      TERMINAL_LINE="$result"
      return 0
    fi
    kill -0 "$CLIENT_PID" 2> /dev/null || return 1
    sleep 0.01
  done
  echo "ERR timed out waiting for completed session-switcher request=$request" >&2
  rg 'synchronized layout actor=session-switcher ' "$CLIENT_LOG" | tail -n 50 >&2 || true
  return 1
}

assert_diagnostic_matches_acceptance() {
  local diagnostic_request diagnostic_sequence diagnostic_native
  diagnostic_request="$(jq -r .request_id <<< "$DIAGNOSTIC_JSON")"
  diagnostic_sequence="$(jq -r .event_sequence <<< "$DIAGNOSTIC_JSON")"
  diagnostic_native="$(jq -r .native_resize_revision <<< "$DIAGNOSTIC_JSON")"
  [[ "$diagnostic_request" -eq "$ACCEPTED_REQUEST" \
    && "$diagnostic_sequence" -eq "$ACCEPTED_EVENT_SEQUENCE" \
    && "$diagnostic_native" -eq "$ACCEPTED_NATIVE_REVISION" ]] || {
    echo "ERR session diagnostic and host acceptance identities differ" >&2
    echo "diagnostic=$DIAGNOSTIC_JSON" >&2
    echo "accepted=$ACCEPTED_TEXT" >&2
    return 1
  }
  jq -e --argjson fast "$FAST_PANE" --argjson slow "$SLOW_PANE" '
    (.wait_for_panes | length) >= 2
    and (.wait_for_panes | length) == (.wait_for_panes | unique | length)
    and (.wait_for_panes | index($fast)) != null
    and (.wait_for_panes | index($slow)) != null
  ' <<< "$DIAGNOSTIC_JSON" > /dev/null
}

window_rect_json() {
  wayland_env swaymsg -t get_tree -r \
    | jq -c --argjson pid "$CLIENT_PID" \
      '[.. | objects | select(.pid? == $pid)][0].rect // empty'
}

record_capture() {
  local manifest="$1"
  local lane="$2"
  local label="$3"
  local file="$4"
  local trace_kind="$5"
  local trace_line="$6"
  local trace_text="$7"
  local input_index="$8"
  local rect_json="$9"
  local width height digest captured_ms
  read -r width height < <(magick "$OUT/$file" -format '%w %h\n' info:)
  digest="$(sha256sum "$OUT/$file" | awk '{print $1}')"
  captured_ms="${10:-}"
  if [[ -z "$captured_ms" ]]; then
    captured_ms="$(date +%s%3N)"
  fi
  jq -cn \
    --arg lane "$lane" \
    --arg label "$label" \
    --arg file "$file" \
    --arg sha256 "$digest" \
    --arg trace_kind "$trace_kind" \
    --arg trace_text "$trace_text" \
    --argjson trace_line "$trace_line" \
    --argjson input_index "$input_index" \
    --argjson captured_unix_ms "$captured_ms" \
    --argjson image_width "$width" \
    --argjson image_height "$height" \
    --argjson window_rect "$rect_json" \
    --argjson output_scale "$OUTPUT_SCALE" \
    '{lane:$lane,label:$label,file:$file,sha256:$sha256,
      captured_unix_ms:$captured_unix_ms,input_index:$input_index,
      image_size:[$image_width,$image_height],window_rect:$window_rect,
      output_scale:$output_scale,
      trace:{kind:$trace_kind,log_line:$trace_line,text:$trace_text}}' \
    >> "$manifest"
}

WINDOW_FRAME_INDEX=0
DEFERRED_WINDOW_LABELS=()
DEFERRED_WINDOW_PATHS=()
DEFERRED_WINDOW_TRACE_KINDS=()
DEFERRED_WINDOW_TRACE_LINES=()
DEFERRED_WINDOW_TRACE_TEXTS=()
DEFERRED_WINDOW_INPUT_INDICES=()
DEFERRED_WINDOW_RECTS=()
DEFERRED_WINDOW_CAPTURED_MS=()
capture_window() {
  local label="$1"
  local trace_kind="$2"
  local trace_line="$3"
  local trace_text="$4"
  local input_index="$5"
  local frame path rect x y width height
  rect="$(window_rect_json)"
  [[ -n "$rect" && "$rect" != null ]]
  read -r x y width height < <(jq -r '[.x,.y,.width,.height] | @tsv' <<< "$rect")
  printf -v frame '%03d' "$WINDOW_FRAME_INDEX"
  path="window-frames/frame-${frame}-${label}.png"
  # Reduce PNG work after the compositor supplies the output buffer. The
  # window-specific slow actor provides the observable projected interval.
  wayland_env grim -l 0 -g "${x},${y} ${width}x${height}" "$OUT/$path"
  [[ -s "$OUT/$path" ]]
  record_capture "$WINDOW_MANIFEST" window "$label" "$path" \
    "$trace_kind" "$trace_line" "$trace_text" "$input_index" "$rect"
  WINDOW_FRAME_INDEX=$((WINDOW_FRAME_INDEX + 1))
}

capture_window_deferred() {
  local label="$1"
  local trace_kind="$2"
  local trace_line="$3"
  local trace_text="$4"
  local input_index="$5"
  local frame path rect x y width height captured_ms
  rect="$(window_rect_json)"
  [[ -n "$rect" && "$rect" != null ]]
  read -r x y width height < <(jq -r '[.x,.y,.width,.height] | @tsv' <<< "$rect")
  printf -v frame '%03d' "$WINDOW_FRAME_INDEX"
  path="window-frames/frame-${frame}-${label}.png"
  wayland_env grim -l 0 -g "${x},${y} ${width}x${height}" "$OUT/$path"
  [[ -s "$OUT/$path" ]]
  captured_ms="$(date +%s%3N)"
  DEFERRED_WINDOW_LABELS+=("$label")
  DEFERRED_WINDOW_PATHS+=("$path")
  DEFERRED_WINDOW_TRACE_KINDS+=("$trace_kind")
  DEFERRED_WINDOW_TRACE_LINES+=("$trace_line")
  DEFERRED_WINDOW_TRACE_TEXTS+=("$trace_text")
  DEFERRED_WINDOW_INPUT_INDICES+=("$input_index")
  DEFERRED_WINDOW_RECTS+=("$rect")
  DEFERRED_WINDOW_CAPTURED_MS+=("$captured_ms")
  WINDOW_FRAME_INDEX=$((WINDOW_FRAME_INDEX + 1))
}

flush_deferred_window_captures() {
  local deferred_index
  for ((deferred_index = 0;
       deferred_index < ${#DEFERRED_WINDOW_LABELS[@]};
       deferred_index++)); do
    record_capture \
      "$WINDOW_MANIFEST" \
      window \
      "${DEFERRED_WINDOW_LABELS[$deferred_index]}" \
      "${DEFERRED_WINDOW_PATHS[$deferred_index]}" \
      "${DEFERRED_WINDOW_TRACE_KINDS[$deferred_index]}" \
      "${DEFERRED_WINDOW_TRACE_LINES[$deferred_index]}" \
      "${DEFERRED_WINDOW_TRACE_TEXTS[$deferred_index]}" \
      "${DEFERRED_WINDOW_INPUT_INDICES[$deferred_index]}" \
      "${DEFERRED_WINDOW_RECTS[$deferred_index]}" \
      "${DEFERRED_WINDOW_CAPTURED_MS[$deferred_index]}"
  done
  DEFERRED_WINDOW_LABELS=()
  DEFERRED_WINDOW_PATHS=()
  DEFERRED_WINDOW_TRACE_KINDS=()
  DEFERRED_WINDOW_TRACE_LINES=()
  DEFERRED_WINDOW_TRACE_TEXTS=()
  DEFERRED_WINDOW_INPUT_INDICES=()
  DEFERRED_WINDOW_RECTS=()
  DEFERRED_WINDOW_CAPTURED_MS=()
}

SIDEBAR_FRAME_INDEX=0
capture_sidebar() {
  local label="$1"
  local trace_line="$2"
  local trace_text="$3"
  local input_index="$4"
  local frame path rect
  rect="$(window_rect_json)"
  [[ -n "$rect" && "$rect" != null ]]
  printf -v frame '%03d' "$SIDEBAR_FRAME_INDEX"
  path="sidebar-frames/frame-${frame}-${label}.png"
  wayland_env grim -o "$OUTPUT_NAME" "$OUT/$path"
  [[ -s "$OUT/$path" ]]
  record_capture "$SIDEBAR_MANIFEST" sidebar "$label" "$path" \
    resize-present "$trace_line" "$trace_text" "$input_index" "$rect"
  SIDEBAR_FRAME_INDEX=$((SIDEBAR_FRAME_INDEX + 1))
}

INPUT_INDEX=0
INPUT_DURATION_MS=0
INPUT_LOG_CURSOR=0
LAST_SWAY_INPUT_STARTED_MS=0
record_input() {
  local kind="$1"
  local action="$2"
  local command="$3"
  local duration_ms="$4"
  local log_cursor="$5"
  local ack="$6"
  jq -cn \
    --argjson index "$INPUT_INDEX" \
    --arg kind "$kind" \
    --arg action "$action" \
    --arg command "$command" \
    --argjson duration_ms "$duration_ms" \
    --argjson log_cursor "$log_cursor" \
    --arg ack "$ack" \
    '{index:$index,kind:$kind,action:$action,command:$command,
      compositor_roundtrip_ms:$duration_ms,client_log_cursor:$log_cursor,ack:$ack}' \
    >> "$INPUT_LOG"
}

send_sway_command() {
  local action="$1"
  local command="$2"
  local started_ns ended_ns ack
  INPUT_INDEX=$((INPUT_INDEX + 1))
  INPUT_LOG_CURSOR="$(wc -l < "$CLIENT_LOG")"
  started_ns="$(date +%s%N)"
  LAST_SWAY_INPUT_STARTED_MS=$((started_ns / 1000000))
  ack="$(wayland_env swaymsg -r "$command")"
  ended_ns="$(date +%s%N)"
  INPUT_DURATION_MS=$(((ended_ns - started_ns) / 1000000))
  jq -e 'all(.[]; .success == true)' <<< "$ack" > /dev/null
  record_input sway-ipc "$action" "$command" "$INPUT_DURATION_MS" "$INPUT_LOG_CURSOR" "$ack"
}

send_shell_key() {
  local action="$1"
  local keycode="$2"
  local started_ns ended_ns
  INPUT_INDEX=$((INPUT_INDEX + 1))
  INPUT_LOG_CURSOR="$(wc -l < "$CLIENT_LOG")"
  started_ns="$(date +%s%N)"
  wayland_env "$OUT/virtual-keyboard-key" "$keycode"
  ended_ns="$(date +%s%N)"
  INPUT_DURATION_MS=$(((ended_ns - started_ns) / 1000000))
  record_input virtual-keyboard "$action" "ctrl-alt evdev=$keycode" \
    "$INPUT_DURATION_MS" "$INPUT_LOG_CURSOR" compositor-roundtrip
}

send_overlay_probe_key() {
  local modifier_mode="$1"
  local keycode="$2"
  INPUT_LOG_CURSOR="$(wc -l < "$CLIENT_LOG")"
  if [[ "$modifier_mode" == "--ctrl-alt" ]]; then
    wayland_env "$OUT/virtual-keyboard-key" "$keycode"
  else
    wayland_env "$OUT/virtual-keyboard-key" "$modifier_mode" "$keycode"
  fi
}

wait_overlay_paint_order() {
  local after_line="$1"
  local expected_visible="$2"
  local result=""
  for _ in $(seq 1 800); do
    result="$(awk \
      -v start="$after_line" \
      -v expected="$expected_visible" '
        NR <= start || index($0, "resize-paint-order ") == 0 { next }
        {
          overlay = elevated = -1
          for (field_index = 1; field_index <= NF; field_index += 1) {
            split($field_index, pair, "=")
            if (pair[1] == "explicit_overlay_panes") overlay = pair[2] + 0
            if (pair[1] == "elevated_overlay_panes") elevated = pair[2] + 0
          }
          if ((expected == 1 && overlay > 0 && elevated > 0) \
              || (expected == 0 && overlay == 0 && elevated == 0)) {
            print NR "\t" substr($0, index($0, "resize-paint-order "))
            exit
          }
        }
      ' "$CLIENT_LOG")"
    if [[ -n "$result" ]]; then
      OVERLAY_PAINT_LINE="${result%%$'\t'*}"
      OVERLAY_PAINT_TEXT="${result#*$'\t'}"
      return 0
    fi
    kill -0 "$CLIENT_PID" 2> /dev/null || {
      echo "ERR production shell client exited waiting for overlay paint order" >&2
      return 1
    }
    sleep 0.025
  done
  echo "ERR timed out waiting for overlay paint order expected_visible=$expected_visible" >&2
  return 1
}

start_root_pointer_sequence() {
  coproc ROOT_POINTER {
    wayland_env "$OUT/virtual-pointer-input" sequence "$LOGICAL_WIDTH" "$LOGICAL_HEIGHT"
  }
  POINTER_PID="$ROOT_POINTER_PID"
  POINTER_READ_FD="${ROOT_POINTER[0]}"
  POINTER_WRITE_FD="${ROOT_POINTER[1]}"
  IFS= read -r -t 5 -u "$POINTER_READ_FD" POINTER_READY
  [[ "$POINTER_READY" == ready* ]]
}

send_root_pointer() {
  local action="$1"
  local command="$2"
  local started_ns ended_ns ack
  INPUT_INDEX=$((INPUT_INDEX + 1))
  INPUT_LOG_CURSOR="$(wc -l < "$CLIENT_LOG")"
  started_ns="$(date +%s%N)"
  printf '%s\n' "$command" >&"$POINTER_WRITE_FD"
  IFS= read -r -t 5 -u "$POINTER_READ_FD" ack
  ended_ns="$(date +%s%N)"
  [[ "$ack" == ack*"action=${command%% *}"* ]]
  INPUT_DURATION_MS=$(((ended_ns - started_ns) / 1000000))
  record_input virtual-pointer "$action" "$command" \
    "$INPUT_DURATION_MS" "$INPUT_LOG_CURSOR" "$ack"
}

exchange_root_pointer() {
  local command="$1"
  local ack
  printf '%s\n' "$command" >&"$POINTER_WRITE_FD"
  IFS= read -r -t 5 -u "$POINTER_READ_FD" ack
  [[ "$ack" == ack*"action=${command%% *}"* ]]
  printf '%s' "$ack"
}

prepare_window_capture_gate_geometry() {
  local intended_width="$1"
  local intended_height="$2"
  local fullscreen="$3"
  local rect geometry_line
  local -a geometry
  rect="$(window_rect_json)"
  geometry_line="$(rg "accepted synchronized layout .*target_rects=\\[.*${SLOW_PANE}:" "$CLIENT_LOG" | tail -1)"
  [[ -n "$geometry_line" ]]
  mapfile -t geometry < <(python3 tools/resize-window-gate-geometry.py \
    "$rect" "$geometry_line" "$SLOW_PANE" "$OUTPUT_SCALE" \
    "$intended_width" "$intended_height" "$fullscreen")
  read -r WINDOW_GATE_POINT_X WINDOW_GATE_POINT_Y WINDOW_GATE_PULSE_X <<< "${geometry[0]}"
  WINDOW_GATE_CURRENT_PANE_RECT="${geometry[1]}"
  WINDOW_GATE_INTENDED_PANE_RECT="${geometry[2]}"
  WINDOW_GATE_CAPTURE_RECT="${geometry[3]}"
}

wait_window_gate_diagnostic() {
  local after_line="$1"
  local kind="$2"
  local epoch="$3"
  local result=""
  for _ in $(seq 1 400); do
    result="$(sed -n "$((after_line + 1)),\$p" "$CLIENT_LOG" \
      | rg -m1 "\\\"kind\\\":\\\"${kind}\\\".*\\\"epoch\\\":${epoch}([,}])" || true)"
    if [[ -n "$result" ]]; then
      WINDOW_GATE_DIAGNOSTIC_LINE="$(rg -n -F "$result" "$CLIENT_LOG" | tail -1 | cut -d: -f1)"
      WINDOW_GATE_DIAGNOSTIC_TEXT="$result"
      return 0
    fi
    kill -0 "$CLIENT_PID" 2> /dev/null || return 1
    sleep 0.01
  done
  echo "ERR timed out waiting for WindowSlow gate diagnostic kind=$kind epoch=$epoch" >&2
  return 1
}

capture_window_gated() {
  local label="$1"
  local trace_kind="$2"
  local trace_line="$3"
  local trace_text="$4"
  local input_index="$5"
  local epoch="$6"
  local held_line="$7"
  local accepted_line="$8"
  local accepted_observed_ns="$9"
  local held_observed_ns="${10}"
  local frame path rect x y width height
  local pulse_cursor pulse_ack started_ns ended_ns grim_started_ns grim_completed_ns
  local client_after_pulse
  rect="$WINDOW_GATE_CAPTURE_RECT"
  [[ -n "$rect" && "$rect" != null ]]
  read -r x y width height < <(jq -r '[.x,.y,.width,.height] | @tsv' <<< "$rect")
  printf -v frame '%03d' "$WINDOW_FRAME_INDEX"
  path="window-frames/frame-${frame}-${label}.png"

  wayland_env grim -l 0 -g "${x},${y} ${width}x${height}" "$OUT/$path" &
  CAPTURE_PID=$!
  grim_started_ns="$(date +%s%N)"
  kill -0 "$CAPTURE_PID" 2> /dev/null

  INPUT_INDEX=$((INPUT_INDEX + 1))
  pulse_cursor="$(wc -l < "$CLIENT_LOG")"
  started_ns="$(date +%s%N)"
  pulse_ack="$(exchange_root_pointer "motion $WINDOW_GATE_PULSE_X $WINDOW_GATE_POINT_Y")"
  ended_ns="$(date +%s%N)"
  INPUT_DURATION_MS=$(((ended_ns - started_ns) / 1000000))
  record_input virtual-pointer window-slow-gate-pulse \
    "motion $WINDOW_GATE_PULSE_X $WINDOW_GATE_POINT_Y" "$INPUT_DURATION_MS" "$pulse_cursor" "$pulse_ack"

  wait "$CAPTURE_PID"
  grim_completed_ns="$(date +%s%N)"
  release_window_capture_gate "$epoch"
  CAPTURE_PID=""
  [[ -s "$OUT/$path" ]]
  client_after_pulse="$(wc -l < "$CLIENT_LOG")"
  if sed -n "$((pulse_cursor + 1)),${client_after_pulse}p" "$CLIENT_LOG" \
    | rg -q 'accepted synchronized layout |resize-proof-window-gate-replaced'; then
    echo "ERR WindowSlow capture pulse changed synchronization or replaced the held target" >&2
    return 1
  fi
  if sed -n "$((held_line + 1)),${client_after_pulse}p" "$CLIENT_LOG" \
    | rg -q "synchronized layout ready .* epoch=${epoch} .*degraded=true"; then
    echo "ERR WindowSlow capture pulse missed the synchronized deadline epoch=$epoch" >&2
    return 1
  fi
  record_capture "$WINDOW_MANIFEST" window "$label" "$path" \
    "$trace_kind" "$trace_line" "$trace_text" "$input_index" "$rect"
  WINDOW_FRAME_INDEX=$((WINDOW_FRAME_INDEX + 1))

  jq -cn \
    --argjson epoch "$epoch" \
    --argjson accepted_log_line "$accepted_line" \
    --argjson held_log_line "$held_line" \
    --argjson accepted_observed_ns "$accepted_observed_ns" \
    --argjson held_observed_ns "$held_observed_ns" \
    --argjson accept_to_held_us "$(((held_observed_ns - accepted_observed_ns) / 1000))" \
    --argjson held_to_pulse_us "$(((ended_ns - held_observed_ns) / 1000))" \
    --argjson grim_started_ns "$grim_started_ns" \
    --argjson pulse_ack_ns "$ended_ns" \
    --argjson grim_completed_ns "$grim_completed_ns" \
    --argjson gate_release_log_line "$WINDOW_GATE_DIAGNOSTIC_LINE" \
    --argjson current_pane_rect "$WINDOW_GATE_CURRENT_PANE_RECT" \
    --argjson intended_pane_rect "$WINDOW_GATE_INTENDED_PANE_RECT" \
    --argjson point_x "$WINDOW_GATE_POINT_X" \
    --argjson point_y "$WINDOW_GATE_POINT_Y" \
    --argjson pulse_x "$WINDOW_GATE_PULSE_X" \
    '{epoch:$epoch,accepted_log_line:$accepted_log_line,held_log_line:$held_log_line,
      accepted_observed_ns:$accepted_observed_ns,held_observed_ns:$held_observed_ns,
      accept_to_held_us:$accept_to_held_us,held_to_pulse_us:$held_to_pulse_us,
      grim_started_ns:$grim_started_ns,
      pulse_ack_ns:$pulse_ack_ns,grim_completed_ns:$grim_completed_ns,
      gate_release_log_line:$gate_release_log_line,current_pane_rect:$current_pane_rect,
      intended_pane_rect:$intended_pane_rect,point:[$point_x,$point_y],pulse_x:$pulse_x}' \
    >> "$WINDOW_GATE_EVENTS"
}

arm_window_capture_gate() {
  local intended_width="$1"
  local intended_height="$2"
  local fullscreen="$3"
  local cursor started_ns ended_ns motion_ack press_ack release_ack
  prepare_window_capture_gate_geometry "$intended_width" "$intended_height" "$fullscreen"
  cursor="$(wc -l < "$CLIENT_LOG")"
  INPUT_INDEX=$((INPUT_INDEX + 1))
  INPUT_LOG_CURSOR="$cursor"
  started_ns="$(date +%s%N)"
  start_root_pointer_sequence
  motion_ack="$(exchange_root_pointer "motion $WINDOW_GATE_POINT_X $WINDOW_GATE_POINT_Y")"
  press_ack="$(exchange_root_pointer press-right)"
  release_ack="$(exchange_root_pointer release-right)"
  ended_ns="$(date +%s%N)"
  INPUT_DURATION_MS=$(((ended_ns - started_ns) / 1000000))
  record_input virtual-pointer window-slow-gate-arm \
    "motion $WINDOW_GATE_POINT_X $WINDOW_GATE_POINT_Y; right-button-click" \
    "$INPUT_DURATION_MS" "$INPUT_LOG_CURSOR" \
    "$motion_ack | $press_ack | $release_ack"
  wait_window_gate_diagnostic "$cursor" resize-proof-window-gate-armed 0
  WINDOW_CAPTURE_GATE_ACTIVE=true
}

release_window_capture_gate() {
  local epoch="$1"
  local cursor started_ns ended_ns press_ack release_ack
  cursor="$(wc -l < "$CLIENT_LOG")"
  INPUT_INDEX=$((INPUT_INDEX + 1))
  INPUT_LOG_CURSOR="$cursor"
  started_ns="$(date +%s%N)"
  press_ack="$(exchange_root_pointer press)"
  release_ack="$(exchange_root_pointer release)"
  ended_ns="$(date +%s%N)"
  INPUT_DURATION_MS=$(((ended_ns - started_ns) / 1000000))
  record_input virtual-pointer window-slow-gate-release left-button-click \
    "$INPUT_DURATION_MS" "$INPUT_LOG_CURSOR" "$press_ack | $release_ack"
  wait_window_gate_diagnostic "$cursor" resize-proof-window-gate-released "$epoch"
  stop_root_pointer_sequence
  WINDOW_CAPTURE_GATE_ACTIVE=false
}

stop_root_pointer_sequence() {
  printf 'quit\n' >&"$POINTER_WRITE_FD"
  local completion
  IFS= read -r -t 5 -u "$POINTER_READ_FD" completion
  [[ "$completion" == done*"reason=quit"* ]]
  wait "$POINTER_PID"
  POINTER_PID=""
}

physical_extent() {
  python3 - "$1" "$2" "$OUTPUT_SCALE" <<'PY'
import sys
print(round(int(sys.argv[1]) * float(sys.argv[3])), round(int(sys.argv[2]) * float(sys.argv[3])))
PY
}

assert_no_ordinary_layout_between() {
  local start="$1"
  local end="$2"
  if sed -n "$((start + 1)),${end}p" "$CLIENT_LOG" \
    | rg -q 'accepted leased actor layout|SetResolvedLayout|set-resolved-layout'; then
    echo "ERR ordinary layout publication appeared during native synchronized settlement lines=$start..$end" >&2
    sed -n "$((start + 1)),${end}p" "$CLIENT_LOG" \
      | rg 'accepted leased actor layout|SetResolvedLayout|set-resolved-layout' >&2 || true
    return 1
  fi
}

append_transaction_event() {
  local lane="$1"
  local action="$2"
  local projected_line="$3"
  local projected_text="$4"
  local promoted_line="$5"
  local promoted_text="$6"
  local input_index="${7:-$INPUT_INDEX}"
  local native_settle_line=0
  if [[ "$lane" == window ]]; then
    native_settle_line="$NATIVE_SETTLE_LINE"
  fi
  jq -cn \
    --arg lane "$lane" \
    --arg action "$action" \
    --argjson input_index "$input_index" \
    --argjson diagnostic_line "$DIAGNOSTIC_LINE" \
    --argjson diagnostic "$DIAGNOSTIC_JSON" \
    --argjson accepted_line "$ACCEPTED_LINE" \
    --arg accepted_text "$ACCEPTED_TEXT" \
    --argjson projected_line "$projected_line" \
    --arg projected_text "$projected_text" \
    --argjson promoted_line "$promoted_line" \
    --arg promoted_text "$promoted_text" \
    --argjson completed_line "$TERMINAL_LINE" \
    --argjson native_settle_line "$native_settle_line" \
    '{lane:$lane,action:$action,input_index:$input_index,
      diagnostic_log_line:$diagnostic_line,diagnostic:$diagnostic,
      accepted:{log_line:$accepted_line,text:$accepted_text},
      projected:{log_line:$projected_line,text:$projected_text},
      promoted:{log_line:$promoted_line,text:$promoted_text},
      completed_log_line:$completed_line,
      native_settle_log_line:$native_settle_line}' \
    >> "$EVENTS"
}

assert_transaction_event_join() {
  local input_index="$1"
  local promoted_line="$2"
  jq -es --argjson input_index "$input_index" --argjson promoted_line "$promoted_line" '
    any(.[];
      .input_index == $input_index
      and .promoted.log_line == $promoted_line)
  ' "$EVENTS" > /dev/null
}

complete_native_transaction() {
  local action="$1"
  local after_line="$2"
  local expected_width="$3"
  local expected_height="$4"
  local capture_prefix="$5"
  local record_transaction="${6:-true}"
  local projected_line projected_text promoted_line promoted_text
  local common_promoted_line common_promoted_text
  local transaction_input_index="$INPUT_INDEX"
  local gated_transaction="$WINDOW_CAPTURE_GATE_ACTIVE"
  local accepted_observed_ns held_observed_ns

  wait_session_request "$after_line" nonzero native
  accepted_observed_ns="$(date +%s%N)"
  assert_diagnostic_matches_acceptance
  NATIVE_SETTLE_LINE="$(awk \
    -v start="$after_line" \
    -v stop="$ACCEPTED_LINE" \
    -v revision="$ACCEPTED_NATIVE_REVISION" '
      NR <= start || NR >= stop { next }
      index($0, "published settled native window resize ") != 0 \
        && index($0, "native_resize_revision=" revision " ") != 0 { line = NR }
      END { if (line > 0) print line }
    ' "$CLIENT_LOG")"
  if [[ -z "$NATIVE_SETTLE_LINE" ]]; then
    echo "ERR accepted native request has no preceding published settlement: $ACCEPTED_TEXT" >&2
    return 1
  fi
  wait_resize_trace "$ACCEPTED_EVENT_SEQUENCE" "$ACCEPTED_EPOCH" projected "$ACCEPTED_LINE"
  projected_line="$RESIZE_TRACE_LINE"
  projected_text="$RESIZE_TRACE_TEXT"
  if [[ "$record_transaction" == true ]]; then
    if [[ "$WINDOW_CAPTURE_GATE_ACTIVE" == true ]]; then
      wait_window_gate_diagnostic "$after_line" resize-proof-window-gate-held \
        "$ACCEPTED_EPOCH"
      held_observed_ns="$(date +%s%N)"
      capture_window_gated "${capture_prefix}-root-projected" resize-present \
        "$projected_line" "$projected_text" "$transaction_input_index" \
        "$ACCEPTED_EPOCH" "$WINDOW_GATE_DIAGNOSTIC_LINE" "$ACCEPTED_LINE" \
        "$accepted_observed_ns" "$held_observed_ns"
    else
      capture_window "${capture_prefix}-root-projected" resize-present \
        "$projected_line" "$projected_text" "$transaction_input_index"
    fi
  fi
  wait_window_trace 'promoted' "$expected_width" "$expected_height" "$projected_line"
  promoted_line="$WINDOW_TRACE_LINE"
  promoted_text="$WINDOW_TRACE_TEXT"
  if [[ "$record_transaction" == true ]]; then
    capture_window "${capture_prefix}-promoted" window-resize-present \
      "$promoted_line" "$promoted_text" "$transaction_input_index"
  fi
  wait_resize_trace "$ACCEPTED_EVENT_SEQUENCE" "$ACCEPTED_EPOCH" promoted "$projected_line"
  common_promoted_line="$RESIZE_TRACE_LINE"
  common_promoted_text="$RESIZE_TRACE_TEXT"
  wait_session_completed "$ACCEPTED_REQUEST" "$common_promoted_line"
  if [[ "$gated_transaction" == true ]]; then
    local ready_line ready_text ready_duration_us release_line
    ready_line="$(awk -v start="$projected_line" -v stop="$common_promoted_line" \
      -v epoch="$ACCEPTED_EPOCH" '
        NR > start && NR <= stop &&
          index($0, "synchronized layout ready ") != 0 &&
          index($0, " epoch=" epoch " ") != 0 { print NR; exit }
      ' "$CLIENT_LOG")"
    [[ -n "$ready_line" ]]
    ready_text="$(sed -n "${ready_line}p" "$CLIENT_LOG")"
    ready_duration_us="$(sed -n 's/.* duration_us=\([0-9][0-9]*\).*/\1/p' <<< "$ready_text")"
    release_line="$(tail -1 "$WINDOW_GATE_EVENTS" | jq -r '.gate_release_log_line')"
    if ! ((release_line < ready_line && ready_line <= common_promoted_line \
        && ready_duration_us < 250000)); then
      echo "ERR WindowSlow gate order/deadline invalid epoch=$ACCEPTED_EPOCH release=$release_line ready=$ready_line promoted=$common_promoted_line duration_us=$ready_duration_us" >&2
      return 1
    fi
    jq -cn \
      --argjson epoch "$ACCEPTED_EPOCH" \
      --argjson ready_log_line "$ready_line" \
      --argjson promoted_log_line "$common_promoted_line" \
      --argjson ready_duration_us "$ready_duration_us" \
      '{epoch:$epoch,ready_log_line:$ready_log_line,
        promoted_log_line:$promoted_log_line,ready_duration_us:$ready_duration_us}' \
      >> "$WINDOW_GATE_EVENTS"
  fi
  if ((TERMINAL_LINE >= promoted_line)); then
    echo "ERR native post-present ordering is not common-promoted < completed < window-promoted" >&2
    echo "common_promoted=$common_promoted_line completed=$TERMINAL_LINE window_promoted=$promoted_line" >&2
    return 1
  fi
  assert_no_ordinary_layout_between "$NATIVE_SETTLE_LINE" "$TERMINAL_LINE"
  if [[ "$record_transaction" == true ]]; then
    append_transaction_event window "$action" \
      "$projected_line" "$projected_text" "$common_promoted_line" "$common_promoted_text" \
      "$transaction_input_index"
  fi
}

run_single_native_resize() {
  local action="$1"
  local logical_width="$2"
  local logical_height="$3"
  local capture_prefix="$4"
  local expected_width expected_height live_line live_text
  read -r expected_width expected_height < <(physical_extent "$logical_width" "$logical_height")
  arm_window_capture_gate "$expected_width" "$expected_height" false
  send_sway_command "$action" \
    "[pid=$CLIENT_PID] resize set $logical_width $logical_height"
  wait_window_trace projected "$expected_width" "$expected_height" "$INPUT_LOG_CURSOR"
  live_line="$WINDOW_TRACE_LINE"
  live_text="$WINDOW_TRACE_TEXT"
  capture_window "${capture_prefix}-live-projected" window-resize-present \
    "$live_line" "$live_text" "$INPUT_INDEX"
  complete_native_transaction "$action" "$INPUT_LOG_CURSOR" \
    "$expected_width" "$expected_height" "$capture_prefix"
}

# The first configure can arrive before the production actor tree has claimed
# root and delegated its rest subtree. Treat that bounded coordinator fallback
# as initialization only. Once the complete topology is visible, require a
# fresh native transaction to pass the full root barrier before resetting the
# evidence streams and starting any captures.
send_sway_command startup-floating-enable "[pid=$CLIENT_PID] floating enable"
send_sway_command startup-floating-move "[pid=$CLIENT_PID] move position 100 48"
read -r STARTUP_READY_WIDTH STARTUP_READY_HEIGHT < <(physical_extent 900 520)
send_sway_command startup-readiness-resize \
  "[pid=$CLIENT_PID] resize set 900 520"
wait_window_trace projected "$STARTUP_READY_WIDTH" "$STARTUP_READY_HEIGHT" "$INPUT_LOG_CURSOR"
STARTUP_READINESS_LIVE_LINE="$WINDOW_TRACE_LINE"
complete_native_transaction startup-readiness "$INPUT_LOG_CURSOR" \
  "$STARTUP_READY_WIDTH" "$STARTUP_READY_HEIGHT" startup-readiness false
STARTUP_TARGET_RECT="$(window_rect_json)"
jq -n \
  --slurpfile sway_probe "$INPUT_LOG" \
  --arg initial_window_trace "$LAST_WINDOW_STARTUP_TRACE" \
  --argjson target_pid "$CLIENT_PID" \
  --argjson target_rect "$STARTUP_TARGET_RECT" \
  --argjson topology_ready_line "$ROOT_TREE_READY_LINE" \
  --argjson live_projected_line "$STARTUP_READINESS_LIVE_LINE" \
  --arg accepted "$ACCEPTED_TEXT" \
  --argjson accepted_line "$ACCEPTED_LINE" \
  --arg common_promoted "$RESIZE_TRACE_TEXT" \
  --argjson common_promoted_line "$RESIZE_TRACE_LINE" \
  --argjson completed_line "$TERMINAL_LINE" \
  --arg window_promoted "$WINDOW_TRACE_TEXT" \
  --argjson window_promoted_line "$WINDOW_TRACE_LINE" \
  '{verified:true,
    initial_pre_topology_window_trace:$initial_window_trace,
    sway_pid_target_probe:{pid:$target_pid,commands:$sway_probe,target_rect:$target_rect},
    topology_ready_line:$topology_ready_line,
    post_topology_native:{
      live_projected_line:$live_projected_line,
      accepted:{line:$accepted_line,text:$accepted},
      common_promoted:{line:$common_promoted_line,text:$common_promoted},
      completed_line:$completed_line,
      window_promoted:{line:$window_promoted_line,text:$window_promoted}
    }}' > "$STARTUP_READINESS"
jq -e '
  .verified == true
  and .sway_pid_target_probe.pid > 0
  and ([.sway_pid_target_probe.commands[].action]
      == ["startup-floating-enable","startup-floating-move","startup-readiness-resize"])
  and all(.sway_pid_target_probe.commands[];
      (.ack | fromjson | all(.[]; .success == true)))
  and .sway_pid_target_probe.target_rect.width == 900
  and .sway_pid_target_probe.target_rect.height == 520
  and .topology_ready_line < .post_topology_native.live_projected_line
  and .post_topology_native.live_projected_line
      < .post_topology_native.accepted.line
  and .post_topology_native.accepted.line
      < .post_topology_native.common_promoted.line
  and .post_topology_native.common_promoted.line
      < .post_topology_native.completed_line
  and .post_topology_native.completed_line
      < .post_topology_native.window_promoted.line
  and (.post_topology_native.accepted.text
      | test("native_resize_revision=[1-9][0-9]*"))
  and (.post_topology_native.common_promoted.text
      | contains("phase=promoted"))
  and (.post_topology_native.common_promoted.text
      | contains("degraded=false"))
  and (.post_topology_native.window_promoted.text
      | contains("phase=promoted"))
  and (.post_topology_native.window_promoted.text
      | contains("degraded=false"))
' "$STARTUP_READINESS" > /dev/null

# Only post-readiness input, events, and frames are proof evidence.
: > "$INPUT_LOG"
: > "$EVENTS"
INPUT_INDEX=0
INPUT_DURATION_MS=0
INPUT_LOG_CURSOR="$(wc -l < "$CLIENT_LOG")"
TEST_LOG_START_LINE="$INPUT_LOG_CURSOR"

# Establish the first captured floating baseline.
run_single_native_resize floating-baseline 816 480 window-baseline

# Continuous compositor resize: each IPC request is acknowledged, its live
# projected frame is captured, and the next configure is sent before the
# 120ms settlement window.  Only the final configure may reach the root actor.
NATIVE_BURST_START_LINE="$(wc -l < "$CLIENT_LOG")"
NATIVE_LIVE_INDEX=0
NATIVE_BURST_SEND_TIMES_MS=()
read -r GATE_TARGET_WIDTH GATE_TARGET_HEIGHT < <(physical_extent 760 456)
arm_window_capture_gate "$GATE_TARGET_WIDTH" "$GATE_TARGET_HEIGHT" false
for logical_extent in '776 448' '856 496' '760 456'; do
  read -r logical_width logical_height <<< "$logical_extent"
  NATIVE_LIVE_INDEX=$((NATIVE_LIVE_INDEX + 1))
  printf -v live_label '%02d' "$NATIVE_LIVE_INDEX"
  read -r expected_width expected_height < <(physical_extent "$logical_width" "$logical_height")
  send_sway_command "native-live-$live_label" \
    "[pid=$CLIENT_PID] resize set $logical_width $logical_height"
  NATIVE_BURST_SEND_TIMES_MS+=("$LAST_SWAY_INPUT_STARTED_MS")
  wait_window_trace projected "$expected_width" "$expected_height" "$INPUT_LOG_CURSOR"
  capture_window_deferred "window-live-${live_label}-projected" window-resize-present \
    "$WINDOW_TRACE_LINE" "$WINDOW_TRACE_TEXT" "$INPUT_INDEX"
done
NATIVE_BURST_INPUT_CURSOR="$INPUT_LOG_CURSOR"
NATIVE_BURST_INPUT_SPACINGS_MS=()
NATIVE_BURST_MAX_INPUT_SPACING_MS=0
for ((burst_index = 1;
     burst_index < ${#NATIVE_BURST_SEND_TIMES_MS[@]};
     burst_index++)); do
  burst_spacing_ms=$((
    NATIVE_BURST_SEND_TIMES_MS[burst_index]
    - NATIVE_BURST_SEND_TIMES_MS[burst_index - 1]
  ))
  NATIVE_BURST_INPUT_SPACINGS_MS+=("$burst_spacing_ms")
  if ((burst_spacing_ms > NATIVE_BURST_MAX_INPUT_SPACING_MS)); then
    NATIVE_BURST_MAX_INPUT_SPACING_MS="$burst_spacing_ms"
  fi
  if ((burst_spacing_ms >= 120)); then
    echo "ERR native configure burst spacing ${burst_spacing_ms}ms reached the 120ms settlement window" >&2
    exit 1
  fi
done
NATIVE_BURST_ELAPSED_MS=$((
  NATIVE_BURST_SEND_TIMES_MS[${#NATIVE_BURST_SEND_TIMES_MS[@]} - 1]
  - NATIVE_BURST_SEND_TIMES_MS[0]
))
NATIVE_BURST_INPUT_SPACINGS_JSON="$({
  printf '%s\n' "${NATIVE_BURST_INPUT_SPACINGS_MS[@]}"
} | jq -s 'map(tonumber)')"

# PNG completion is the input/capture barrier.  Dimension reads, hashing, and
# JSONL enrichment are intentionally deferred until all three configures have
# been sent and the final transaction captured, so bookkeeping can neither
# trigger an intermediate settlement nor hide the final projected surface.
complete_native_transaction native-live-final "$NATIVE_BURST_INPUT_CURSOR" \
  "$expected_width" "$expected_height" window-live-final
flush_deferred_window_captures

NATIVE_BURST_REQUESTS="$(sed -n "$((NATIVE_BURST_START_LINE + 1)),\$p" "$CLIENT_LOG" \
  | rg -c 'accepted synchronized layout actor=session-switcher .*native_resize_revision=[1-9]' || true)"
NATIVE_BURST_PUBLICATIONS="$(sed -n "$((NATIVE_BURST_START_LINE + 1)),${ACCEPTED_LINE}p" "$CLIENT_LOG" \
  | rg -c 'published settled native window resize native_resize_revision=[1-9]' || true)"
if [[ "$NATIVE_BURST_REQUESTS" -ne 1 || "$NATIVE_BURST_PUBLICATIONS" -ne 1 ]]; then
  echo "ERR native configure burst must publish and accept exactly one final root transaction" >&2
  exit 1
fi

# Return to the full output before exercising root-owned sidebars.  Fullscreen
# is itself a native configure and therefore goes through the same final root
# synchronization path.
arm_window_capture_gate "$MODE_WIDTH" "$MODE_HEIGHT" true
send_sway_command fullscreen-enable "[pid=$CLIENT_PID] fullscreen enable"
FULLSCREEN_TRANSACTION_INPUT_INDEX="$INPUT_INDEX"
FULLSCREEN_CURSOR="$INPUT_LOG_CURSOR"
wait_window_trace projected "$MODE_WIDTH" "$MODE_HEIGHT" "$FULLSCREEN_CURSOR"
capture_window window-restore-live-projected window-resize-present \
  "$WINDOW_TRACE_LINE" "$WINDOW_TRACE_TEXT" "$FULLSCREEN_TRANSACTION_INPUT_INDEX"
complete_native_transaction fullscreen-restore "$FULLSCREEN_CURSOR" \
  "$MODE_WIDTH" "$MODE_HEIGHT" window-restore

# The restored promoted frame is also the stable initial shell composition.
assert_transaction_event_join \
  "$FULLSCREEN_TRANSACTION_INPUT_INDEX" "$RESIZE_TRACE_LINE"
capture_sidebar sidebar-initial "$RESIZE_TRACE_LINE" "$RESIZE_TRACE_TEXT" \
  "$FULLSCREEN_TRANSACTION_INPUT_INDEX"

run_sidebar_transition() {
  local action="$1"
  local reason="$2"
  local keycode="$3"
  local capture_prefix="$4"
  local projected_line projected_text promoted_line promoted_text

  send_shell_key "$action" "$keycode"
  wait_session_request "$INPUT_LOG_CURSOR" zero "$reason"
  assert_diagnostic_matches_acceptance
  wait_resize_trace "$ACCEPTED_EVENT_SEQUENCE" "$ACCEPTED_EPOCH" projected "$ACCEPTED_LINE"
  projected_line="$RESIZE_TRACE_LINE"
  projected_text="$RESIZE_TRACE_TEXT"
  capture_sidebar "${capture_prefix}-projected" \
    "$projected_line" "$projected_text" "$INPUT_INDEX"
  wait_resize_trace "$ACCEPTED_EVENT_SEQUENCE" "$ACCEPTED_EPOCH" promoted "$projected_line"
  promoted_line="$RESIZE_TRACE_LINE"
  promoted_text="$RESIZE_TRACE_TEXT"
  capture_sidebar "${capture_prefix}-promoted" \
    "$promoted_line" "$promoted_text" "$INPUT_INDEX"
  wait_session_completed "$ACCEPTED_REQUEST" "$promoted_line"
  append_transaction_event sidebar "$action" \
    "$projected_line" "$projected_text" "$promoted_line" "$promoted_text"
}

# Linux evdev KEY_M=50 and KEY_I=23.  The helper's default modifier set is
# Ctrl+Alt and its final wl_display_roundtrip is the compositor input ACK.
run_sidebar_transition sidebar-hide sidebar 50 sidebar-hide
run_sidebar_transition sidebar-show sidebar 50 sidebar-show
run_sidebar_transition inbox-show inbox 23 inbox-show
run_sidebar_transition sidebar-hide-with-inbox sidebar 50 sidebar-hide-with-inbox
run_sidebar_transition sidebar-show-with-inbox sidebar 50 sidebar-show-with-inbox

complete_shell_pointer_transaction() {
  local action="$1"
  local reason="$2"
  local capture_prefix="$3"
  local projected_line projected_text promoted_line promoted_text

  wait_session_request "$INPUT_LOG_CURSOR" zero "$reason"
  assert_diagnostic_matches_acceptance
  wait_resize_trace "$ACCEPTED_EVENT_SEQUENCE" "$ACCEPTED_EPOCH" projected "$ACCEPTED_LINE"
  projected_line="$RESIZE_TRACE_LINE"
  projected_text="$RESIZE_TRACE_TEXT"
  capture_sidebar "${capture_prefix}-projected" \
    "$projected_line" "$projected_text" "$INPUT_INDEX"
  wait_resize_trace "$ACCEPTED_EVENT_SEQUENCE" "$ACCEPTED_EPOCH" promoted "$projected_line"
  promoted_line="$RESIZE_TRACE_LINE"
  promoted_text="$RESIZE_TRACE_TEXT"
  capture_sidebar "${capture_prefix}-promoted" \
    "$promoted_line" "$promoted_text" "$INPUT_INDEX"
  wait_session_completed "$ACCEPTED_REQUEST" "$promoted_line"
  append_transaction_event sidebar "$action" \
    "$projected_line" "$projected_text" "$promoted_line" "$promoted_text"
}

root_divider_points() {
  local divider="$1"
  local delta="$2"
  python3 - "$DIAGNOSTIC_JSON" "$divider" "$delta" "$OUTPUT_SCALE" \
    "$LOGICAL_WIDTH" "$LOGICAL_HEIGHT" <<'PY'
import json
import sys

geometry = json.loads(sys.argv[1])
divider = sys.argv[2]
delta = float(sys.argv[3])
scale = float(sys.argv[4])
logical_width = int(sys.argv[5])
logical_height = int(sys.argv[6])
rest = float(geometry["rest_width"])
sidebar = float(geometry["sidebar_width"])
start = rest if divider == "sidebar" else rest + sidebar
end = start + delta
x1 = min(max(round(start / scale), 1), logical_width - 1)
x2 = min(max(round(end / scale), 1), logical_width - 1)
y = min(max(round(float(geometry["rest_height"]) * 0.5 / scale), 1), logical_height - 1)
print(x1, x2, y)
PY
}

run_root_divider_drag() {
  local divider="$1"
  local delta="$2"
  local reason="${divider}-resize"
  local start_x end_x pointer_y
  read -r start_x end_x pointer_y < <(root_divider_points "$divider" "$delta")
  start_root_pointer_sequence
  send_root_pointer "${divider}-divider-hover" "motion $start_x $pointer_y"
  send_root_pointer "${divider}-divider-press" press
  send_root_pointer "${divider}-divider-motion" "motion $end_x $pointer_y"
  complete_shell_pointer_transaction \
    "${divider}-divider-motion" "$reason" "${divider}-divider-motion"
  send_root_pointer "${divider}-divider-release" release
  complete_shell_pointer_transaction \
    "${divider}-divider-release" "$reason" "${divider}-divider-release"
  stop_root_pointer_sequence
}

# With the delegated main region, session switcher, and inbox all visible,
# move each root-owned boundary independently. Negative movement expands the
# switcher into the main region; positive movement expands it into the inbox.
run_root_divider_drag sidebar -96
run_root_divider_drag inbox 80

kill -0 "$CLIENT_PID"

UNEXPECTED_ROOT_TERMINALS="$(sed -n "$((TEST_LOG_START_LINE + 1)),\$p" "$CLIENT_LOG" \
  | rg 'rejected synchronized layout actor=session-switcher |degraded synchronized layout actor=session-switcher ' \
  || true)"
if [[ -n "$UNEXPECTED_ROOT_TERMINALS" ]]; then
  echo "ERR production shell root transaction rejected or degraded" >&2
  printf '%s\n' "$UNEXPECTED_ROOT_TERMINALS" >&2
  exit 1
fi

if ! python3 tools/analyze-resize-overhaul-window-shell.py \
  "$OUT" \
  "$WINDOW_MANIFEST" \
  "$SIDEBAR_MANIFEST" \
  "$EVENTS" \
  "$INPUT_LOG" \
  "$CLIENT_LOG" \
  "$WINDOW_GATE_EVENTS" \
  "$OUTPUT_SCALE" \
  "${MODE_WIDTH}x${MODE_HEIGHT}" \
  > "$METRICS" 2>> "$ANALYSIS_LOG"; then
  cat "$ANALYSIS_LOG" >&2
  exit 1
fi

# Exercise both production overlay actors after the fixed capture set has been
# analyzed. Each actor must publish an explicit OverlayPane that the renderer
# places after the applicable native dividers. Closing it must remove that
# overlay classification again.
OVERLAY_PROBES="$OUT/overlay-probes.jsonl"
: > "$OVERLAY_PROBES"
probe_explicit_overlay() {
  local name="$1"
  local modifier_mode="$2"
  local keycode="$3"
  local reason="$4"
  local open_request open_accepted_line open_promoted_line open_terminal_line
  local visible_paint_line visible_paint_text hidden_paint_line hidden_paint_text
  local close_request close_accepted_line close_promoted_line close_terminal_line

  send_overlay_probe_key "$modifier_mode" "$keycode"
  wait_session_request "$INPUT_LOG_CURSOR" zero "$reason"
  open_request="$ACCEPTED_REQUEST"
  open_accepted_line="$ACCEPTED_LINE"
  wait_resize_trace "$ACCEPTED_EVENT_SEQUENCE" "$ACCEPTED_EPOCH" projected "$ACCEPTED_LINE"
  wait_resize_trace "$ACCEPTED_EVENT_SEQUENCE" "$ACCEPTED_EPOCH" promoted "$RESIZE_TRACE_LINE"
  open_promoted_line="$RESIZE_TRACE_LINE"
  wait_session_completed "$ACCEPTED_REQUEST" "$RESIZE_TRACE_LINE"
  open_terminal_line="$TERMINAL_LINE"
  wait_overlay_paint_order "$open_accepted_line" 1
  visible_paint_line="$OVERLAY_PAINT_LINE"
  visible_paint_text="$OVERLAY_PAINT_TEXT"
  wayland_env grim -l 0 -o "$OUTPUT_NAME" "$OVERLAY_FRAMES/${name}-visible.png"
  [[ -s "$OVERLAY_FRAMES/${name}-visible.png" ]]

  send_overlay_probe_key --none 1
  wait_session_request "$INPUT_LOG_CURSOR" zero "$reason"
  close_request="$ACCEPTED_REQUEST"
  close_accepted_line="$ACCEPTED_LINE"
  wait_resize_trace "$ACCEPTED_EVENT_SEQUENCE" "$ACCEPTED_EPOCH" projected "$ACCEPTED_LINE"
  wait_resize_trace "$ACCEPTED_EVENT_SEQUENCE" "$ACCEPTED_EPOCH" promoted "$RESIZE_TRACE_LINE"
  close_promoted_line="$RESIZE_TRACE_LINE"
  wait_session_completed "$ACCEPTED_REQUEST" "$RESIZE_TRACE_LINE"
  close_terminal_line="$TERMINAL_LINE"
  wait_overlay_paint_order "$close_accepted_line" 0
  hidden_paint_line="$OVERLAY_PAINT_LINE"
  hidden_paint_text="$OVERLAY_PAINT_TEXT"

  jq -cn \
    --arg actor "$name" \
    --argjson open_request "$open_request" \
    --argjson open_accepted_line "$open_accepted_line" \
    --argjson open_promoted_line "$open_promoted_line" \
    --argjson open_terminal_line "$open_terminal_line" \
    --argjson visible_paint_line "$visible_paint_line" \
    --arg visible_paint "$visible_paint_text" \
    --argjson close_request "$close_request" \
    --argjson close_accepted_line "$close_accepted_line" \
    --argjson close_promoted_line "$close_promoted_line" \
    --argjson close_terminal_line "$close_terminal_line" \
    --argjson hidden_paint_line "$hidden_paint_line" \
    --arg hidden_paint "$hidden_paint_text" \
    '{actor:$actor,
      open:{request:$open_request,accepted_line:$open_accepted_line,
        promoted_line:$open_promoted_line,terminal_line:$open_terminal_line,
        paint_line:$visible_paint_line,paint:$visible_paint},
      close:{request:$close_request,accepted_line:$close_accepted_line,
        promoted_line:$close_promoted_line,terminal_line:$close_terminal_line,
        paint_line:$hidden_paint_line,paint:$hidden_paint}}' \
    >> "$OVERLAY_PROBES"
}

# Linux evdev KEY_P=25, KEY_SLASH=53, and KEY_ESC=1.
probe_explicit_overlay picker --ctrl-alt 25 picker
probe_explicit_overlay keybindings --ctrl-shift-only 53 keybindings

python3 - "$OVERLAY_PROBES" > "$OVERLAY_METRICS" <<'PY'
import json
import pathlib
import re
import sys

records = [
    json.loads(line)
    for line in pathlib.Path(sys.argv[1]).read_text().splitlines()
    if line
]
paint_pattern = re.compile(
    r"resize-paint-order ordinary_panes=(?P<ordinary>\d+) "
    r"dividers=(?P<dividers>\d+) explicit_overlay_panes=(?P<overlay>\d+) "
    r"elevated_overlay_panes=(?P<elevated>\d+) "
    r"low_z_divider_over_ordinary=(?:true|false)$"
)
if [record["actor"] for record in records] != ["picker", "keybindings"]:
    raise SystemExit(f"wrong production overlay probes: {records}")
for record in records:
    visible = paint_pattern.fullmatch(record["open"]["paint"])
    hidden = paint_pattern.fullmatch(record["close"]["paint"])
    if visible is None or hidden is None:
        raise SystemExit(f"invalid overlay paint trace: {record}")
    if int(visible["overlay"]) < 1 or int(visible["elevated"]) < 1:
        raise SystemExit(f"visible actor was not elevated above divider chrome: {record}")
    if int(hidden["overlay"]) != 0 or int(hidden["elevated"]) != 0:
        raise SystemExit(f"closed actor remained in the paint partition: {record}")
    for phase in ("open", "close"):
        evidence = record[phase]
        if not (
            evidence["accepted_line"] < evidence["promoted_line"]
            < evidence["terminal_line"]
        ):
            raise SystemExit(f"overlay transaction order is invalid: {record}")

print(json.dumps({
    "verified": True,
    "actors": records,
    "assertions": {
        "picker_is_explicit_elevated_overlay": True,
        "keybindings_is_explicit_elevated_overlay": True,
        "overlay_close_removes_paint_partition": True,
    },
}, indent=2, sort_keys=True))
PY

mapfile -t WINDOW_FRAME_FILES < <(
  find "$WINDOW_FRAMES" -maxdepth 1 -name '*.png' -type f | sort
)
mapfile -t SIDEBAR_FRAME_FILES < <(
  find "$SIDEBAR_FRAMES" -maxdepth 1 -name '*.png' -type f | sort
)
if ((${#WINDOW_FRAME_FILES[@]} > 0)); then
  magick montage \
    -font DejaVu-Sans \
    -label '%f' \
    "${WINDOW_FRAME_FILES[@]}" \
    -thumbnail 320x180 \
    -tile 4x \
    -geometry +4+4 \
    "$OUT/window-contact-sheet.png"
  magick -delay 32 -loop 0 "${WINDOW_FRAME_FILES[@]}" \
    -resize 50% "$OUT/window-resize.gif"
fi
if ((${#SIDEBAR_FRAME_FILES[@]} > 0)); then
  magick montage \
    -font DejaVu-Sans \
    -label '%f' \
    "${SIDEBAR_FRAME_FILES[@]}" \
    -thumbnail 320x180 \
    -tile 4x \
    -geometry +4+4 \
    "$OUT/sidebar-contact-sheet.png"
  magick -delay 32 -loop 0 "${SIDEBAR_FRAME_FILES[@]}" \
    -resize 50% "$OUT/sidebar-resize.gif"
fi

jq -n \
  --slurpfile metrics "$METRICS" \
  --slurpfile startup "$STARTUP_READINESS" \
  --slurpfile overlays "$OVERLAY_METRICS" \
  --arg output_name "$OUTPUT_NAME" \
  --argjson output_scale "$OUTPUT_SCALE" \
  --argjson native_burst_elapsed_ms "$NATIVE_BURST_ELAPSED_MS" \
  --argjson native_burst_input_spacings_ms "$NATIVE_BURST_INPUT_SPACINGS_JSON" \
  --argjson native_burst_max_input_spacing_ms "$NATIVE_BURST_MAX_INPUT_SPACING_MS" \
  --argjson native_burst_root_requests "$NATIVE_BURST_REQUESTS" \
  --argjson native_burst_publications "$NATIVE_BURST_PUBLICATIONS" \
  '{
    kind:"brood-resize-overhaul-window-shell-nested",
    verified:true,
    output:{name:$output_name,scale:$output_scale},
    metrics:$metrics[0],
    startup_readiness:$startup[0],
    overlay_panes:$overlays[0],
    native_configure_burst:{
      captured_steps:3,
      elapsed_ms:$native_burst_elapsed_ms,
      input_spacings_ms:$native_burst_input_spacings_ms,
      max_input_spacing_ms:$native_burst_max_input_spacing_ms,
      root_transaction_count:$native_burst_root_requests,
      native_publication_count:$native_burst_publications,
      settled_once_at_final_configure:
        ($native_burst_root_requests == 1 and $native_burst_publications == 1)
    },
    assertions:($metrics[0].assertions + {
      production_shell_claims_root:true,
      workspace_owns_only_recursive_rest:true,
      post_topology_native_readiness:$startup[0].verified,
      native_configure_burst_settles_once:
        ($native_burst_root_requests == 1 and $native_burst_publications == 1),
      native_configure_burst_input_spacing_bounded:
        ($native_burst_max_input_spacing_ms < 120),
      native_configure_burst_has_no_rejections:true,
      no_ordinary_layout_during_native_completion:true,
      picker_is_explicit_elevated_overlay:
        $overlays[0].assertions.picker_is_explicit_elevated_overlay,
      keybindings_is_explicit_elevated_overlay:
        $overlays[0].assertions.keybindings_is_explicit_elevated_overlay,
      overlay_close_removes_paint_partition:
        $overlays[0].assertions.overlay_close_removes_paint_partition
    }),
    artifacts:{
      window_frame_manifest:"window-frame-manifest.jsonl",
      sidebar_frame_manifest:"sidebar-frame-manifest.jsonl",
      resize_events:"resize-events.jsonl",
      input:"input.jsonl",
      window_gate_events:"window-gate-events.jsonl",
      metrics:"metrics.json",
      client_log:"client.log",
      startup_readiness:"startup-readiness.json",
      overlay_pane_metrics:"overlay-pane-metrics.json",
      overlay_frames:"overlay-frames",
      window_contact_sheet:"window-contact-sheet.png",
      window_animation:"window-resize.gif",
      sidebar_contact_sheet:"sidebar-contact-sheet.png",
      sidebar_animation:"sidebar-resize.gif"
    }
  }' > "$PROOF"

jq -e '
  .verified == true
  and ([.assertions[]] | all(. == true))
  and .metrics.captures.window == 11
  and .metrics.captures.sidebar == 19
  and .metrics.captures.mixed_waited_pairs == 0
  and .metrics.max_staged_count <= 1
  and .native_configure_burst.captured_steps == 3
  and .native_configure_burst.root_transaction_count == 1
  and .native_configure_burst.native_publication_count == 1
  and .native_configure_burst.max_input_spacing_ms < 120
  and .output.scale == 1.25
' "$PROOF" > /dev/null

kill "$CLIENT_PID" > /dev/null 2>&1 || true
wait "$CLIENT_PID" > /dev/null 2>&1 || true
CLIENT_PID=""

echo "resize_window_shell_nested=verified"
echo "proof=$PROOF"
echo "metrics=$METRICS"
echo "window_manifest=$WINDOW_MANIFEST"
echo "sidebar_manifest=$SIDEBAR_MANIFEST"
echo "events=$EVENTS"
