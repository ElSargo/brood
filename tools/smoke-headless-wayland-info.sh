#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

if ! command -v wayland-info > /dev/null 2>&1; then
  echo "ERR wayland-info is required for headless Wayland global smoke" >&2
  exit 2
fi

cargo build -p manifoldd --offline > /dev/null

TMPDIR="$(mktemp -d /tmp/manifold-headless-wayland-info.XXXXXX)"
DBUS_NAME="org.manifold.Test.p$$"
LOG="$TMPDIR/manifoldd.log"
INFO="$TMPDIR/wayland-info.log"

cleanup() {
  if [[ -n "${PID:-}" ]] && kill -0 "$PID" 2> /dev/null; then
    target/debug/manifold ctl --dbus-name "$DBUS_NAME" quit > /dev/null 2>&1 || true
    sleep 0.5
    kill "$PID" > /dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

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

STATUS="$(ctl status)"
[[ "$(field "$STATUS" presentation)" == "Detached" ]] || {
  echo "ERR expected headless startup to be detached: $STATUS" >&2
  exit 1
}
require_field_ge "$STATUS" shm_formats 2

NESTED_DISPLAY="$(field "$STATUS" wayland_display)"
[[ -n "$NESTED_DISPLAY" ]] || {
  echo "ERR missing nested wayland_display in status: $STATUS" >&2
  exit 1
}

env WAYLAND_DISPLAY="$NESTED_DISPLAY" timeout 8 wayland-info > "$INFO" 2>&1 || {
  echo "ERR wayland-info could not inspect nested compositor" >&2
  echo "status=$STATUS" >&2
  cat "$INFO" >&2 || true
  tail -120 "$LOG" >&2 || true
  exit 1
}

for global in wl_compositor wl_shm wl_seat xdg_wm_base; do
  grep -q "interface: '$global'" "$INFO" || {
    echo "ERR wayland-info did not report required global $global" >&2
    echo "status=$STATUS" >&2
    cat "$INFO" >&2 || true
    exit 1
  }
done

grep -Eq "'XRGB8888'|'XR24'" "$INFO" || {
  echo "ERR nested wl_shm did not advertise XRGB8888/XR24" >&2
  cat "$INFO" >&2 || true
  exit 1
}
grep -Eq "'ARGB8888'|'AR24'" "$INFO" || {
  echo "ERR nested wl_shm did not advertise ARGB8888/AR24" >&2
  cat "$INFO" >&2 || true
  exit 1
}

echo "OK headless Wayland global smoke passed"
echo "status=$STATUS"
echo "wayland_info=$INFO"
echo "log=$LOG"

ctl quit > /dev/null || true
wait "$PID" || true
trap - EXIT
