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
    echo "ERR no detached-spawn-resume smoke client found; install weston-simple-egl or set MANIFOLD_SMOKE_CLIENT" >&2
    exit 2
  fi
fi

cargo build -p manifoldd --offline >/dev/null

exec 9>/tmp/manifold-host-smoke.lock
flock 9

TMPDIR="$(mktemp -d /tmp/manifold-detached-spawn-resume.XXXXXX)"
DBUS_NAME="org.manifold.Test.p$$"
LOG="$TMPDIR/manifoldd.log"
TRACE="$TMPDIR/client-wayland.log"
CLIENT_PIDFILE="$TMPDIR/client.pid"
WRAPPER="$TMPDIR/client-wrapper.sh"
CAPTURE_PS1="$ROOT/experimental/wslg/tools/wslg-capture-manifold-window.ps1"
PNG_REATTACHED="$TMPDIR/manifold-window-detached-spawn-resume.png"

cleanup() {
  if [[ -s "$CLIENT_PIDFILE" ]]; then
    kill "$(cat "$CLIENT_PIDFILE")" >/dev/null 2>&1 || true
  fi
  if [[ -n "${PID:-}" ]] && kill -0 "$PID" 2>/dev/null; then
    target/debug/manifoldd ctl --dbus-name "$DBUS_NAME" quit >/dev/null 2>&1 || true
    sleep 0.5
    kill "$PID" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

cat >"$WRAPPER" <<EOF
#!/usr/bin/env bash
echo "\$\$" >"$CLIENT_PIDFILE"
exec env WAYLAND_DEBUG=1 "$CLIENT" 2>"$TRACE"
EOF
chmod +x "$WRAPPER"

RUST_LOG="${RUST_LOG:-manifoldd=info,smithay=warn}" \
  target/debug/manifoldd run --dbus-name "$DBUS_NAME" >"$LOG" 2>&1 &
PID=$!

wait_for_manifoldd_control "$DBUS_NAME" "$PID" "$LOG"

ctl() {
  target/debug/manifoldd ctl --dbus-name "$DBUS_NAME" "$@"
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

field() {
  local status="$1"
  local key="$2"
  sed -n "s/.* ${key}=\\([^ ]*\\).*/\\1/p" <<<"$status"
}

require_numeric_field_ge() {
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
  tail -120 "$LOG" >&2 || true
  exit 1
}

STATUS_ATTACHED_EMPTY="$(wait_status '[[ "$(field "$status" presentation)" == "Attached" ]] && [[ "$(field "$status" frames)" =~ ^[0-9]+$ ]] && (( "$(field "$status" frames)" >= 2 )) && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" == 0 ))')"
FRAMES_ATTACHED_EMPTY="$(field "$STATUS_ATTACHED_EMPTY" frames)"
ATTACHES_ATTACHED_EMPTY="$(field "$STATUS_ATTACHED_EMPTY" attach_count)"
DETACHES_ATTACHED_EMPTY="$(field "$STATUS_ATTACHED_EMPTY" detach_count)"

ctl detach >/dev/null
STATUS_DETACHED_EMPTY="$(wait_status '[[ "$(field "$status" presentation)" == "Detached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" == 0 )) && [[ "$(field "$status" detach_count)" =~ ^[0-9]+$ ]] && (( "$(field "$status" detach_count)" > DETACHES_ATTACHED_EMPTY ))')"

ctl spawn "$WRAPPER" >/dev/null
for _ in $(seq 1 100); do
  [[ -s "$CLIENT_PIDFILE" ]] && break
  sleep 0.1
done
[[ -s "$CLIENT_PIDFILE" ]] || {
  echo "ERR client pidfile was not written" >&2
  tail -120 "$LOG" >&2 || true
  exit 1
}

STATUS_SPAWNED_DETACHED="$(wait_status '[[ "$(field "$status" presentation)" == "Detached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" commits)" =~ ^[0-9]+$ ]] && (( "$(field "$status" commits)" >= 1 ))')"
require_numeric_field_ge "$STATUS_SPAWNED_DETACHED" windows 1
require_numeric_field_ge "$STATUS_SPAWNED_DETACHED" commits 1
CALLBACKS_SPAWNED_DETACHED="$(field "$STATUS_SPAWNED_DETACHED" client_frame_callbacks)"
[[ "$CALLBACKS_SPAWNED_DETACHED" =~ ^[0-9]+$ ]] || {
  echo "ERR missing client_frame_callbacks in detached spawned status: $STATUS_SPAWNED_DETACHED" >&2
  exit 1
}

