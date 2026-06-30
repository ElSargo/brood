#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

if ! command -v weston-eventdemo > /dev/null 2>&1; then
  echo "ERR weston-eventdemo is required for headless multi-client input focus smoke" >&2
  exit 2
fi

cargo build -p manifoldd --offline > /dev/null

TMPDIR="$(mktemp -d /tmp/manifold-headless-multi-input.XXXXXX)"
DBUS_NAME="org.manifold.Test.p$$"
LOG="$TMPDIR/manifoldd.log"
CLIENT1_LOG="$TMPDIR/eventdemo-1.log"
CLIENT2_LOG="$TMPDIR/eventdemo-2.log"
CLIENT1_PIDFILE="$TMPDIR/eventdemo-1.pid"
CLIENT2_PIDFILE="$TMPDIR/eventdemo-2.pid"
WRAPPER1="$TMPDIR/eventdemo-1-wrapper.sh"
WRAPPER2="$TMPDIR/eventdemo-2-wrapper.sh"

cleanup() {
  for pidfile in "$CLIENT1_PIDFILE" "$CLIENT2_PIDFILE"; do
    if [[ -s "$pidfile" ]]; then
      kill "$(cat "$pidfile")" > /dev/null 2>&1 || true
    fi
  done
  if [[ -n "${PID:-}" ]] && kill -0 "$PID" 2> /dev/null; then
    target/debug/manifold ctl --dbus-name "$DBUS_NAME" quit > /dev/null 2>&1 || true
    sleep 0.5
    kill "$PID" > /dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

cat > "$WRAPPER1" << EOF
#!/usr/bin/env bash
echo "\$\$" >"$CLIENT1_PIDFILE"
exec stdbuf -oL -eL weston-eventdemo \
  --log-motion --log-button --log-key --log-focus --log-redraw --log-resize \
  >"$CLIENT1_LOG" 2>&1
EOF
cat > "$WRAPPER2" << EOF
#!/usr/bin/env bash
echo "\$\$" >"$CLIENT2_PIDFILE"
exec stdbuf -oL -eL weston-eventdemo \
  --log-motion --log-button --log-key --log-focus --log-redraw --log-resize \
  >"$CLIENT2_LOG" 2>&1
EOF
chmod +x "$WRAPPER1" "$WRAPPER2"

env WAYLAND_DISPLAY=manifold-missing-wayland DISPLAY= \
  RUST_LOG="${RUST_LOG:-manifoldd=info,smithay=warn}" \
  target/debug/manifold run --headless --dbus-name "$DBUS_NAME" > "$LOG" 2>&1 &
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
  cat "$CLIENT1_LOG" "$CLIENT2_LOG" >&2 2> /dev/null || true
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

event_count() {
  local file="$1"
  local pattern="$2"
  if [[ -s "$file" ]]; then
    grep -c "$pattern" "$file" || true
  else
    printf '0'
  fi
}

wait_event_count_ge() {
  local name="$1"
  local file="$2"
  local pattern="$3"
  local min="$4"
  local count
  for _ in $(seq 1 50); do
    count="$(event_count "$file" "$pattern")"
    if ((count >= min)); then
      printf '%s' "$count"
      return 0
    fi
    sleep 0.1
  done
  echo "ERR expected $name count >= $min" >&2
  echo "client1_log=$CLIENT1_LOG" >&2
  cat "$CLIENT1_LOG" >&2 || true
  echo "client2_log=$CLIENT2_LOG" >&2
  cat "$CLIENT2_LOG" >&2 || true
  exit 1
}

inject_click_and_key() {
  local x="$1"
  local y="$2"
  ctl inject motion "$x" "$y" > /dev/null
  sleep 0.05
  ctl inject button 272 pressed > /dev/null
  ctl inject button 272 released > /dev/null
  ctl inject key 30 pressed > /dev/null
  ctl inject key 30 released > /dev/null
}

ctl spawn "$WRAPPER1" > /dev/null
wait_pidfile "$CLIENT1_PIDFILE"
STATUS_ONE="$(wait_status '[[ "$(field "$status" presentation)" == "Detached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" == 1 )) && [[ "$(field "$status" commits)" =~ ^[0-9]+$ ]] && (( "$(field "$status" commits)" >= 1 ))')"

ctl spawn "$WRAPPER2" > /dev/null
wait_pidfile "$CLIENT2_PIDFILE"
STATUS_TWO="$(wait_status '[[ "$(field "$status" presentation)" == "Detached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 2 )) && [[ "$(field "$status" commits)" =~ ^[0-9]+$ ]] && (( "$(field "$status" commits)" >= 2 ))')"

