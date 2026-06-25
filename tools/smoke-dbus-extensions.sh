#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

cargo build -p manifoldd >/dev/null

TMPDIR="$(mktemp -d /tmp/manifold-dbus-extensions.XXXXXX)"
CONF="$TMPDIR/session.conf"
BUS_INFO="$TMPDIR/bus.info"
LOG="$TMPDIR/manifoldd.log"
CLIENT_PID_FILE="$TMPDIR/client.pid"
CLIENT_LOG="$TMPDIR/client.log"
CLIENT_WRAPPER="$TMPDIR/client-wrapper.sh"
PROBE="$TMPDIR/probe.sh"
PROBE_ENV="$TMPDIR/probe.env"
PROBE_STATUS="$TMPDIR/probe-status.json"
DBUS_NAME="org.manifold.Test.DBusExtensions.p$$"

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
DBUS_ADDRESS="$(sed -n '1p' "$BUS_INFO")"
DBUS_PID="$(sed -n '2p' "$BUS_INFO")"

cleanup() {
  target/debug/manifoldd ctl --dbus-address "$DBUS_ADDRESS" --dbus-name "$DBUS_NAME" quit >/dev/null 2>&1 || true
  if [[ -n "${MANIFOLD_PID:-}" ]] && kill -0 "$MANIFOLD_PID" 2>/dev/null; then
    kill "$MANIFOLD_PID" >/dev/null 2>&1 || true
  fi
  kill "$DBUS_PID" >/dev/null 2>&1 || true
}
trap cleanup EXIT

cat >"$CLIENT_WRAPPER" <<EOF
#!/usr/bin/env bash
echo "\$\$" >"$CLIENT_PID_FILE"
exec weston-simple-shm >"$CLIENT_LOG" 2>&1
EOF
chmod +x "$CLIENT_WRAPPER"

RUST_LOG="${RUST_LOG:-manifoldd=info,smithay=warn}" \
  target/debug/manifoldd run --headless --no-default-modules --dbus-address "$DBUS_ADDRESS" --dbus-name "$DBUS_NAME" -c "$CLIENT_WRAPPER" >"$LOG" 2>&1 &
MANIFOLD_PID=$!

ctl() {
  target/debug/manifoldd ctl --dbus-address "$DBUS_ADDRESS" --dbus-name "$DBUS_NAME" "$@"
}

call() {
  busctl --address "$DBUS_ADDRESS" call "$DBUS_NAME" "$@"
}

extract_string() {
  sed -n 's/^s "\([^"]*\)".*/\1/p'
}

wait_for_control() {
  for _ in $(seq 1 100); do
    if ctl status-json >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.1
  done
  echo "ERR timed out waiting for Manifold DBus service" >&2
  tail -120 "$LOG" >&2 || true
  exit 1
}

wait_for_client_window() {
  for _ in $(seq 1 100); do
    if [[ -s "$CLIENT_PID_FILE" ]] && ctl status-json | python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin).get("windows", 0) >= 1 else 1)' 2>/dev/null; then
      return 0
    fi
    sleep 0.1
  done
  echo "ERR timed out waiting for hosted Wayland client" >&2
  cat "$CLIENT_LOG" >&2 || true
  tail -120 "$LOG" >&2 || true
  exit 1
}

wait_for_probe() {
  for _ in $(seq 1 100); do
    if [[ -s "$PROBE_ENV" && -s "$PROBE_STATUS" ]]; then
      return 0
    fi
    sleep 0.1
  done
  echo "ERR timed out waiting for managed-client DBus probe" >&2
  tail -120 "$LOG" >&2 || true
  exit 1
}

wait_for_control
wait_for_client_window
CLIENT_PID="$(cat "$CLIENT_PID_FILE")"

CREATED_OUT="$(call /org/manifold/Session org.manifold.Session CreateSurfaceSession s dbus-smoke)"
[[ "$CREATED_OUT" =~ ^t\  ]] || {
  echo "ERR CreateSurfaceSession returned unexpected output: $CREATED_OUT" >&2
  exit 1
}
CREATED_ID="${CREATED_OUT#t }"

ACTIVE_OUT="$(call /org/manifold/Manager org.manifold.Manager ActiveSession)"
[[ "$ACTIVE_OUT" == "t $CREATED_ID" ]] || {
  echo "ERR ActiveSession did not return created session: $ACTIVE_OUT created=$CREATED_ID" >&2
  exit 1
}

RESOLVED_OUT="$(call /org/manifold/Manager org.manifold.Manager ResolveSession s dbus-smoke)"
[[ "$RESOLVED_OUT" == "t $CREATED_ID" ]] || {
  echo "ERR ResolveSession did not resolve by name: $RESOLVED_OUT created=$CREATED_ID" >&2
  exit 1
}

call /org/manifold/Session org.manifold.Session ActivateSurfaceSession t 1 >/dev/null

EXT_ID="$(call /org/manifold/Extension org.manifold.Extension RegisterExtension 'sa{ss}' session-switcher 2 name smoke role session-switcher | extract_string)"
EXT_ID_AGAIN="$(call /org/manifold/Extension org.manifold.Extension RegisterExtension 'sa{ss}' session-switcher 2 name smoke role session-switcher | extract_string)"
[[ -n "$EXT_ID" && "$EXT_ID" == "$EXT_ID_AGAIN" ]] || {
  echo "ERR extension registration was not stable: first=$EXT_ID second=$EXT_ID_AGAIN" >&2
  exit 1
}

