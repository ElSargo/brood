#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

CLIENT="${MANIFOLD_HEADLESS_CLIENT:-}"
if [[ -z "$CLIENT" ]]; then
  if command -v weston-simple-shm >/dev/null 2>&1; then
    CLIENT="$(command -v weston-simple-shm)"
  else
    echo "ERR weston-simple-shm is required for headless native-client smoke" >&2
    exit 2
  fi
fi

cargo build -p manifoldd --offline >/dev/null

TMPDIR="$(mktemp -d /tmp/manifold-headless-native-client.XXXXXX)"
DBUS_NAME="org.manifold.Test.p$$"
LOG="$TMPDIR/manifoldd.log"
CLIENT_LOG="$TMPDIR/client.log"
WRAPPER="$TMPDIR/client-wrapper.sh"

cleanup() {
  if [[ -n "${PID:-}" ]] && kill -0 "$PID" 2>/dev/null; then
    target/debug/manifold ctl --dbus-name "$DBUS_NAME" quit >/dev/null 2>&1 || true
    sleep 0.5
    kill "$PID" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

printf '#!/usr/bin/env bash\nexec %q >%q 2>&1\n' "$CLIENT" "$CLIENT_LOG" >"$WRAPPER"
chmod +x "$WRAPPER"

env WAYLAND_DISPLAY=manifold-missing-wayland DISPLAY= \
  RUST_LOG="${RUST_LOG:-manifoldd=info,smithay=warn}" \
  target/debug/manifold run --headless --dbus-name "$DBUS_NAME" -c "$WRAPPER" >"$LOG" 2>&1 &
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

STATUS1="$(wait_status '[[ "$(field "$status" presentation)" == "Detached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" commits)" =~ ^[0-9]+$ ]] && (( "$(field "$status" commits)" >= 1 ))')"
require_field_ge "$STATUS1" shm_formats 2
require_field_ge "$STATUS1" windows 1
require_field_ge "$STATUS1" commits 1
COMMITS1="$(field "$STATUS1" commits)"

ctl inject center-motion >/dev/null
STATUS_TARGET="$(wait_status '[[ "$(field "$status" pointer_motion_events)" =~ ^[0-9]+$ ]] && (( "$(field "$status" pointer_motion_events)" >= 1 )) && [[ "$(field "$status" pointer_target_hits)" =~ ^[0-9]+$ ]] && (( "$(field "$status" pointer_target_hits)" >= 1 ))')"
require_field_ge "$STATUS_TARGET" pointer_target_hits 1

ctl attach >/dev/null

STATUS2="$(wait_status '[[ "$(field "$status" presentation)" == "Detached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" commits)" =~ ^[0-9]+$ ]] && (( "$(field "$status" commits)" >= COMMITS1 )) && [[ "$(field "$status" presentation_attach_failures)" =~ ^[0-9]+$ ]] && (( "$(field "$status" presentation_attach_failures)" >= 1 )) && [[ "$(field "$status" presentation_runtime_failures)" == "0" ]] && [[ "$(field "$status" presentation_last_error)" != "none" ]]')"
require_field_ge "$STATUS2" windows 1
require_field_ge "$STATUS2" commits "$COMMITS1"
require_field_ge "$STATUS2" presentation_attach_failures 1
[[ "$(field "$STATUS2" presentation_runtime_failures)" == "0" ]] || {
  echo "ERR failed attach should not count as runtime redraw failure: $STATUS2" >&2
  exit 1
}

echo "OK headless native-client smoke passed"
echo "client=$CLIENT"
echo "status_mapped=$STATUS1"
echo "status_targeted=$STATUS_TARGET"
echo "status_after_failed_attach=$STATUS2"
echo "client_log=$CLIENT_LOG"
echo "log=$LOG"

ctl quit >/dev/null || true
wait "$PID" || true
trap - EXIT
