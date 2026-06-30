#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

if ! command -v weston-eventdemo >/dev/null 2>&1; then
  echo "ERR weston-eventdemo is required for headless input forwarding smoke" >&2
  exit 2
fi

cargo build -p manifoldd --offline >/dev/null

TMPDIR="$(mktemp -d /tmp/manifold-headless-input.XXXXXX)"
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

env WAYLAND_DISPLAY=manifold-missing-wayland DISPLAY= \
  RUST_LOG="${RUST_LOG:-manifoldd=info,smithay=warn}" \
  target/debug/manifold run --headless --no-session-runner --no-default-modules --dbus-name "$DBUS_NAME" -c "$WRAPPER" >"$LOG" 2>&1 &
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

STATUS_MAPPED="$(wait_status '[[ "$(field "$status" presentation)" == "Detached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" commits)" =~ ^[0-9]+$ ]] && (( "$(field "$status" commits)" >= 1 ))')"
require_field_ge "$STATUS_MAPPED" windows 1
require_field_ge "$STATUS_MAPPED" commits 1

ctl inject center-motion >/dev/null
sleep 0.1
ctl inject center-motion >/dev/null
ctl inject button 272 pressed >/dev/null
ctl inject button 272 released >/dev/null
ctl inject key 30 pressed >/dev/null
ctl inject key 30 released >/dev/null
ctl inject key 42 pressed >/dev/null
ctl inject key 42 released >/dev/null
ctl inject key 54 pressed >/dev/null
ctl inject key 54 released >/dev/null
ctl inject axis 0 15 >/dev/null

STATUS_FORWARDED="$(wait_status '[[ "$(field "$status" pointer_motion_events)" =~ ^[0-9]+$ ]] && (( "$(field "$status" pointer_motion_events)" >= 2 )) && [[ "$(field "$status" pointer_button_events)" =~ ^[0-9]+$ ]] && (( "$(field "$status" pointer_button_events)" >= 2 )) && [[ "$(field "$status" keyboard_events)" =~ ^[0-9]+$ ]] && (( "$(field "$status" keyboard_events)" >= 6 )) && [[ "$(field "$status" pointer_axis_events)" =~ ^[0-9]+$ ]] && (( "$(field "$status" pointer_axis_events)" >= 1 )) && [[ "$(field "$status" pointer_target_hits)" =~ ^[0-9]+$ ]] && (( "$(field "$status" pointer_target_hits)" >= 1 ))')"

require_field_ge "$STATUS_FORWARDED" pointer_motion_events 2
require_field_ge "$STATUS_FORWARDED" pointer_button_events 2
require_field_ge "$STATUS_FORWARDED" keyboard_events 6
require_field_ge "$STATUS_FORWARDED" pointer_axis_events 1
require_field_ge "$STATUS_FORWARDED" pointer_target_hits 1

ctl inject key 37 pressed >/dev/null
ctl inject key 56 pressed >/dev/null
ctl inject key 56 pressed >/dev/null
ctl inject key 56 released >/dev/null
ctl inject key 37 released >/dev/null
STATUS_MODE_NORMAL="$(wait_status '[[ "$(field "$status" input_mode)" == "normal" ]]')"

ctl inject key 37 pressed >/dev/null
ctl inject key 56 pressed >/dev/null
ctl inject key 56 pressed >/dev/null
ctl inject key 56 released >/dev/null
ctl inject key 37 released >/dev/null
STATUS_MODE_PASSTHROUGH="$(wait_status '[[ "$(field "$status" input_mode)" == "passthrough" ]]')"

for _ in $(seq 1 30); do
  if grep -q "focus x:" "$CLIENT_LOG" &&
    grep -q "button time: .*button: 272, state: pressed" "$CLIENT_LOG" &&
    grep -q "button time: .*button: 272, state: released" "$CLIENT_LOG" &&
    grep -q "key key: 22, .*state: pressed" "$CLIENT_LOG" &&
    grep -q "key key: 34, .*state: pressed" "$CLIENT_LOG" &&
    grep -q "key key: 46, .*state: pressed" "$CLIENT_LOG" &&
    grep -q "key key: 22, .*state: released" "$CLIENT_LOG" &&
    grep -q "key key: 34, .*state: released" "$CLIENT_LOG" &&
    grep -q "key key: 46, .*state: released" "$CLIENT_LOG" &&
    grep -q "axis time: .*axis: vertical" "$CLIENT_LOG"; then
    break
  fi
  sleep 0.1
done

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
grep -q "key key: 34, .*state: pressed" "$CLIENT_LOG" || {
  echo "ERR eventdemo did not log g key press" >&2
  cat "$CLIENT_LOG" >&2 || true
  exit 1
}
grep -q "key key: 34, .*state: released" "$CLIENT_LOG" || {
  echo "ERR eventdemo did not log g key release" >&2
  cat "$CLIENT_LOG" >&2 || true
  exit 1
}
grep -q "key key: 46, .*state: pressed" "$CLIENT_LOG" || {
  echo "ERR eventdemo did not log c key press" >&2
  cat "$CLIENT_LOG" >&2 || true
  exit 1
}
grep -q "key key: 46, .*state: released" "$CLIENT_LOG" || {
  echo "ERR eventdemo did not log c key release" >&2
  cat "$CLIENT_LOG" >&2 || true
  exit 1
}
grep -q "axis time: .*axis: vertical" "$CLIENT_LOG" || {
  echo "ERR eventdemo did not log vertical axis" >&2
  cat "$CLIENT_LOG" >&2 || true
  exit 1
}

echo "OK headless input forwarding smoke passed"
echo "status_mapped=$STATUS_MAPPED"
echo "status_forwarded=$STATUS_FORWARDED"
echo "status_mode_normal=$STATUS_MODE_NORMAL"
echo "status_mode_passthrough=$STATUS_MODE_PASSTHROUGH"
echo "client_log=$CLIENT_LOG"
echo "log=$LOG"

ctl quit >/dev/null || true
wait "$PID" || true
trap - EXIT