ctl attach >/dev/null
STATUS_REATTACHED="$(wait_status '[[ "$(field "$status" presentation)" == "Attached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" attach_count)" =~ ^[0-9]+$ ]] && (( "$(field "$status" attach_count)" > ATTACHES_ATTACHED_EMPTY )) && [[ "$(field "$status" frames)" =~ ^[0-9]+$ ]] && (( "$(field "$status" frames)" > FRAMES_ATTACHED_EMPTY )) && [[ "$(field "$status" client_frame_callbacks)" =~ ^[0-9]+$ ]] && (( "$(field "$status" client_frame_callbacks)" > CALLBACKS_SPAWNED_DETACHED ))')"
require_numeric_field_ge "$STATUS_REATTACHED" windows 1
LAST_ATTACH_TO_FRAME_US="$(field "$STATUS_REATTACHED" last_attach_to_frame_us)"
[[ "$LAST_ATTACH_TO_FRAME_US" =~ ^[0-9]+$ ]] || {
  echo "ERR missing attach-to-frame latency after detached spawn: $STATUS_REATTACHED" >&2
  exit 1
}
INPUT_FRAME_COUNT_BEFORE="$(numeric_field "$STATUS_REATTACHED" input_frame_count)"
INPUT_EVENTS_BEFORE="$(numeric_field "$STATUS_REATTACHED" input_events)"
POINTER_BUTTONS_BEFORE="$(numeric_field "$STATUS_REATTACHED" pointer_button_events)"
KEYBOARD_EVENTS_BEFORE="$(numeric_field "$STATUS_REATTACHED" keyboard_events)"
TARGET_HITS_BEFORE="$(numeric_field "$STATUS_REATTACHED" pointer_target_hits)"

ctl inject center-motion >/dev/null
ctl inject button 272 pressed >/dev/null
ctl inject button 272 released >/dev/null
ctl inject key 30 pressed >/dev/null
ctl inject key 30 released >/dev/null
STATUS_REATTACHED_INPUT="$(wait_status '[[ "$(field "$status" presentation)" == "Attached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" input_events)" =~ ^[0-9]+$ ]] && (( "$(field "$status" input_events)" >= INPUT_EVENTS_BEFORE + 5 )) && [[ "$(field "$status" pointer_button_events)" =~ ^[0-9]+$ ]] && (( "$(field "$status" pointer_button_events)" >= POINTER_BUTTONS_BEFORE + 2 )) && [[ "$(field "$status" keyboard_events)" =~ ^[0-9]+$ ]] && (( "$(field "$status" keyboard_events)" >= KEYBOARD_EVENTS_BEFORE + 2 )) && [[ "$(field "$status" pointer_target_hits)" =~ ^[0-9]+$ ]] && (( "$(field "$status" pointer_target_hits)" > TARGET_HITS_BEFORE )) && [[ "$(field "$status" input_frame_count)" =~ ^[0-9]+$ ]] && (( "$(field "$status" input_frame_count)" > INPUT_FRAME_COUNT_BEFORE )) && [[ "$(field "$status" last_input_to_frame_us)" =~ ^[0-9]+$ ]]')"
require_numeric_field_ge "$STATUS_REATTACHED_INPUT" pointer_target_hits "$((TARGET_HITS_BEFORE + 1))"
LAST_INPUT_TO_FRAME_US="$(field "$STATUS_REATTACHED_INPUT" last_input_to_frame_us)"
MAX_INPUT_TO_FRAME_US="$(numeric_field "$STATUS_REATTACHED_INPUT" max_input_to_frame_us)"
(( MAX_INPUT_TO_FRAME_US >= LAST_INPUT_TO_FRAME_US )) || {
  echo "ERR max input-to-frame latency does not cover last latency: $STATUS_REATTACHED_INPUT" >&2
  exit 1
}

CAPTURE_REATTACHED="$(capture_window "$PNG_REATTACHED")"
CLIENT_PID="$(cat "$CLIENT_PIDFILE")"

echo "OK WSLg detached spawn/resume smoke passed"
echo "client=$CLIENT"
echo "client_pid=$CLIENT_PID"
echo "status_attached_empty=$STATUS_ATTACHED_EMPTY"
echo "status_detached_empty=$STATUS_DETACHED_EMPTY"
echo "status_spawned_detached=$STATUS_SPAWNED_DETACHED"
echo "status_reattached=$STATUS_REATTACHED"
echo "status_reattached_input=$STATUS_REATTACHED_INPUT"
echo "last_attach_to_frame_us=$LAST_ATTACH_TO_FRAME_US"
echo "last_input_to_frame_us=$LAST_INPUT_TO_FRAME_US"
echo "max_input_to_frame_us=$MAX_INPUT_TO_FRAME_US"
echo "client_frame_callbacks_while_detached=$CALLBACKS_SPAWNED_DETACHED"
echo "capture_reattached=$CAPTURE_REATTACHED"
echo "png_reattached=$PNG_REATTACHED"
echo "log=$LOG"
echo "wayland_trace=$TRACE"

ctl quit >/dev/null || true
wait "$PID" || true
trap - EXIT
