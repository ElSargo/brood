#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

DURATION_SECONDS="${MANIFOLD_PROBE_DURATION_SECONDS:-10}"
SAMPLE_INTERVAL_SECONDS="${MANIFOLD_PROBE_SAMPLE_INTERVAL_SECONDS:-0.05}"
CONTROL_SOCKET="${MANIFOLD_PROBE_CONTROL_SOCKET:-}"
OUTPUT_DIR="${MANIFOLD_PROBE_OUTPUT_DIR:-}"
CLIENT_LOG="${MANIFOLD_PROBE_CLIENT_LOG:-}"
WAYLAND_TRACE="${MANIFOLD_PROBE_WAYLAND_TRACE:-}"
MANIFOLDD_LOG="${MANIFOLD_PROBE_MANIFOLDD_LOG:-}"
MANIFOLDD_FEATURES="${MANIFOLD_PROBE_MANIFOLDD_FEATURES:-}"
CLIENT=()

usage() {
  cat <<'USAGE'
Usage: tools/manifold-behavior-probe.sh [--duration SECONDS] [--sample-interval SECONDS] [--out-dir DIR] [--] [CLIENT [ARGS...]]

Runs manifoldd with a strict dmabuf-capable client, samples status-json, captures
the client's WAYLAND_DEBUG trace, and emits JSONL behavior events to stdout.

Defaults to: vkcube --wsi wayland --c 120000 --width 640 --height 360

Environment overrides:
  MANIFOLD_PROBE_DURATION_SECONDS
  MANIFOLD_PROBE_SAMPLE_INTERVAL_SECONDS
  MANIFOLD_PROBE_OUTPUT_DIR
  MANIFOLD_PROBE_CONTROL_SOCKET
  MANIFOLD_PROBE_CLIENT_LOG
  MANIFOLD_PROBE_WAYLAND_TRACE
  MANIFOLD_PROBE_MANIFOLDD_LOG
  MANIFOLD_PROBE_MANIFOLDD_FEATURES
USAGE
}

while (($#)); do
  case "$1" in
    --duration)
      DURATION_SECONDS="$2"
      shift 2
      ;;
    --sample-interval)
      SAMPLE_INTERVAL_SECONDS="$2"
      shift 2
      ;;
    --out-dir)
      OUTPUT_DIR="$2"
      shift 2
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    --)
      shift
      CLIENT=("$@")
      break
      ;;
    *)
      CLIENT=("$@")
      break
      ;;
  esac
done

