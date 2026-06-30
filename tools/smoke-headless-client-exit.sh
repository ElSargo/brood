#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

if ! command -v weston-simple-shm > /dev/null 2>&1; then
  echo "ERR weston-simple-shm is required for headless client-exit smoke" >&2
  exit 2
fi

cargo build -p manifoldd --offline > /dev/null

TMPDIR="$(mktemp -d /tmp/manifold-headless-client-exit.XXXXXX)"
DBUS_NAME="org.manifold.Test.p$$"
LOG="$TMPDIR/manifoldd.log"
CLIENT_LOG="$TMPDIR/client.log"
CLIENT_PIDFILE="$TMPDIR/client.pid"
WRAPPER="$TMPDIR/client-wrapper.sh"

cleanup() {
  if [[ -s "$CLIENT_PIDFILE" ]]; then
    CLIENT_PID="$(cat "$CLIENT_PIDFILE")"
    kill "$CLIENT_PID" > /dev/null 2>&1 || true
  fi
  if [[ -n "${PID:-}" ]] && kill -0 "$PID" 2> /dev/null; then
    target/debug/manifold ctl --dbus-name "$DBUS_NAME" quit > /dev/null 2>&1 || true
    sleep 0.5
    kill "$PID" > /dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

cat > "$WRAPPER" << EOF
#!/usr/bin/env bash
echo "\$\$" >"$CLIENT_PIDFILE"
exec weston-simple-shm >"$CLIENT_LOG" 2>&1
EOF
chmod +x "$WRAPPER"

RUST_LOG="${RUST_LOG:-manifoldd=info,smithay=warn}" \
  target/debug/manifold run --headless --dbus-name "$DBUS_NAME" -c "$WRAPPER" > "$LOG" 2>&1 &
PID=$!

wait_for_manifoldd_control "$DBUS_NAME" "$PID" "$LOG"

ctl() {
  target/debug/manifold ctl --dbus-name "$DBUS_NAME" "$@"
}

field() {
  local status="$1"
  local key="$2"
  sed -n "s/.* ${key}=\\([^ ]*\\).*/\\1/p" <<< "$status"
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
  ((value >= min)) || {
    echo "ERR expected $key >= $min, got $value in status: $status" >&2
    exit 1
  }
}

wait_status() {
  local predicate="$1"
  local status
  for _ in $(seq 1 100); do
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

for _ in $(seq 1 100); do
  [[ -s "$CLIENT_PIDFILE" ]] && break
  sleep 0.1
done
[[ -s "$CLIENT_PIDFILE" ]] || {
  echo "ERR client pidfile was not written" >&2
  tail -120 "$LOG" >&2 || true
  exit 1
}

STATUS1="$(wait_status '[[ "$(field "$status" presentation)" == "Detached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" commits)" =~ ^[0-9]+$ ]] && (( "$(field "$status" commits)" >= 1 ))')"
require_field_ge "$STATUS1" windows 1
require_field_ge "$STATUS1" commits 1

ctl inject center-motion > /dev/null
ctl inject button 272 pressed > /dev/null
ctl inject button 272 released > /dev/null
STATUS_FOCUSED="$(wait_status '[[ "$(field "$status" pointer_target_hits)" =~ ^[0-9]+$ ]] && (( "$(field "$status" pointer_target_hits)" >= 3 )) && [[ "$(field "$status" pointer_button_events)" =~ ^[0-9]+$ ]] && (( "$(field "$status" pointer_button_events)" >= 2 ))')"
require_field_ge "$STATUS_FOCUSED" pointer_target_hits 3
require_field_ge "$STATUS_FOCUSED" pointer_button_events 2

CLIENT_PID="$(cat "$CLIENT_PIDFILE")"
kill "$CLIENT_PID"
sleep 0.5

STATUS2="$(wait_status '[[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" == 0 )) && [[ "$(field "$status" window_lifecycle_focus_clears)" =~ ^[0-9]+$ ]] && (( "$(field "$status" window_lifecycle_focus_clears)" >= 1 ))')"
require_field_ge "$STATUS2" focus_clears 1
require_field_ge "$STATUS2" window_lifecycle_focus_clears 1

echo "OK headless client-exit smoke passed"
echo "client_pid=$CLIENT_PID"
echo "status_mapped=$STATUS1"
echo "status_focused=$STATUS_FOCUSED"
echo "status_after_exit=$STATUS2"
echo "client_log=$CLIENT_LOG"
echo "log=$LOG"

ctl quit > /dev/null || true
wait "$PID" || true
trap - EXIT
