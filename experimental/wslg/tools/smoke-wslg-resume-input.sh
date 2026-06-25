#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

require_host_presentation_environment

if ! command -v weston-eventdemo >/dev/null 2>&1; then
  echo "ERR weston-eventdemo is required for WSLg resume input smoke" >&2
  exit 2
fi

MAX_RESUME_ATTACH_LATENCY_US="${MANIFOLD_MAX_RESUME_ATTACH_LATENCY_US:-75000}"
if ! [[ "$MAX_RESUME_ATTACH_LATENCY_US" =~ ^[0-9]+$ ]] || (( MAX_RESUME_ATTACH_LATENCY_US < 1 )); then
  echo "ERR MANIFOLD_MAX_RESUME_ATTACH_LATENCY_US must be a positive integer" >&2
  exit 2
fi
MAX_RESUME_DETACH_LATENCY_US="${MANIFOLD_MAX_RESUME_DETACH_LATENCY_US:-75000}"
if ! [[ "$MAX_RESUME_DETACH_LATENCY_US" =~ ^[0-9]+$ ]] || (( MAX_RESUME_DETACH_LATENCY_US < 1 )); then
  echo "ERR MANIFOLD_MAX_RESUME_DETACH_LATENCY_US must be a positive integer" >&2
  exit 2
fi

cargo build -p manifoldd --offline >/dev/null

exec 9>/tmp/manifold-host-smoke.lock
flock 9

TMPDIR="$(mktemp -d /tmp/manifold-resume-input.XXXXXX)"
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
exec stdbuf -oL -eL weston-eventdemo \
  --log-motion --log-button --log-key --log-axis --log-focus --log-redraw --log-resize \
  >"$CLIENT_LOG" 2>&1
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

numeric_field() {
  local status="$1"
  local key="$2"
  local value
  value="$(field "$status" "$key")"
  [[ "$value" =~ ^[0-9]+$ ]] || {
    echo "ERR missing numeric $key in status: $status" >&2
    exit 1
  }
  printf '%s' "$value"
}

require_field_ge() {
  local status="$1"
  local key="$2"
  local min="$3"
  local value
  value="$(numeric_field "$status" "$key")"
  (( value >= min )) || {
    echo "ERR expected $key >= $min, got $value in status: $status" >&2
    exit 1
  }
}

wait_status() {
  local predicate="$1"
  local status
  for _ in $(seq 1 120); do
    status="$(ctl status)"
    if eval "$predicate"; then
      echo "$status"
      return 0
    fi
    sleep 0.1
  done
  echo "ERR timed out waiting for status predicate: $predicate" >&2
  ctl status >&2 || true
  cat "$CLIENT_LOG" >&2 || true
  tail -120 "$LOG" >&2 || true
  exit 1
}

event_count() {
  local pattern="$1"
  if [[ -s "$CLIENT_LOG" ]]; then
    grep -c "$pattern" "$CLIENT_LOG" || true
  else
    printf '0'
  fi
}

wait_event_count_ge() {
  local name="$1"
  local pattern="$2"
  local min="$3"
  local count
  for _ in $(seq 1 40); do
    count="$(event_count "$pattern")"
    if (( count >= min )); then
      printf '%s' "$count"
      return 0
    fi
    sleep 0.1
  done
  echo "ERR expected $name count >= $min" >&2
  cat "$CLIENT_LOG" >&2 || true
  exit 1
}

inject_input_batch() {
  ctl inject center-motion >/dev/null
  sleep 0.1
  ctl inject center-motion >/dev/null
  ctl inject button 272 pressed >/dev/null
  ctl inject button 272 released >/dev/null
  ctl inject key 30 pressed >/dev/null
  ctl inject key 30 released >/dev/null
  ctl inject axis 0 15 >/dev/null
}

STATUS_INITIAL="$(wait_status '[[ "$(field "$status" presentation)" == "Attached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" frames)" =~ ^[0-9]+$ ]] && (( "$(field "$status" frames)" >= 2 )) && [[ "$(field "$status" commits)" =~ ^[0-9]+$ ]] && (( "$(field "$status" commits)" >= 1 ))')"
require_field_ge "$STATUS_INITIAL" windows 1
require_field_ge "$STATUS_INITIAL" frames 2

FRAMES_INITIAL="$(numeric_field "$STATUS_INITIAL" frames)"
COMMITS_INITIAL="$(numeric_field "$STATUS_INITIAL" commits)"
ATTACHES_INITIAL="$(numeric_field "$STATUS_INITIAL" attach_count)"
DETACHES_INITIAL="$(numeric_field "$STATUS_INITIAL" detach_count)"

