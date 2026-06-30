#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

HOST_DBUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:-}"
if [[ -z "$HOST_DBUS_ADDRESS" ]]; then
  echo "ERR DBUS_SESSION_BUS_ADDRESS is required to show the final desktop notification" >&2
  exit 1
fi

cargo build -p manifoldd >/dev/null

TMPDIR="$(mktemp -d /tmp/manifold-dbus-rube.XXXXXX)"
CONF="$TMPDIR/control-bus.conf"
BUS_INFO="$TMPDIR/control-bus.info"
LOG="$TMPDIR/manifoldd.log"
DBUS_NAME="org.manifold.Test.DBusRube.p$$"
TOKEN="stress-$(date +%s)-$$"

cat >"$CONF" <<EOF
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:tmpdir=$TMPDIR</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow user="*"/>
    <allow own="*"/>
    <allow send_destination="*"/>
    <allow receive_sender="*"/>
  </policy>
</busconfig>
EOF

dbus-daemon --config-file="$CONF" --fork --print-address=1 --print-pid=1 >"$BUS_INFO"
CONTROL_ADDRESS="$(sed -n '1p' "$BUS_INFO")"
CONTROL_BUS_PID="$(sed -n '2p' "$BUS_INFO")"

cleanup() {
  target/debug/manifold ctl --dbus-address "$CONTROL_ADDRESS" --dbus-name "$DBUS_NAME" quit >/dev/null 2>&1 || true
  if [[ -n "${MANIFOLD_PID:-}" ]] && kill -0 "$MANIFOLD_PID" 2>/dev/null; then
    kill "$MANIFOLD_PID" >/dev/null 2>&1 || true
  fi
  kill "$CONTROL_BUS_PID" >/dev/null 2>&1 || true
}
trap cleanup EXIT

RUST_LOG="${RUST_LOG:-manifoldd=info,smithay=warn}" \
  target/debug/manifold run --headless --no-default-modules --dbus-address "$CONTROL_ADDRESS" --dbus-name "$DBUS_NAME" >"$LOG" 2>&1 &
MANIFOLD_PID=$!

ctl() {
  target/debug/manifold ctl --dbus-address "$CONTROL_ADDRESS" --dbus-name "$DBUS_NAME" "$@"
}

wait_for_control() {
  for _ in $(seq 1 100); do
    if ctl status-json >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.1
  done
  echo "ERR timed out waiting for Manifold control bus" >&2
  tail -120 "$LOG" >&2 || true
  exit 1
}

wait_for_file_matching() {
  local path="$1"
  local pattern="$2"
  local label="$3"
  for _ in $(seq 1 150); do
    if [[ -s "$path" ]] && grep -q "$pattern" "$path"; then
      return 0
    fi
    sleep 0.1
  done
  echo "ERR timed out waiting for $label" >&2
  [[ -e "$path" ]] && cat "$path" >&2 || true
  [[ -e "$TMPDIR/events.log" ]] && cat "$TMPDIR/events.log" >&2 || true
  tail -120 "$LOG" >&2 || true
  exit 1
}

assert_no_spawned_session_dbus_daemon() {
  local matches
  matches="$(pgrep -af "dbus-daemon.*manifold-session-dbus-${MANIFOLD_PID}-" || true)"
  if [[ -n "$matches" ]]; then
    echo "ERR Manifold spawned a per-session dbus-daemon:" >&2
    echo "$matches" >&2
    exit 1
  fi
}

wait_for_control

for node in A C D B; do
  ctl spawn env "MANIFOLD_HOST_DBUS_ADDRESS=$HOST_DBUS_ADDRESS" \
    python3 "$ROOT/tools/dbus-rube-goldberg.py" \
    --role node \
    --node "$node" \
    --run-dir "$TMPDIR" \
    --timeout 30 >/dev/null
  wait_for_file_matching "$TMPDIR/ready-$node" "ready=true" "node $node ready"
done

assert_no_spawned_session_dbus_daemon

ctl spawn python3 "$ROOT/tools/dbus-rube-goldberg.py" \
  --role kick \
  --run-dir "$TMPDIR" \
  --token "$TOKEN" \
  --value 41 >/dev/null

wait_for_file_matching "$TMPDIR/kick" "introspected=A,B,C,D" "introspection/properties kick"
wait_for_file_matching "$TMPDIR/done" "done=true" "final notification hop"
wait_for_file_matching "$TMPDIR/done" "notification_id=" "desktop notification id"

grep -q "A Kick token=$TOKEN value=41" "$TMPDIR/events.log" || {
  echo "ERR A method call/property leg missing" >&2
  cat "$TMPDIR/events.log" >&2
  exit 1
}
grep -q "B Started token=$TOKEN" "$TMPDIR/events.log" || {
  echo "ERR B signal/introspection leg missing" >&2
  cat "$TMPDIR/events.log" >&2
  exit 1
}
grep -q "C Transform token=$TOKEN input=42 output=49" "$TMPDIR/events.log" || {
  echo "ERR C method/signal leg missing" >&2
  cat "$TMPDIR/events.log" >&2
  exit 1
}
grep -q "D Transformed token=$TOKEN value=49" "$TMPDIR/events.log" || {
  echo "ERR D signal/notification leg missing" >&2
  cat "$TMPDIR/events.log" >&2
  exit 1
}

ctl quit >/dev/null || true
wait "$MANIFOLD_PID" || true
kill "$CONTROL_BUS_PID" >/dev/null 2>&1 || true
trap - EXIT

echo "OK DBus Rube Goldberg stress smoke passed"
echo "token=$TOKEN"
echo "run_dir=$TMPDIR"
echo "kick=$(tr '\n' ' ' <"$TMPDIR/kick")"
echo "done=$(tr '\n' ' ' <"$TMPDIR/done")"
echo "events=$(tr '\n' '|' <"$TMPDIR/events.log")"
