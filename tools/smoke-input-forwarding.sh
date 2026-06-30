#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

require_host_presentation_environment

if ! command -v weston-eventdemo >/dev/null 2>&1; then
  echo "ERR weston-eventdemo is required for input forwarding smoke" >&2
  exit 2
fi

cargo build -p manifoldd --offline >/dev/null

exec 9>/tmp/manifold-host-smoke.lock
flock 9

TMPDIR="$(mktemp -d /tmp/manifold-input-smoke.XXXXXX)"
DBUS_NAME="org.manifold.Test.p$$"
LOG="$TMPDIR/manifoldd.log"
CLIENT_LOG="$TMPDIR/weston-eventdemo.log"
WRAPPER="$TMPDIR/weston-eventdemo-wrapper.sh"

cleanup() {
  if [[ -n "${PID:-}" ]] && kill -0 "$PID" 2>/dev/null; then
    target/debug/manifold ctl --dbus-name "$DBUS_NAME" quit >/dev/null 2>&1 || true
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
  target/debug/manifold run --dbus-name "$DBUS_NAME" -c "$WRAPPER" >"$LOG" 2>&1 &
PID=$!

wait_for_manifoldd_control "$DBUS_NAME" "$PID" "$LOG"

ctl() {
  target/debug/manifold ctl --dbus-name "$DBUS_NAME" "$@"
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

for _ in $(seq 1 80); do
  STATUS="$(ctl status)"
  if [[ "$(field "$STATUS" windows)" =~ ^[0-9]+$ ]] &&
    (( "$(field "$STATUS" windows)" >= 1 )) &&
    [[ "$(field "$STATUS" frames)" =~ ^[0-9]+$ ]] &&
    (( "$(field "$STATUS" frames)" >= 2 )); then
    break
  fi
  sleep 0.1
done

STATUS="$(ctl status)"
require_field_ge "$STATUS" windows 1
require_field_ge "$STATUS" frames 2

ctl inject center-motion >/dev/null
sleep 0.1
ctl inject center-motion >/dev/null
ctl inject button 272 pressed >/dev/null
ctl inject button 272 released >/dev/null
ctl inject key 30 pressed >/dev/null
ctl inject key 30 released >/dev/null
ctl inject axis 0 15 >/dev/null

sleep 1
STATUS="$(ctl status)"
require_field_ge "$STATUS" pointer_motion_events 1
require_field_ge "$STATUS" pointer_button_events 2
require_field_ge "$STATUS" keyboard_events 2
require_field_ge "$STATUS" pointer_axis_events 1
require_field_ge "$STATUS" pointer_target_hits 1

grep -q "focus x:" "$CLIENT_LOG" || {
  echo "ERR eventdemo did not log pointer focus" >&2
  cat "$CLIENT_LOG" >&2 || true
  exit 1
}
grep -q "button time: .*button: 272, state: pressed" "$CLIENT_LOG" || {
  echo "ERR eventdemo did not log button press" >&2
  cat "$CLIENT_LOG" >&2 || true
  exit 1
}
grep -q "button time: .*button: 272, state: released" "$CLIENT_LOG" || {
  echo "ERR eventdemo did not log button release" >&2
  cat "$CLIENT_LOG" >&2 || true
  exit 1
}
grep -q "key key: .*state: pressed" "$CLIENT_LOG" || {
  echo "ERR eventdemo did not log key press" >&2
  cat "$CLIENT_LOG" >&2 || true
  exit 1
}
grep -q "key key: .*state: released" "$CLIENT_LOG" || {
  echo "ERR eventdemo did not log key release" >&2
  cat "$CLIENT_LOG" >&2 || true
  exit 1
}
grep -q "axis time: .*axis: vertical" "$CLIENT_LOG" || {
  echo "ERR eventdemo did not log vertical axis" >&2
  cat "$CLIENT_LOG" >&2 || true
  exit 1
}

echo "OK input forwarding smoke passed"
echo "status=$STATUS"
echo "client_log=$CLIENT_LOG"
echo "log=$LOG"

ctl quit >/dev/null || true
wait "$PID" || true
trap - EXIT
