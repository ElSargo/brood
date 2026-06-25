#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

cargo build -p manifoldd >/dev/null

TMPDIR="$(mktemp -d /tmp/manifold-session-dbus-gtk.XXXXXX)"
CONF="$TMPDIR/control-bus.conf"
BUS_INFO="$TMPDIR/control-bus.info"
LOG="$TMPDIR/manifoldd.log"
SERVER_READY="$TMPDIR/server.ready"
SERVER_CLICK="$TMPDIR/server.click"
CLIENT_OTHER="$TMPDIR/client-other.out"
CLIENT_SAME="$TMPDIR/client-same.out"
SERVICE_NAME="org.manifold.TestSessionServer.p$$"
DBUS_NAME="org.manifold.Test.SessionDbusGtk.p$$"

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
  target/debug/manifoldd ctl --dbus-address "$CONTROL_ADDRESS" --dbus-name "$DBUS_NAME" quit >/dev/null 2>&1 || true
  if [[ -n "${MANIFOLD_PID:-}" ]] && kill -0 "$MANIFOLD_PID" 2>/dev/null; then
    kill "$MANIFOLD_PID" >/dev/null 2>&1 || true
  fi
  kill "$CONTROL_BUS_PID" >/dev/null 2>&1 || true
}
trap cleanup EXIT

RUST_LOG="${RUST_LOG:-manifoldd=info,smithay=warn}" \
  target/debug/manifoldd run --headless --no-default-modules --dbus-address "$CONTROL_ADDRESS" --dbus-name "$DBUS_NAME" >"$LOG" 2>&1 &
MANIFOLD_PID=$!

ctl() {
  target/debug/manifoldd ctl --dbus-address "$CONTROL_ADDRESS" --dbus-name "$DBUS_NAME" "$@"
}

call() {
  busctl --address "$CONTROL_ADDRESS" call "$DBUS_NAME" "$@"
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
  for _ in $(seq 1 100); do
    if [[ -s "$path" ]] && grep -q "$pattern" "$path"; then
      return 0
    fi
    sleep 0.1
  done
  echo "ERR timed out waiting for $label" >&2
  [[ -e "$path" ]] && cat "$path" >&2 || true
  tail -120 "$LOG" >&2 || true
  exit 1
}

active_session() {
  ctl status-json | python3 -c 'import json,sys; print(json.load(sys.stdin)["active_surface_session"])'
}

wait_for_active_session() {
  local expected="$1"
  for _ in $(seq 1 100); do
    if [[ "$(active_session)" == "$expected" ]]; then
      return 0
    fi
    sleep 0.1
  done
  echo "ERR timed out waiting for active session $expected; current=$(active_session)" >&2
  ctl status-json >&2 || true
  exit 1
}

wait_for_control

assert_no_spawned_session_dbus_daemon() {
  local matches
  matches="$(pgrep -af "dbus-daemon.*manifold-session-dbus-${MANIFOLD_PID}-" || true)"
  if [[ -n "$matches" ]]; then
    echo "ERR Manifold spawned a per-session dbus-daemon:" >&2
    echo "$matches" >&2
    exit 1
  fi
}

SESSION_TWO="$(call /org/manifold/Session org.manifold.Session CreateSurfaceSession s dbus-other | sed -n 's/^t //p')"
[[ -n "$SESSION_TWO" ]] || {
  echo "ERR failed to create second session" >&2
  exit 1
}

call /org/manifold/Session org.manifold.Session ActivateSurfaceSession t 1 >/dev/null
wait_for_active_session 1

ctl spawn python3 "$ROOT/tools/dbus-session-server-gtk.py" \
  --service-name "$SERVICE_NAME" \
  --target-session "$SESSION_TWO" \
  --ready-file "$SERVER_READY" \
  --click-file "$SERVER_CLICK" >/dev/null
wait_for_file_matching "$SERVER_READY" "ready=true" "GTK DBus server registration"
SERVER_SESSION_DBUS_ADDRESS="$(sed -n 's/^session_bus_address=//p' "$SERVER_READY")"
[[ "$SERVER_SESSION_DBUS_ADDRESS" == unix:path=*manifold-inprocess-dbus-* ]] || {
  echo "ERR GTK server did not receive a Manifold in-process DBus address" >&2
  cat "$SERVER_READY" >&2
  exit 1
}
assert_no_spawned_session_dbus_daemon

call /org/manifold/Session org.manifold.Session ActivateSurfaceSession t "$SESSION_TWO" >/dev/null
wait_for_active_session "$SESSION_TWO"
ctl spawn python3 "$ROOT/tools/dbus-session-client.py" \
  --service-name "$SERVICE_NAME" \
  --out-file "$CLIENT_OTHER" >/dev/null
wait_for_file_matching "$CLIENT_OTHER" "has_owner=false" "different-session client miss"

call /org/manifold/Session org.manifold.Session ActivateSurfaceSession t 1 >/dev/null
wait_for_active_session 1
ctl spawn python3 "$ROOT/tools/dbus-session-client.py" \
  --service-name "$SERVICE_NAME" \
  --out-file "$CLIENT_SAME" >/dev/null
wait_for_file_matching "$CLIENT_SAME" "has_owner=true" "same-session client hit"
wait_for_file_matching "$CLIENT_SAME" "ping=pong" "same-session client Ping"

ctl inject center-motion >/dev/null
ctl inject button 272 pressed >/dev/null
ctl inject button 272 released >/dev/null
wait_for_file_matching "$SERVER_CLICK" "clicked=true" "GTK switcher click"
wait_for_active_session "$SESSION_TWO"

ctl quit >/dev/null || true
wait "$MANIFOLD_PID" || true
kill "$CONTROL_BUS_PID" >/dev/null 2>&1 || true
trap - EXIT

echo "OK session DBus isolation GTK smoke passed"
echo "service_name=$SERVICE_NAME"
echo "server_ready=$SERVER_READY"
echo "client_other=$(tr '\n' ' ' <"$CLIENT_OTHER")"
echo "client_same=$(tr '\n' ' ' <"$CLIENT_SAME")"
echo "clicked=$(tr '\n' ' ' <"$SERVER_CLICK")"
echo "activated_session=$SESSION_TWO"
