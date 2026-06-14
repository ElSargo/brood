#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

require_host_presentation_environment

CLIENT="${MANIFOLD_SMOKE_CLIENT:-}"
if [[ -z "$CLIENT" ]]; then
  if command -v weston-simple-egl >/dev/null 2>&1; then
    CLIENT="$(command -v weston-simple-egl)"
  elif command -v weston-simple-shm >/dev/null 2>&1; then
    CLIENT="$(command -v weston-simple-shm)"
  else
    echo "ERR no detached-client-exit smoke client found; install weston-simple-egl or set MANIFOLD_SMOKE_CLIENT" >&2
    exit 2
  fi
fi

cargo build -p manifoldd --offline >/dev/null

exec 9>/tmp/manifold-wslg-smoke.lock
flock 9

TMPDIR="$(mktemp -d /tmp/manifold-detached-client-exit.XXXXXX)"
SOCK="$TMPDIR/control.sock"
LOG="$TMPDIR/manifoldd.log"
TRACE="$TMPDIR/client-wayland.log"
CLIENT_PIDFILE="$TMPDIR/client.pid"
WRAPPER="$TMPDIR/client-wrapper.sh"

cleanup() {
  if [[ -s "$CLIENT_PIDFILE" ]]; then
    kill "$(cat "$CLIENT_PIDFILE")" >/dev/null 2>&1 || true
  fi
  if [[ -n "${PID:-}" ]] && kill -0 "$PID" 2>/dev/null; then
    target/debug/manifoldd ctl --control-socket "$SOCK" quit >/dev/null 2>&1 || true
    sleep 0.5
    kill "$PID" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

cat >"$WRAPPER" <<EOF
#!/usr/bin/env bash
echo "\$\$" >"$CLIENT_PIDFILE"
exec env WAYLAND_DEBUG=1 "$CLIENT" 2>"$TRACE"
EOF
chmod +x "$WRAPPER"

RUST_LOG="${RUST_LOG:-manifoldd=info,smithay=warn}" \
  target/debug/manifoldd run --control-socket "$SOCK" -c "$WRAPPER" >"$LOG" 2>&1 &
PID=$!

wait_for_manifoldd_control "$SOCK" "$PID" "$LOG"

ctl() {
  target/debug/manifoldd ctl --control-socket "$SOCK" "$@"
}

field() {
  local status="$1"
  local key="$2"
  sed -n "s/.* ${key}=\\([^ ]*\\).*/\\1/p" <<<"$status"
}

require_numeric_field_ge() {
  local status="$1"
  local key="$2"
  local min="$3"
  local value
  value="$(field "$status" "$key")"
  [[ "$value" =~ ^[0-9]+$ ]] || {
    echo "ERR missing numeric $key in status: $status" >&2
    exit 1
  }
  (( value >= min )) || {
    echo "ERR expected $key >= $min, got $value in status: $status" >&2
    exit 1
  }
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
  tail -120 "$LOG" >&2 || true
  exit 1
}

for _ in $(seq 1 100); do
  [[ -s "$CLIENT_PIDFILE" ]] && break
  sleep 0.1
done
[[ -s "$CLIENT_PIDFILE" ]] || {
  echo "ERR client pidfile was not written" >&2
  tail -120 "$LOG" >&2 || true
  exit 1
}

STATUS_ATTACHED="$(wait_status '[[ "$(field "$status" presentation)" == "Attached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" frames)" =~ ^[0-9]+$ ]] && (( "$(field "$status" frames)" >= 2 )) && [[ "$(field "$status" commits)" =~ ^[0-9]+$ ]] && (( "$(field "$status" commits)" >= 1 ))')"
require_numeric_field_ge "$STATUS_ATTACHED" windows 1
require_numeric_field_ge "$STATUS_ATTACHED" frames 2
require_numeric_field_ge "$STATUS_ATTACHED" commits 1
FRAMES_ATTACHED="$(field "$STATUS_ATTACHED" frames)"
ATTACHES_ATTACHED="$(field "$STATUS_ATTACHED" attach_count)"
DETACHES_ATTACHED="$(field "$STATUS_ATTACHED" detach_count)"

ctl inject center-motion >/dev/null
ctl inject button 272 pressed >/dev/null
ctl inject button 272 released >/dev/null
STATUS_FOCUSED="$(wait_status '[[ "$(field "$status" pointer_target_hits)" =~ ^[0-9]+$ ]] && (( "$(field "$status" pointer_target_hits)" >= 3 )) && [[ "$(field "$status" pointer_button_events)" =~ ^[0-9]+$ ]] && (( "$(field "$status" pointer_button_events)" >= 2 ))')"
require_numeric_field_ge "$STATUS_FOCUSED" pointer_target_hits 3
require_numeric_field_ge "$STATUS_FOCUSED" pointer_button_events 2

ctl detach >/dev/null
STATUS_DETACHED="$(wait_status '[[ "$(field "$status" presentation)" == "Detached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" detach_count)" =~ ^[0-9]+$ ]] && (( "$(field "$status" detach_count)" > DETACHES_ATTACHED ))')"
require_numeric_field_ge "$STATUS_DETACHED" windows 1

CLIENT_PID="$(cat "$CLIENT_PIDFILE")"
kill "$CLIENT_PID"
sleep 0.5

