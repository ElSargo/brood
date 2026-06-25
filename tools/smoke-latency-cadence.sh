#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

require_host_presentation_environment

if ! command -v weston-eventdemo >/dev/null 2>&1; then
  echo "ERR weston-eventdemo is required for latency smoke" >&2
  exit 2
fi

MAX_FRAME_INTERVAL_US="${MANIFOLD_MAX_FRAME_INTERVAL_US:-250000}"
MAX_INPUT_TO_FRAME_US="${MANIFOLD_MAX_INPUT_TO_FRAME_US:-150000}"
MAX_COMMIT_TO_FRAME_US="${MANIFOLD_MAX_COMMIT_TO_FRAME_US:-150000}"

cargo build -p manifoldd --offline >/dev/null

exec 9>/tmp/manifold-host-smoke.lock
flock 9

TMPDIR="$(mktemp -d /tmp/manifold-latency-smoke.XXXXXX)"
DBUS_NAME="org.manifold.Test.p$$"
LOG="$TMPDIR/manifoldd.log"
CLIENT_LOG="$TMPDIR/weston-eventdemo.log"
WRAPPER="$TMPDIR/weston-eventdemo-wrapper.sh"

cleanup() {
  if [[ -n "${PID:-}" ]] && kill -0 "$PID" 2>/dev/null; then
    target/debug/manifoldd ctl --dbus-name "$DBUS_NAME" quit >/dev/null 2>&1 || true
    sleep 0.5
    kill "$PID" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

cat >"$WRAPPER" <<EOF
#!/usr/bin/env bash
exec stdbuf -oL -eL weston-eventdemo --log-motion --log-key --log-redraw --log-resize >"$CLIENT_LOG" 2>&1
EOF
chmod +x "$WRAPPER"

RUST_LOG="${RUST_LOG:-manifoldd=info,smithay=warn}" \
  target/debug/manifoldd run --dbus-name "$DBUS_NAME" -c "$WRAPPER" >"$LOG" 2>&1 &
PID=$!

wait_for_manifoldd_control "$DBUS_NAME" "$PID" "$LOG"

ctl() {
  target/debug/manifoldd ctl --dbus-name "$DBUS_NAME" "$@"
}

field() {
  local status="$1"
  local key="$2"
  sed -n "s/.* ${key}=\\([^ ]*\\).*/\\1/p" <<<"$status"
}

require_field_ge() {
  local status="$1"
  local key="$2"
  local min="$3"
  local value
  value="$(field "$status" "$key")"
  [[ -n "$value" ]] || {
    echo "ERR missing $key in status: $status" >&2
    exit 1
  }
  (( value >= min )) || {
    echo "ERR expected $key >= $min, got $value in status: $status" >&2
    exit 1
  }
}

require_numeric_field() {
  local status="$1"
  local key="$2"
  local value
  value="$(field "$status" "$key")"
  [[ "$value" =~ ^[0-9]+$ ]] || {
    echo "ERR expected numeric $key, got '${value:-missing}' in status: $status" >&2
    exit 1
  }
  printf '%s' "$value"
}

for _ in $(seq 1 80); do
  STATUS="$(ctl status)"
  if [[ "$(field "$STATUS" windows)" =~ ^[0-9]+$ ]] &&
    (( "$(field "$STATUS" windows)" >= 1 )) &&
    [[ "$(field "$STATUS" frames)" =~ ^[0-9]+$ ]] &&
    (( "$(field "$STATUS" frames)" >= 5 )) &&
    [[ "$(field "$STATUS" max_frame_interval_us)" =~ ^[0-9]+$ ]] &&
    (( "$(field "$STATUS" max_frame_interval_us)" > 0 )) &&
    [[ "$(field "$STATUS" commit_frame_count)" =~ ^[0-9]+$ ]] &&
    (( "$(field "$STATUS" commit_frame_count)" >= 1 )) &&
    [[ "$(field "$STATUS" last_commit_to_frame_us)" =~ ^[0-9]+$ ]]; then
    break
  fi
  sleep 0.1
done

STATUS1="$(ctl status)"
require_field_ge "$STATUS1" windows 1
require_field_ge "$STATUS1" frames 5
require_field_ge "$STATUS1" commit_frame_count 1
FRAMES1="$(field "$STATUS1" frames)"
INPUT_FRAMES1="$(field "$STATUS1" input_frame_count)"
INPUT_EVENTS1="$(field "$STATUS1" input_events)"
KEYBOARD_EVENTS1="$(field "$STATUS1" keyboard_events)"
POINTER_MOTION_EVENTS1="$(field "$STATUS1" pointer_motion_events)"

ctl inject center-motion >/dev/null
ctl inject key 30 pressed >/dev/null
ctl inject key 30 released >/dev/null

for _ in $(seq 1 80); do
  STATUS2="$(ctl status)"
  if [[ "$(field "$STATUS2" frames)" =~ ^[0-9]+$ ]] &&
    (( "$(field "$STATUS2" frames)" >= FRAMES1 + 3 )) &&
    [[ "$(field "$STATUS2" input_frame_count)" =~ ^[0-9]+$ ]] &&
    (( "$(field "$STATUS2" input_frame_count)" > INPUT_FRAMES1 )) &&
    [[ "$(field "$STATUS2" last_input_to_frame_us)" =~ ^[0-9]+$ ]]; then
    break
  fi
  sleep 0.1
done

STATUS2="$(ctl status)"
require_field_ge "$STATUS2" frames "$((FRAMES1 + 3))"
require_field_ge "$STATUS2" input_events "$((INPUT_EVENTS1 + 3))"
require_field_ge "$STATUS2" keyboard_events "$((KEYBOARD_EVENTS1 + 2))"
require_field_ge "$STATUS2" pointer_motion_events "$((POINTER_MOTION_EVENTS1 + 1))"

LAST_INPUT_TO_FRAME="$(field "$STATUS2" last_input_to_frame_us)"
LAST_INPUT_BATCH_EVENTS="$(field "$STATUS2" last_input_batch_events)"
INPUT_FRAME_COUNT="$(field "$STATUS2" input_frame_count)"
LAST_COMMIT_TO_FRAME="$(require_numeric_field "$STATUS2" last_commit_to_frame_us)"
COMMIT_FRAME_COUNT="$(field "$STATUS2" commit_frame_count)"
MAX_COMMIT_TO_FRAME="$(field "$STATUS2" max_commit_to_frame_us)"
MAX_FRAME_INTERVAL="$(field "$STATUS2" max_frame_interval_us)"
LAST_FRAME_INTERVAL="$(field "$STATUS2" last_frame_interval_us)"
LAST_FRAME_RENDER="$(field "$STATUS2" last_frame_us)"

[[ "$LAST_INPUT_TO_FRAME" =~ ^[0-9]+$ ]] || {
  echo "ERR missing numeric last_input_to_frame_us: $STATUS2" >&2
  exit 1
}
(( LAST_INPUT_TO_FRAME <= MAX_INPUT_TO_FRAME_US )) || {
  echo "ERR input-to-frame latency ${LAST_INPUT_TO_FRAME}us exceeds ${MAX_INPUT_TO_FRAME_US}us" >&2
  echo "status=$STATUS2" >&2
  exit 1
}
(( LAST_COMMIT_TO_FRAME <= MAX_COMMIT_TO_FRAME_US )) || {
  echo "ERR commit-to-frame latency ${LAST_COMMIT_TO_FRAME}us exceeds ${MAX_COMMIT_TO_FRAME_US}us" >&2
  echo "status=$STATUS2" >&2
  exit 1
}
(( MAX_COMMIT_TO_FRAME <= MAX_COMMIT_TO_FRAME_US )) || {
  echo "ERR max commit-to-frame latency ${MAX_COMMIT_TO_FRAME}us exceeds ${MAX_COMMIT_TO_FRAME_US}us" >&2
  echo "status=$STATUS2" >&2
  exit 1
}
(( MAX_FRAME_INTERVAL <= MAX_FRAME_INTERVAL_US )) || {
  echo "ERR max frame interval ${MAX_FRAME_INTERVAL}us exceeds ${MAX_FRAME_INTERVAL_US}us" >&2
  echo "status=$STATUS2" >&2
  exit 1
}

echo "OK latency/cadence smoke passed"
echo "status_before=$STATUS1"
echo "status_after=$STATUS2"
echo "last_input_to_frame_us=$LAST_INPUT_TO_FRAME"
echo "input_frame_count=$INPUT_FRAME_COUNT"
echo "last_input_batch_events=$LAST_INPUT_BATCH_EVENTS"
echo "last_commit_to_frame_us=$LAST_COMMIT_TO_FRAME"
echo "commit_frame_count=$COMMIT_FRAME_COUNT"
echo "max_commit_to_frame_us=$MAX_COMMIT_TO_FRAME"
echo "last_frame_interval_us=$LAST_FRAME_INTERVAL"
echo "max_frame_interval_us=$MAX_FRAME_INTERVAL"
echo "last_frame_render_us=$LAST_FRAME_RENDER"
echo "max_input_to_frame_us=$MAX_INPUT_TO_FRAME_US"
echo "max_commit_to_frame_budget_us=$MAX_COMMIT_TO_FRAME_US"
echo "max_frame_interval_budget_us=$MAX_FRAME_INTERVAL_US"
echo "client_log=$CLIENT_LOG"
echo "log=$LOG"

ctl quit >/dev/null || true
wait "$PID" || true
trap - EXIT
