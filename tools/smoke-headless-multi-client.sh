#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

if ! command -v weston-simple-shm >/dev/null 2>&1; then
  echo "ERR weston-simple-shm is required for headless multi-client smoke" >&2
  exit 2
fi

cargo build -p manifoldd --offline >/dev/null

TMPDIR="$(mktemp -d /tmp/manifold-headless-multi-client.XXXXXX)"
DBUS_NAME="org.manifold.Test.p$$"
LOG="$TMPDIR/manifoldd.log"
CLIENT1_LOG="$TMPDIR/client-1.log"
CLIENT2_LOG="$TMPDIR/client-2.log"
CLIENT1_PIDFILE="$TMPDIR/client-1.pid"
CLIENT2_PIDFILE="$TMPDIR/client-2.pid"
WRAPPER1="$TMPDIR/client-1-wrapper.sh"
WRAPPER2="$TMPDIR/client-2-wrapper.sh"

cleanup() {
  for pidfile in "$CLIENT1_PIDFILE" "$CLIENT2_PIDFILE"; do
    if [[ -s "$pidfile" ]]; then
      kill "$(cat "$pidfile")" >/dev/null 2>&1 || true
    fi
  done
  if [[ -n "${PID:-}" ]] && kill -0 "$PID" 2>/dev/null; then
    target/debug/manifoldd ctl --dbus-name "$DBUS_NAME" quit >/dev/null 2>&1 || true
    sleep 0.5
    kill "$PID" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

cat >"$WRAPPER1" <<EOF
#!/usr/bin/env bash
echo "\$\$" >"$CLIENT1_PIDFILE"
exec weston-simple-shm >"$CLIENT1_LOG" 2>&1
EOF
cat >"$WRAPPER2" <<EOF
#!/usr/bin/env bash
echo "\$\$" >"$CLIENT2_PIDFILE"
exec weston-simple-shm >"$CLIENT2_LOG" 2>&1
EOF
chmod +x "$WRAPPER1" "$WRAPPER2"

RUST_LOG="${RUST_LOG:-manifoldd=info,smithay=warn}" \
  target/debug/manifoldd run --headless --dbus-name "$DBUS_NAME" >"$LOG" 2>&1 &
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
  [[ "$value" =~ ^[0-9]+$ ]] || {
    echo "ERR missing numeric $key in status: $status" >&2
    exit 1
  }
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
  cat "$CLIENT1_LOG" "$CLIENT2_LOG" >&2 2>/dev/null || true
  tail -120 "$LOG" >&2 || true
  exit 1
}

wait_pidfile() {
  local pidfile="$1"
  for _ in $(seq 1 100); do
    [[ -s "$pidfile" ]] && return 0
    sleep 0.1
  done
  echo "ERR client pidfile was not written: $pidfile" >&2
  tail -120 "$LOG" >&2 || true
  exit 1
}

ctl spawn "$WRAPPER1" >/dev/null
wait_pidfile "$CLIENT1_PIDFILE"

STATUS_ONE_INITIAL="$(wait_status '[[ "$(field "$status" presentation)" == "Detached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" == 1 )) && [[ "$(field "$status" commits)" =~ ^[0-9]+$ ]] && (( "$(field "$status" commits)" >= 1 ))')"
require_field_ge "$STATUS_ONE_INITIAL" windows 1
require_field_ge "$STATUS_ONE_INITIAL" commits 1

ctl inject center-motion >/dev/null
ctl inject button 272 pressed >/dev/null
ctl inject button 272 released >/dev/null
STATUS_FOCUSED_ONE="$(wait_status '[[ "$(field "$status" pointer_target_hits)" =~ ^[0-9]+$ ]] && (( "$(field "$status" pointer_target_hits)" >= 3 )) && [[ "$(field "$status" pointer_button_events)" =~ ^[0-9]+$ ]] && (( "$(field "$status" pointer_button_events)" >= 2 ))')"
require_field_ge "$STATUS_FOCUSED_ONE" pointer_target_hits 3
require_field_ge "$STATUS_FOCUSED_ONE" pointer_button_events 2

ctl spawn "$WRAPPER2" >/dev/null
wait_pidfile "$CLIENT2_PIDFILE"

STATUS_TWO="$(wait_status '[[ "$(field "$status" presentation)" == "Detached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 2 )) && [[ "$(field "$status" commits)" =~ ^[0-9]+$ ]] && (( "$(field "$status" commits)" >= 2 ))')"
require_field_ge "$STATUS_TWO" windows 2
require_field_ge "$STATUS_TWO" commits 2

CLIENT1_PID="$(cat "$CLIENT1_PIDFILE")"
CLIENT2_PID="$(cat "$CLIENT2_PIDFILE")"
kill "$CLIENT2_PID"
sleep 0.5
STATUS_ONE="$(wait_status '[[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" == 1 )) && [[ "$(field "$status" focus_repairs)" =~ ^[0-9]+$ ]] && (( "$(field "$status" focus_repairs)" >= 1 )) && [[ "$(field "$status" window_lifecycle_focus_repairs)" =~ ^[0-9]+$ ]] && (( "$(field "$status" window_lifecycle_focus_repairs)" >= 1 ))')"
require_field_ge "$STATUS_ONE" focus_repairs 1
require_field_ge "$STATUS_ONE" window_lifecycle_focus_repairs 1

kill "$CLIENT1_PID"
sleep 0.5
STATUS_ZERO="$(wait_status '[[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" == 0 )) && [[ "$(field "$status" focus_clears)" =~ ^[0-9]+$ ]] && (( "$(field "$status" focus_clears)" >= 1 )) && [[ "$(field "$status" window_lifecycle_focus_clears)" =~ ^[0-9]+$ ]] && (( "$(field "$status" window_lifecycle_focus_clears)" >= 1 ))')"
require_field_ge "$STATUS_ZERO" focus_clears 1
require_field_ge "$STATUS_ZERO" window_lifecycle_focus_clears 1

echo "OK headless multi-client smoke passed"
echo "client_1_pid=$CLIENT1_PID"
echo "client_2_pid=$CLIENT2_PID"
echo "status_one_initial=$STATUS_ONE_INITIAL"
echo "status_focused_one=$STATUS_FOCUSED_ONE"
echo "status_two=$STATUS_TWO"
echo "status_one=$STATUS_ONE"
echo "status_zero=$STATUS_ZERO"
echo "client_1_log=$CLIENT1_LOG"
echo "client_2_log=$CLIENT2_LOG"
echo "log=$LOG"

ctl quit >/dev/null || true
wait "$PID" || true
trap - EXIT