STATUS_EXITED_DETACHED="$(wait_status '[[ "$(field "$status" presentation)" == "Detached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" == 0 )) && [[ "$(field "$status" focus_clears)" =~ ^[0-9]+$ ]] && (( "$(field "$status" focus_clears)" >= 1 )) && [[ "$(field "$status" window_lifecycle_focus_clears)" =~ ^[0-9]+$ ]] && (( "$(field "$status" window_lifecycle_focus_clears)" >= 1 ))')"
require_numeric_field_ge "$STATUS_EXITED_DETACHED" focus_clears 1
require_numeric_field_ge "$STATUS_EXITED_DETACHED" window_lifecycle_focus_clears 1

ctl attach >/dev/null
STATUS_REATTACHED_EMPTY="$(wait_status '[[ "$(field "$status" presentation)" == "Attached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" == 0 )) && [[ "$(field "$status" attach_count)" =~ ^[0-9]+$ ]] && (( "$(field "$status" attach_count)" > ATTACHES_ATTACHED )) && [[ "$(field "$status" frames)" =~ ^[0-9]+$ ]] && (( "$(field "$status" frames)" > FRAMES_ATTACHED ))')"
require_numeric_field_ge "$STATUS_REATTACHED_EMPTY" attach_count "$((ATTACHES_ATTACHED + 1))"
FRAMES_REATTACHED_EMPTY="$(numeric_field "$STATUS_REATTACHED_EMPTY" frames)"
INPUT_EVENTS_BEFORE_RESPAWN="$(numeric_field "$STATUS_REATTACHED_EMPTY" input_events)"
INPUT_FRAME_COUNT_BEFORE_RESPAWN="$(numeric_field "$STATUS_REATTACHED_EMPTY" input_frame_count)"
POINTER_BUTTONS_BEFORE_RESPAWN="$(numeric_field "$STATUS_REATTACHED_EMPTY" pointer_button_events)"
KEYBOARD_EVENTS_BEFORE_RESPAWN="$(numeric_field "$STATUS_REATTACHED_EMPTY" keyboard_events)"
TARGET_HITS_BEFORE_RESPAWN="$(numeric_field "$STATUS_REATTACHED_EMPTY" pointer_target_hits)"

: >"$CLIENT_PIDFILE"
ctl spawn "$WRAPPER" >/dev/null
for _ in $(seq 1 100); do
  [[ -s "$CLIENT_PIDFILE" ]] && break
  sleep 0.1
done
[[ -s "$CLIENT_PIDFILE" ]] || {
  echo "ERR respawned client pidfile was not written" >&2
  tail -120 "$LOG" >&2 || true
  exit 1
}
RESPAWNED_CLIENT_PID="$(cat "$CLIENT_PIDFILE")"

STATUS_RESPAWNED_ATTACHED="$(wait_status '[[ "$(field "$status" presentation)" == "Attached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" frames)" =~ ^[0-9]+$ ]] && (( "$(field "$status" frames)" > FRAMES_REATTACHED_EMPTY ))')"
require_numeric_field_ge "$STATUS_RESPAWNED_ATTACHED" windows 1

ctl inject center-motion >/dev/null
ctl inject button 272 pressed >/dev/null
ctl inject button 272 released >/dev/null
ctl inject key 30 pressed >/dev/null
ctl inject key 30 released >/dev/null
STATUS_RESPAWNED_INPUT="$(wait_status '[[ "$(field "$status" presentation)" == "Attached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" input_events)" =~ ^[0-9]+$ ]] && (( "$(field "$status" input_events)" >= INPUT_EVENTS_BEFORE_RESPAWN + 5 )) && [[ "$(field "$status" pointer_button_events)" =~ ^[0-9]+$ ]] && (( "$(field "$status" pointer_button_events)" >= POINTER_BUTTONS_BEFORE_RESPAWN + 2 )) && [[ "$(field "$status" keyboard_events)" =~ ^[0-9]+$ ]] && (( "$(field "$status" keyboard_events)" >= KEYBOARD_EVENTS_BEFORE_RESPAWN + 2 )) && [[ "$(field "$status" pointer_target_hits)" =~ ^[0-9]+$ ]] && (( "$(field "$status" pointer_target_hits)" > TARGET_HITS_BEFORE_RESPAWN )) && [[ "$(field "$status" input_frame_count)" =~ ^[0-9]+$ ]] && (( "$(field "$status" input_frame_count)" > INPUT_FRAME_COUNT_BEFORE_RESPAWN )) && [[ "$(field "$status" last_input_to_frame_us)" =~ ^[0-9]+$ ]]')"
require_numeric_field_ge "$STATUS_RESPAWNED_INPUT" pointer_target_hits "$((TARGET_HITS_BEFORE_RESPAWN + 1))"

echo "OK WSLg detached client-exit smoke passed"
echo "client=$CLIENT"
echo "client_pid=$CLIENT_PID"
echo "respawned_client_pid=$RESPAWNED_CLIENT_PID"
echo "status_attached=$STATUS_ATTACHED"
echo "status_focused=$STATUS_FOCUSED"
echo "status_detached=$STATUS_DETACHED"
echo "status_exited_detached=$STATUS_EXITED_DETACHED"
echo "status_reattached_empty=$STATUS_REATTACHED_EMPTY"
echo "status_respawned_attached=$STATUS_RESPAWNED_ATTACHED"
echo "status_respawned_input=$STATUS_RESPAWNED_INPUT"
echo "log=$LOG"
echo "wayland_trace=$TRACE"

ctl quit >/dev/null || true
wait "$PID" || true
trap - EXIT
