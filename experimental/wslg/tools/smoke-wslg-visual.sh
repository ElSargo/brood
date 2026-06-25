#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

require_host_presentation_environment

CLIENT="${MANIFOLD_SMOKE_CLIENT:-}"
if [[ -z "$CLIENT" ]]; then
  if command -v weston-simple-egl >/dev/null 2>&1; then
    CLIENT="$(command -v weston-simple-egl)"
  elif command -v weston-simple-shm >/dev/null 2>&1; then
    CLIENT="$(command -v weston-simple-shm)"
  else
    echo "ERR no visual smoke client found; install weston-simple-egl or set MANIFOLD_SMOKE_CLIENT" >&2
    exit 2
  fi
fi

cargo build -p manifoldd --offline >/dev/null

exec 9>/tmp/manifold-host-smoke.lock
flock 9

TMPDIR="$(mktemp -d /tmp/manifold-visual-smoke.XXXXXX)"
DBUS_NAME="org.manifold.Test.p$$"
LOG="$TMPDIR/manifoldd.log"
TRACE="$TMPDIR/client-wayland.log"
WRAPPER="$TMPDIR/client-wrapper.sh"
CAPTURE_PS1="$ROOT/experimental/wslg/tools/wslg-capture-manifold-window.ps1"
PNG_INITIAL="$TMPDIR/manifold-window-initial.png"
PNG_REATTACHED="$TMPDIR/manifold-window-reattached.png"

cleanup() {
  if [[ -n "${PID:-}" ]] && kill -0 "$PID" 2>/dev/null; then
    target/debug/manifoldd ctl --dbus-name "$DBUS_NAME" quit >/dev/null 2>&1 || true
    sleep 0.5
    kill "$PID" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

printf '#!/usr/bin/env bash\nexec env WAYLAND_DEBUG=1 %q 2>%q\n' "$CLIENT" "$TRACE" >"$WRAPPER"
chmod +x "$WRAPPER"

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

wait_attached_visual_status() {
  local min_frames="$1"
  local status
  for _ in $(seq 1 100); do
    status="$(ctl status)"
    if [[ "$(field "$status" presentation)" == "Attached" ]] &&
      [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] &&
      (( "$(field "$status" windows)" >= 1 )) &&
      [[ "$(field "$status" frames)" =~ ^[0-9]+$ ]] &&
      (( "$(field "$status" frames)" >= min_frames )); then
      echo "$status"
      return 0
    fi
    sleep 0.1
  done
  echo "ERR hosted client did not reach visual capture readiness" >&2
  ctl status >&2 || true
  tail -120 "$LOG" >&2 || true
  exit 1
}

wait_detached_with_window() {
  local status
  for _ in $(seq 1 100); do
    status="$(ctl status)"
    if [[ "$(field "$status" presentation)" == "Detached" ]] &&
      [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] &&
      (( "$(field "$status" windows)" >= 1 )); then
      echo "$status"
      return 0
    fi
    sleep 0.1
  done
  echo "ERR frontend did not detach while preserving hosted window" >&2
  ctl status >&2 || true
  tail -120 "$LOG" >&2 || true
  exit 1
}

capture_window() {
  local png="$1"
  local result
  local rc
  for _ in $(seq 1 20); do
    set +e
    result="$(powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$(wslpath -w "$CAPTURE_PS1")" "$(wslpath -w "$png")" 2>&1 | tr -d '\r')"
    rc=$?
    set -e
    if [[ "$rc" == "0" && -s "$png" ]]; then
      echo "$result"
      return 0
    fi
    sleep 0.1
  done
  echo "ERR visual capture failed for $png" >&2
  echo "$result" >&2
  exit 1
}

STATUS_INITIAL="$(wait_attached_visual_status 8)"
FRAMES_INITIAL="$(field "$STATUS_INITIAL" frames)"
ATTACHES_INITIAL="$(field "$STATUS_INITIAL" attach_count)"
if [[ ! "$FRAMES_INITIAL" =~ ^[0-9]+$ ]] || [[ ! "$ATTACHES_INITIAL" =~ ^[0-9]+$ ]]; then
  echo "ERR missing frame or attach counters in status: $STATUS_INITIAL" >&2
  exit 1
fi

CAPTURE_INITIAL="$(capture_window "$PNG_INITIAL")"

ctl detach >/dev/null
STATUS_DETACHED="$(wait_detached_with_window)"

ctl attach >/dev/null
STATUS_REATTACHED="$(wait_attached_visual_status "$((FRAMES_INITIAL + 1))")"
ATTACHES_REATTACHED="$(field "$STATUS_REATTACHED" attach_count)"
if [[ ! "$ATTACHES_REATTACHED" =~ ^[0-9]+$ ]] || (( ATTACHES_REATTACHED <= ATTACHES_INITIAL )); then
  echo "ERR attach counter did not advance after visual resume: $STATUS_REATTACHED" >&2
  exit 1
fi
CAPTURE_REATTACHED="$(capture_window "$PNG_REATTACHED")"

echo "OK WSLg visual smoke passed"
echo "client=$CLIENT"
echo "status_initial=$STATUS_INITIAL"
echo "status_detached=$STATUS_DETACHED"
echo "status_reattached=$STATUS_REATTACHED"
echo "capture_initial=$CAPTURE_INITIAL"
echo "capture_reattached=$CAPTURE_REATTACHED"
echo "png_initial=$PNG_INITIAL"
echo "png_reattached=$PNG_REATTACHED"
echo "log=$LOG"
echo "wayland_trace=$TRACE"

ctl quit >/dev/null || true
wait "$PID" || true
trap - EXIT