inject_input_batch
STATUS_INPUT1="$(wait_status '[[ "$(field "$status" pointer_motion_events)" =~ ^[0-9]+$ ]] && (( "$(field "$status" pointer_motion_events)" >= 2 )) && [[ "$(field "$status" pointer_button_events)" =~ ^[0-9]+$ ]] && (( "$(field "$status" pointer_button_events)" >= 2 )) && [[ "$(field "$status" keyboard_events)" =~ ^[0-9]+$ ]] && (( "$(field "$status" keyboard_events)" >= 2 )) && [[ "$(field "$status" pointer_axis_events)" =~ ^[0-9]+$ ]] && (( "$(field "$status" pointer_axis_events)" >= 1 )) && [[ "$(field "$status" pointer_target_hits)" =~ ^[0-9]+$ ]] && (( "$(field "$status" pointer_target_hits)" >= 1 ))')"
BUTTON_PRESSED_1="$(wait_event_count_ge "button press before resume" "button time: .*button: 272, state: pressed" 1)"
BUTTON_RELEASED_1="$(wait_event_count_ge "button release before resume" "button time: .*button: 272, state: released" 1)"
KEY_PRESSED_1="$(wait_event_count_ge "key press before resume" "key key: .*state: pressed" 1)"
KEY_RELEASED_1="$(wait_event_count_ge "key release before resume" "key key: .*state: released" 1)"
AXIS_1="$(wait_event_count_ge "axis before resume" "axis time: .*axis: vertical" 1)"

ctl detach >/dev/null
STATUS_DETACHED="$(wait_status '[[ "$(field "$status" presentation)" == "Detached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" detach_count)" =~ ^[0-9]+$ ]] && (( "$(field "$status" detach_count)" > DETACHES_INITIAL ))')"
require_field_ge "$STATUS_DETACHED" windows 1
CALLBACKS_DETACHED="$(numeric_field "$STATUS_DETACHED" client_frame_callbacks)"
LAST_DETACH_LATENCY_US="$(numeric_field "$STATUS_DETACHED" last_detach_latency_us)"
MAX_DETACH_LATENCY_US="$(numeric_field "$STATUS_DETACHED" max_detach_latency_us)"
(( LAST_DETACH_LATENCY_US <= MAX_RESUME_DETACH_LATENCY_US )) || {
  echo "ERR resume detach latency ${LAST_DETACH_LATENCY_US}us exceeds ${MAX_RESUME_DETACH_LATENCY_US}us" >&2
  echo "status=$STATUS_DETACHED" >&2
  exit 1
}

ctl attach >/dev/null
STATUS_REATTACHED="$(wait_status '[[ "$(field "$status" presentation)" == "Attached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" attach_count)" =~ ^[0-9]+$ ]] && (( "$(field "$status" attach_count)" > ATTACHES_INITIAL )) && [[ "$(field "$status" frames)" =~ ^[0-9]+$ ]] && (( "$(field "$status" frames)" > FRAMES_INITIAL )) && [[ "$(field "$status" client_frame_callbacks)" =~ ^[0-9]+$ ]] && (( "$(field "$status" client_frame_callbacks)" > CALLBACKS_DETACHED ))')"
require_field_ge "$STATUS_REATTACHED" windows 1
require_field_ge "$STATUS_REATTACHED" commits "$COMMITS_INITIAL"
LAST_ATTACH_LATENCY_US="$(numeric_field "$STATUS_REATTACHED" last_attach_latency_us)"
MAX_ATTACH_LATENCY_US="$(numeric_field "$STATUS_REATTACHED" max_attach_latency_us)"
LAST_ATTACH_TO_FRAME_US="$(numeric_field "$STATUS_REATTACHED" last_attach_to_frame_us)"
MAX_ATTACH_TO_FRAME_US="$(numeric_field "$STATUS_REATTACHED" max_attach_to_frame_us)"
(( LAST_ATTACH_LATENCY_US <= MAX_RESUME_ATTACH_LATENCY_US )) || {
  echo "ERR resume attach latency ${LAST_ATTACH_LATENCY_US}us exceeds ${MAX_RESUME_ATTACH_LATENCY_US}us" >&2
  echo "status=$STATUS_REATTACHED" >&2
  exit 1
}
(( LAST_ATTACH_TO_FRAME_US <= MAX_RESUME_ATTACH_LATENCY_US )) || {
  echo "ERR resume attach-to-frame latency ${LAST_ATTACH_TO_FRAME_US}us exceeds ${MAX_RESUME_ATTACH_LATENCY_US}us" >&2
  echo "status=$STATUS_REATTACHED" >&2
  exit 1
}