if ((${#CLIENT[@]} == 0)); then
  if ! command -v vkcube >/dev/null 2>&1; then
    echo "ERR default probe client vkcube is unavailable; pass a client command after --" >&2
    exit 1
  fi
  CLIENT=(vkcube --wsi wayland --c 120000 --width 640 --height 360)
fi

if [[ -d "$ROOT/.dev/lib" && -z "${RUSTFLAGS:-}" ]]; then
  export RUSTFLAGS="-L native=$ROOT/.dev/lib"
fi

BUILD_ARGS=(cargo build -p manifoldd --offline)
if [[ -n "$MANIFOLDD_FEATURES" ]]; then
  BUILD_ARGS+=(--features "$MANIFOLDD_FEATURES")
fi
"${BUILD_ARGS[@]}" >/dev/null

if [[ -z "$OUTPUT_DIR" ]]; then
  OUTPUT_DIR="$(mktemp -d /tmp/manifold-behavior-probe.XXXXXX)"
else
  mkdir -p "$OUTPUT_DIR"
fi
CONTROL_SOCKET="${CONTROL_SOCKET:-$OUTPUT_DIR/control.sock}"
CLIENT_LOG="${CLIENT_LOG:-$OUTPUT_DIR/client.log}"
WAYLAND_TRACE="${WAYLAND_TRACE:-$OUTPUT_DIR/client-wayland.log}"
MANIFOLDD_LOG="${MANIFOLDD_LOG:-$OUTPUT_DIR/manifoldd.log}"
WRAPPER="$OUTPUT_DIR/client-wrapper.sh"

json_string() {
  python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$1"
}

emit_json() {
  python3 - "$@" <<'PY'
import json
import sys
print(json.dumps(json.loads(sys.argv[1]), separators=(",", ":")), flush=True)
PY
}

cat >"$WRAPPER" <<EOF
#!/usr/bin/env bash
exec env WAYLAND_DEBUG=1 $(printf ' %q' "${CLIENT[@]}") >$(printf '%q' "$CLIENT_LOG") 2>$(printf '%q' "$WAYLAND_TRACE")
EOF
chmod +x "$WRAPPER"

cleanup() {
  if [[ -n "${PID:-}" ]] && kill -0 "$PID" 2>/dev/null; then
    target/debug/manifoldd ctl --control-socket "$CONTROL_SOCKET" quit >/dev/null 2>&1 || true
    sleep 0.2
    kill "$PID" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

RUST_LOG="${RUST_LOG:-manifoldd=info,smithay=warn}" \
  target/debug/manifoldd run --control-socket "$CONTROL_SOCKET" -c "$WRAPPER" \
  >"$MANIFOLDD_LOG" 2>&1 &
PID=$!

for _ in $(seq 1 100); do
  [[ -S "$CONTROL_SOCKET" ]] && break
  kill -0 "$PID" 2>/dev/null || {
    sed -n '1,200p' "$MANIFOLDD_LOG" >&2 || true
    sed -n '1,200p' "$CLIENT_LOG" >&2 || true
    sed -n '1,200p' "$WAYLAND_TRACE" >&2 || true
    exit 1
  }
  sleep 0.1
done

START_NS="$(date +%s%N)"
LAST_STATUS=""
LAST_FRAMES=""
LAST_INPUTS=""
LAST_COMMITS=""
LAST_CLIENT_CALLBACKS=""
LAST_SAMPLE_NS="$START_NS"
TRACE_OFFSET=0

emit_event() {
  local event="$1"
  local payload="$2"
  local now_ns elapsed_us
  now_ns="$(date +%s%N)"
  elapsed_us="$(((now_ns - START_NS) / 1000))"
  python3 - "$event" "$elapsed_us" "$now_ns" "$payload" <<'PY'
import json
import sys
event, elapsed_us, unix_ns, payload = sys.argv[1:]
print(json.dumps({
    "event": event,
    "elapsed_us": int(elapsed_us),
    "unix_ns": int(unix_ns),
    "fields": json.loads(payload),
}, separators=(",", ":")), flush=True)
PY
}

emit_event "probe_started" "$(
  python3 - "$OUTPUT_DIR" "$CONTROL_SOCKET" "$MANIFOLDD_LOG" "$CLIENT_LOG" "$WAYLAND_TRACE" "${CLIENT[@]}" <<'PY'
import json
import sys
print(json.dumps({
    "output_dir": sys.argv[1],
    "control_socket": sys.argv[2],
    "manifoldd_log": sys.argv[3],
    "client_log": sys.argv[4],
    "wayland_trace": sys.argv[5],
    "client": sys.argv[6:],
}))
PY
)"

parse_trace_range() {
  local start="$1"
  local size="$2"
  python3 - "$WAYLAND_TRACE" "$start" "$size" "$START_NS" <<'PY'
import json
import re
import sys
import time

path, start, size, probe_start_ns = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4])
object = r"[@#]\d+"
configure_re = re.compile(rf'xdg_toplevel{object}\.configure\(([-0-9]+), ([-0-9]+),')
wl_surface_frame_re = re.compile(rf'wl_surface{object}\.frame\(new id wl_callback{object}\)')
wl_callback_done_re = re.compile(rf'wl_callback{object}\.done\(([-0-9]+)\)')
commit_re = re.compile(rf'wl_surface{object}\.commit\(\)')

with open(path, "rb") as handle:
    handle.seek(start)
    data = handle.read(size - start).decode("utf-8", errors="replace")

for line in data.splitlines():
    event = None
    fields = {"trace": line}
    if match := configure_re.search(line):
        event = "window_size_request"
        fields.update({"width": int(match.group(1)), "height": int(match.group(2))})
    elif wl_surface_frame_re.search(line):
        event = "client_frame_requested"
    elif match := wl_callback_done_re.search(line):
        event = "client_frame_callback_done"
        fields["callback_time_ms"] = int(match.group(1))
    elif commit_re.search(line):
        event = "client_surface_commit"
    if event:
        unix_ns = time.time_ns()
        print(json.dumps({
            "event": event,
            "elapsed_us": (unix_ns - probe_start_ns) // 1000,
            "unix_ns": unix_ns,
            "fields": fields,
        }, separators=(",", ":")))
PY
}

while true; do
  NOW_NS="$(date +%s%N)"
  if (( (NOW_NS - START_NS) / 1000000000 >= DURATION_SECONDS )); then
    break
  fi

  STATUS="$(target/debug/manifoldd ctl --control-socket "$CONTROL_SOCKET" status-json)"
  SAMPLE_NS="$(date +%s%N)"
  SAMPLE_DELTA_US="$(((SAMPLE_NS - LAST_SAMPLE_NS) / 1000))"
  python3 - "$STATUS" "$LAST_STATUS" "$SAMPLE_DELTA_US" <<'PY' | while IFS= read -r event_line; do
import json
import sys

status = json.loads(sys.argv[1])
previous = json.loads(sys.argv[2]) if sys.argv[2] else None
sample_delta_us = int(sys.argv[3])

def emit(event, fields):
    print(json.dumps({"event": event, "fields": fields}, separators=(",", ":")))

fields = {
    "presentation": status["presentation"],
    "windows": status["windows"],
    "frames": status["frames"],
    "commits": status["commits"],
    "client_frame_callbacks": status["client_frame_callbacks"],
    "input_events": status["input_events"],
    "last_frame_us": status["last_frame_us"],
    "last_frame_interval_us": status["last_frame_interval_us"],
    "max_frame_interval_us": status["max_frame_interval_us"],
    "last_input_to_frame_us": status["last_input_to_frame_us"],
    "max_input_to_frame_us": status["max_input_to_frame_us"],
    "last_commit_to_frame_us": status["last_commit_to_frame_us"],
    "max_commit_to_frame_us": status["max_commit_to_frame_us"],
    "dmabuf": status["dmabuf"],
    "dmabuf_accepted": status["dmabuf_accepted"],
    "dmabuf_rejected": status["dmabuf_rejected"],
    "explicit_sync": status["explicit_sync"],
    "presentation_runtime_failures": status["presentation_runtime_failures"],
}

if previous:
    frame_delta = status["frames"] - previous["frames"]
    input_delta = status["input_events"] - previous["input_events"]
    commit_delta = status["commits"] - previous["commits"]
    callback_delta = status["client_frame_callbacks"] - previous["client_frame_callbacks"]
    fields.update({
        "sample_delta_us": sample_delta_us,
        "frame_delta": frame_delta,
        "input_delta": input_delta,
        "commit_delta": commit_delta,
        "client_frame_callback_delta": callback_delta,
        "estimated_fps": (frame_delta * 1_000_000 / sample_delta_us) if sample_delta_us else None,
    })

emit("status_sample", fields)

if previous and status["input_events"] > previous["input_events"]:
    emit("input_counter_advanced", {
        "input_delta": status["input_events"] - previous["input_events"],
        "last_input_to_frame_us": status["last_input_to_frame_us"],
        "max_input_to_frame_us": status["max_input_to_frame_us"],
    })

if previous and status["frames"] > previous["frames"]:
    emit("frame_pacing_sample", {
        "frame_delta": status["frames"] - previous["frames"],
        "sample_delta_us": sample_delta_us,
        "estimated_fps": ((status["frames"] - previous["frames"]) * 1_000_000 / sample_delta_us) if sample_delta_us else None,
        "last_frame_interval_us": status["last_frame_interval_us"],
        "max_frame_interval_us": status["max_frame_interval_us"],
    })
PY
    event="$(python3 -c 'import json,sys; print(json.loads(sys.stdin.read())["event"])' <<<"$event_line")"
    fields="$(python3 -c 'import json,sys; print(json.dumps(json.loads(sys.stdin.read())["fields"]))' <<<"$event_line")"
    emit_event "$event" "$fields"
  done

  TRACE_EVENTS=""
  if [[ -f "$WAYLAND_TRACE" ]]; then
    TRACE_SIZE="$(wc -c <"$WAYLAND_TRACE")"
    if (( TRACE_SIZE > TRACE_OFFSET )); then
      TRACE_EVENTS="$(parse_trace_range "$TRACE_OFFSET" "$TRACE_SIZE" || true)"
      TRACE_OFFSET="$TRACE_SIZE"
    fi
  fi
  if [[ -n "$TRACE_EVENTS" ]]; then
    printf '%s\n' "$TRACE_EVENTS"
  fi

  LAST_STATUS="$STATUS"
  LAST_SAMPLE_NS="$SAMPLE_NS"
  sleep "$SAMPLE_INTERVAL_SECONDS"
done

emit_event "probe_finished" "$(
  python3 - "$LAST_STATUS" <<'PY'
import json
import sys
status = json.loads(sys.argv[1]) if sys.argv[1] else {}
print(json.dumps({"final_status": status}))
PY
)"

target/debug/manifoldd ctl --control-socket "$CONTROL_SOCKET" quit >/dev/null || true
wait "$PID" || true
trap - EXIT
