#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

if ! command -v weston-simple-shm >/dev/null 2>&1; then
  echo "ERR weston-simple-shm is required for startup attach fallback smoke" >&2
  exit 2
fi

cargo build -p manifoldd --offline >/dev/null

TMPDIR="$(mktemp -d /tmp/manifold-startup-attach-fallback.XXXXXX)"
DBUS_NAME="org.manifold.Test.p$$"
LOG="$TMPDIR/manifoldd.log"
CLIENT_LOG="$TMPDIR/client.log"
WRAPPER="$TMPDIR/client-wrapper.sh"

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
exec weston-simple-shm >"$CLIENT_LOG" 2>&1
EOF
chmod +x "$WRAPPER"

env WAYLAND_DISPLAY=manifold-missing-wayland DISPLAY= \
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

STATUS=""
for _ in $(seq 1 100); do
  STATUS="$(ctl status)"
  if [[ "$(field "$STATUS" presentation)" == "Detached" ]] &&
    [[ "$(field "$STATUS" presentation_attach_failures)" =~ ^[0-9]+$ ]] &&
    (( "$(field "$STATUS" presentation_attach_failures)" >= 1 )) &&
    [[ "$(field "$STATUS" presentation_last_error)" != "none" ]] &&
    [[ "$(field "$STATUS" windows)" =~ ^[0-9]+$ ]] &&
    (( "$(field "$STATUS" windows)" >= 1 )) &&
    [[ "$(field "$STATUS" commits)" =~ ^[0-9]+$ ]] &&
    (( "$(field "$STATUS" commits)" >= 1 )); then
    break
  fi
  sleep 0.1
done

[[ "$(field "$STATUS" presentation)" == "Detached" ]] || {
  echo "ERR expected startup attach failure to continue detached: $STATUS" >&2
  tail -120 "$LOG" >&2 || true
  exit 1
}
[[ "$(field "$STATUS" presentation_attach_failures)" =~ ^[0-9]+$ ]] &&
  (( "$(field "$STATUS" presentation_attach_failures)" >= 1 )) || {
  echo "ERR expected startup attach failure count: $STATUS" >&2
  exit 1
}
[[ "$(field "$STATUS" presentation_runtime_failures)" == "0" ]] || {
  echo "ERR startup attach failure should not count as runtime redraw failure: $STATUS" >&2
  exit 1
}
[[ "$(field "$STATUS" presentation_last_error)" != "none" ]] || {
  echo "ERR expected startup attach failure to report presentation_last_error: $STATUS" >&2
  exit 1
}
[[ "$(field "$STATUS" windows)" =~ ^[0-9]+$ ]] &&
  (( "$(field "$STATUS" windows)" >= 1 )) || {
  echo "ERR expected nested client to map despite failed startup frontend: $STATUS" >&2
  exit 1
}
[[ "$(field "$STATUS" commits)" =~ ^[0-9]+$ ]] &&
  (( "$(field "$STATUS" commits)" >= 1 )) || {
  echo "ERR expected nested client commits despite failed startup frontend: $STATUS" >&2
  exit 1
}

echo "OK startup attach fallback smoke passed"
echo "status=$STATUS"
echo "client_log=$CLIENT_LOG"
echo "log=$LOG"

ctl quit >/dev/null || true
wait "$PID" || true
trap - EXIT