MOTION_BEFORE="$(numeric_field "$STATUS_INPUT1" pointer_motion_events)"
BUTTON_BEFORE="$(numeric_field "$STATUS_INPUT1" pointer_button_events)"
KEY_BEFORE="$(numeric_field "$STATUS_INPUT1" keyboard_events)"
AXIS_BEFORE="$(numeric_field "$STATUS_INPUT1" pointer_axis_events)"
HITS_BEFORE="$(numeric_field "$STATUS_INPUT1" pointer_target_hits)"

inject_input_batch
STATUS_INPUT2="$(wait_status '[[ "$(field "$status" pointer_motion_events)" =~ ^[0-9]+$ ]] && (( "$(field "$status" pointer_motion_events)" >= MOTION_BEFORE + 2 )) && [[ "$(field "$status" pointer_button_events)" =~ ^[0-9]+$ ]] && (( "$(field "$status" pointer_button_events)" >= BUTTON_BEFORE + 2 )) && [[ "$(field "$status" keyboard_events)" =~ ^[0-9]+$ ]] && (( "$(field "$status" keyboard_events)" >= KEY_BEFORE + 2 )) && [[ "$(field "$status" pointer_axis_events)" =~ ^[0-9]+$ ]] && (( "$(field "$status" pointer_axis_events)" >= AXIS_BEFORE + 1 )) && [[ "$(field "$status" pointer_target_hits)" =~ ^[0-9]+$ ]] && (( "$(field "$status" pointer_target_hits)" > HITS_BEFORE ))')"
BUTTON_PRESSED_2="$(wait_event_count_ge "button press after resume" "button time: .*button: 272, state: pressed" "$((BUTTON_PRESSED_1 + 1))")"
BUTTON_RELEASED_2="$(wait_event_count_ge "button release after resume" "button time: .*button: 272, state: released" "$((BUTTON_RELEASED_1 + 1))")"
KEY_PRESSED_2="$(wait_event_count_ge "key press after resume" "key key: .*state: pressed" "$((KEY_PRESSED_1 + 1))")"
KEY_RELEASED_2="$(wait_event_count_ge "key release after resume" "key key: .*state: released" "$((KEY_RELEASED_1 + 1))")"
AXIS_2="$(wait_event_count_ge "axis after resume" "axis time: .*axis: vertical" "$((AXIS_1 + 1))")"

echo "OK WSLg resume input smoke passed"
echo "status_initial=$STATUS_INITIAL"
echo "status_before_detach_input=$STATUS_INPUT1"
echo "status_detached=$STATUS_DETACHED"
echo "status_reattached=$STATUS_REATTACHED"
echo "status_after_resume_input=$STATUS_INPUT2"
echo "last_detach_latency_us=$LAST_DETACH_LATENCY_US"
echo "max_detach_latency_us=$MAX_DETACH_LATENCY_US"
echo "max_resume_detach_latency_budget_us=$MAX_RESUME_DETACH_LATENCY_US"
echo "last_attach_latency_us=$LAST_ATTACH_LATENCY_US"
echo "max_attach_latency_us=$MAX_ATTACH_LATENCY_US"
echo "last_attach_to_frame_us=$LAST_ATTACH_TO_FRAME_US"
echo "max_attach_to_frame_us=$MAX_ATTACH_TO_FRAME_US"
echo "max_resume_attach_latency_budget_us=$MAX_RESUME_ATTACH_LATENCY_US"
echo "client_frame_callbacks_after_detach=$CALLBACKS_DETACHED"
echo "button_pressed_counts=$BUTTON_PRESSED_1->$BUTTON_PRESSED_2"
echo "button_released_counts=$BUTTON_RELEASED_1->$BUTTON_RELEASED_2"
echo "key_pressed_counts=$KEY_PRESSED_1->$KEY_PRESSED_2"
echo "key_released_counts=$KEY_RELEASED_1->$KEY_RELEASED_2"
echo "axis_counts=$AXIS_1->$AXIS_2"
echo "client_log=$CLIENT_LOG"
echo "log=$LOG"

ctl quit >/dev/null || true
wait "$PID" || true
trap - EXIT