WINDOW_ID="$(call /org/manifold/Compositor org.manifold.Compositor RegisterWindow 'sa{ss}s' "$EXT_ID" 1 pid "$CLIENT_PID" session-switcher | extract_string)"
[[ -n "$WINDOW_ID" ]] || {
  echo "ERR RegisterWindow did not return a window id" >&2
  exit 1
}

MEMBERSHIP_GRANT="$(call /org/manifold/Compositor org.manifold.Compositor RequestPresentationMembership 'sa{ss}' "$WINDOW_ID" 1 audience Presentation | extract_string)"
PLACEMENT_GRANT="$(call /org/manifold/Compositor org.manifold.Compositor RequestPlacement 'sa{ss}' "$WINDOW_ID" 3 mode Overlay audience Presentation unsupported-field recorded | extract_string)"
SHORTCUT_GRANT="$(call /org/manifold/Compositor org.manifold.Compositor RequestShortcut ssss "$EXT_ID" Ctrl+Alt+Space Presentation 'Open session switcher' | extract_string)"

[[ -n "$MEMBERSHIP_GRANT" && -n "$PLACEMENT_GRANT" && -n "$SHORTCUT_GRANT" ]] || {
  echo "ERR one or more grant requests failed: membership=$MEMBERSHIP_GRANT placement=$PLACEMENT_GRANT shortcut=$SHORTCUT_GRANT" >&2
  exit 1
}

GRANTS_BEFORE="$(ctl extensions grants "$EXT_ID")"
grep -q "grant id=$MEMBERSHIP_GRANT" <<<"$GRANTS_BEFORE" || {
  echo "ERR membership grant not listed: $GRANTS_BEFORE" >&2
  exit 1
}
grep -q "grant id=$PLACEMENT_GRANT" <<<"$GRANTS_BEFORE" || {
  echo "ERR placement grant not listed: $GRANTS_BEFORE" >&2
  exit 1
}
grep -q "unsupported-field=recorded" <<<"$GRANTS_BEFORE" || {
  echo "ERR unsupported placement field was not recorded: $GRANTS_BEFORE" >&2
  exit 1
}
grep -q "grant id=$SHORTCUT_GRANT" <<<"$GRANTS_BEFORE" || {
  echo "ERR shortcut grant not listed: $GRANTS_BEFORE" >&2
  exit 1
}

call /org/manifold/Extension org.manifold.Extension ReleaseGrant s "$PLACEMENT_GRANT" >/dev/null
GRANTS_AFTER="$(ctl extensions grants "$EXT_ID")"
grep -q "grant id=$PLACEMENT_GRANT .* active=false" <<<"$GRANTS_AFTER" || {
  echo "ERR released grant was not marked inactive: $GRANTS_AFTER" >&2
  exit 1
}

call /org/manifold/Session org.manifold.Session ActivateSurfaceSession t "$CREATED_ID" >/dev/null
WINDOWS_AFTER_SWITCH="$(ctl status-json | python3 -c 'import json,sys; print(json.load(sys.stdin).get("windows", 0))')"
[[ "$WINDOWS_AFTER_SWITCH" -ge 1 ]] || {
  echo "ERR presentation member window disappeared after session switch" >&2
  ctl status-json >&2 || true
  exit 1
}

cat >"$PROBE" <<EOF
#!/usr/bin/env bash
set -euo pipefail
{
  printf "DBUS_SESSION_BUS_ADDRESS=%s\\n" "\${DBUS_SESSION_BUS_ADDRESS:-}"
  printf "MANIFOLDD_DBUS_DESTINATION=%s\\n" "\${MANIFOLDD_DBUS_DESTINATION:-}"
} >"$PROBE_ENV"
RUST_LOG=off "$ROOT/target/debug/manifoldd" ctl status-json >"$PROBE_STATUS"
EOF
chmod +x "$PROBE"
ctl spawn "$PROBE" >/dev/null
wait_for_probe

SESSION_DBUS_ADDRESS="$(sed -n 's/^DBUS_SESSION_BUS_ADDRESS=//p' "$PROBE_ENV")"
[[ -n "$SESSION_DBUS_ADDRESS" && "$SESSION_DBUS_ADDRESS" != "$DBUS_ADDRESS" ]] || {
  echo "ERR managed client did not receive a session-local DBus address" >&2
  cat "$PROBE_ENV" >&2
  exit 1
}
python3 - "$PROBE_STATUS" <<'PY'
import json
import sys
with open(sys.argv[1], "r", encoding="utf-8") as handle:
    data = json.load(handle)
if "active_surface_session" not in data:
    raise SystemExit(f"probe did not target Manifold status JSON: {data}")
PY

EXTENSIONS="$(ctl extensions list)"
grep -q "extension id=$EXT_ID" <<<"$EXTENSIONS" || {
  echo "ERR typed ctl extension listing missed registered extension: $EXTENSIONS" >&2
  exit 1
}

ctl quit >/dev/null || true
wait "$MANIFOLD_PID" || true
kill "$DBUS_PID" >/dev/null 2>&1 || true
trap - EXIT

echo "OK DBus extension smoke passed"
echo "extension_id=$EXT_ID"
echo "window_id=$WINDOW_ID"
echo "membership_grant=$MEMBERSHIP_GRANT"
echo "placement_grant=$PLACEMENT_GRANT"
echo "shortcut_grant=$SHORTCUT_GRANT"
echo "created_session=$CREATED_ID"