CLIENT1_KEY_BASE="$(event_count "$CLIENT1_LOG" 'key key: .*state: pressed')"
CLIENT2_KEY_BASE="$(event_count "$CLIENT2_LOG" 'key key: .*state: pressed')"

# The second window is mapped above the first at an offset, so this point targets it.
inject_click_and_key 100 60
CLIENT2_KEY_AFTER="$(wait_event_count_ge "second client key after top-window click" "$CLIENT2_LOG" 'key key: .*state: pressed' "$((CLIENT2_KEY_BASE + 1))")"
CLIENT1_KEY_AFTER_TOP="$(event_count "$CLIENT1_LOG" 'key key: .*state: pressed')"
[[ "$CLIENT1_KEY_AFTER_TOP" == "$CLIENT1_KEY_BASE" ]] || {
  echo "ERR top-window key was also delivered to first client" >&2
  cat "$CLIENT1_LOG" >&2 || true
  cat "$CLIENT2_LOG" >&2 || true
  exit 1
}

# The left strip remains exposed because windows cascade by 40px.
inject_click_and_key 10 60
CLIENT1_KEY_AFTER="$(wait_event_count_ge "first client key after exposed-strip click" "$CLIENT1_LOG" 'key key: .*state: pressed' "$((CLIENT1_KEY_BASE + 1))")"
CLIENT2_KEY_AFTER_FIRST="$(event_count "$CLIENT2_LOG" 'key key: .*state: pressed')"
[[ "$CLIENT2_KEY_AFTER_FIRST" == "$CLIENT2_KEY_AFTER" ]] || {
  echo "ERR exposed-strip key was also delivered to second client" >&2
  cat "$CLIENT1_LOG" >&2 || true
  cat "$CLIENT2_LOG" >&2 || true
  exit 1
}

STATUS_ROUTED="$(wait_status '[[ "$(field "$status" pointer_target_hits)" =~ ^[0-9]+$ ]] && (( "$(field "$status" pointer_target_hits)" >= 2 )) && [[ "$(field "$status" pointer_button_events)" =~ ^[0-9]+$ ]] && (( "$(field "$status" pointer_button_events)" >= 4 )) && [[ "$(field "$status" keyboard_events)" =~ ^[0-9]+$ ]] && (( "$(field "$status" keyboard_events)" >= 4 ))')"

wait_event_count_ge "first client focus" "$CLIENT1_LOG" 'focus x:' 1 > /dev/null
wait_event_count_ge "second client focus" "$CLIENT2_LOG" 'focus x:' 1 > /dev/null

echo "OK headless multi-client input focus smoke passed"
echo "status_one=$STATUS_ONE"
echo "status_two=$STATUS_TWO"
echo "status_routed=$STATUS_ROUTED"
echo "client1_key_presses=$CLIENT1_KEY_AFTER"
echo "client2_key_presses=$CLIENT2_KEY_AFTER_FIRST"
echo "client1_log=$CLIENT1_LOG"
echo "client2_log=$CLIENT2_LOG"
echo "log=$LOG"

ctl quit > /dev/null || true
wait "$PID" || true
trap - EXIT
