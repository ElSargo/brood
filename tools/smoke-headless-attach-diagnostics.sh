#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

cargo build -p manifoldd --offline > /dev/null

TMPDIR="$(mktemp -d /tmp/manifold-headless-attach-smoke.XXXXXX)"
DBUS_NAME="org.manifold.Test.p$$"
LOG="$TMPDIR/manifoldd.log"

cleanup() {
  if [[ -n "${PID:-}" ]] && kill -0 "$PID" 2> /dev/null; then
    target/debug/manifold ctl --dbus-name "$DBUS_NAME" quit > /dev/null 2>&1 || true
    sleep 0.5
    kill "$PID" > /dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

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

STATUS1="$(ctl status)"
[[ "$(field "$STATUS1" presentation)" == "Detached" ]] || {
  echo "ERR expected headless startup to be detached: $STATUS1" >&2
  exit 1
}
require_field_ge "$STATUS1" presentation_attach_failures 0
require_field_ge "$STATUS1" presentation_runtime_failures 0
[[ "$(field "$STATUS1" presentation_runtime_failures)" == "0" ]] || {
  echo "ERR expected no runtime failures before attach request: $STATUS1" >&2
  exit 1
}
[[ "$(field "$STATUS1" presentation_last_error)" == "none" ]] || {
  echo "ERR expected no presentation error before attach request: $STATUS1" >&2
  exit 1
}

ctl attach > /dev/null

STATUS2=""
for _ in $(seq 1 100); do
  STATUS2="$(ctl status)"
  if [[ "$(field "$STATUS2" presentation)" == "Detached" ]] &&
    [[ "$(field "$STATUS2" presentation_attach_failures)" =~ ^[0-9]+$ ]] &&
    (("$(field "$STATUS2" presentation_attach_failures)" >= 1)) &&
    [[ "$(field "$STATUS2" presentation_last_error)" != "none" ]]; then
    break
  fi
  sleep 0.1
done

[[ "$(field "$STATUS2" presentation)" == "Detached" ]] || {
  echo "ERR presentation should remain detached after failed attach: $STATUS2" >&2
  exit 1
}
require_field_ge "$STATUS2" presentation_attach_failures 1
[[ "$(field "$STATUS2" presentation_runtime_failures)" == "0" ]] || {
  echo "ERR failed startup attach should not count as runtime redraw failure: $STATUS2" >&2
  exit 1
}
[[ "$(field "$STATUS2" presentation_last_error)" != "none" ]] || {
  echo "ERR expected failed attach to report presentation_last_error: $STATUS2" >&2
  tail -120 "$LOG" >&2 || true
  exit 1
}

echo "OK headless attach diagnostics smoke passed"
echo "status_before=$STATUS1"
echo "status_after=$STATUS2"
echo "log=$LOG"

ctl quit > /dev/null || true
wait "$PID" || true
trap - EXIT
