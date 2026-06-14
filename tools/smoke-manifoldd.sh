#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

require_host_presentation_environment

CLIENT="${MANIFOLD_SMOKE_CLIENT:-}"
if [[ -z "$CLIENT" ]]; then
  if command -v weston-simple-egl >/dev/null 2>&1; then
    CLIENT="$(command -v weston-simple-egl)"
  elif command -v weston-terminal >/dev/null 2>&1; then
    CLIENT="$(command -v weston-terminal)"
  else
    echo "ERR no smoke client found; install weston-simple-egl or set MANIFOLD_SMOKE_CLIENT" >&2
    exit 2
  fi
fi

cargo build -p manifoldd --offline >/dev/null

exec 9>/tmp/manifold-wslg-smoke.lock
flock 9

TMPDIR="$(mktemp -d /tmp/manifold-smoke.XXXXXX)"
SOCK="$TMPDIR/control.sock"
LOG="$TMPDIR/manifoldd.log"
TRACE="$TMPDIR/client-wayland.log"
WRAPPER="$TMPDIR/client-wrapper.sh"

cleanup() {
  if [[ -n "${PID:-}" ]] && kill -0 "$PID" 2>/dev/null; then
    target/debug/manifoldd ctl --control-socket "$SOCK" quit >/dev/null 2>&1 || true
    sleep 0.5
    kill "$PID" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

printf '#!/usr/bin/env bash\nexec env WAYLAND_DEBUG=1 %q 2>%q\n' "$CLIENT" "$TRACE" >"$WRAPPER"
chmod +x "$WRAPPER"

RUST_LOG="${RUST_LOG:-manifoldd=info,smithay=warn}" \
  target/debug/manifoldd run --control-socket "$SOCK" -c "$WRAPPER" >"$LOG" 2>&1 &
PID=$!

wait_for_manifoldd_control "$SOCK" "$PID" "$LOG"

ctl() {
  target/debug/manifoldd ctl --control-socket "$SOCK" "$@"
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
  [[ -n "$value" ]] || {
    echo "ERR missing $key in status: $status" >&2
    exit 1
  }
  (( value >= min )) || {
    echo "ERR expected $key >= $min, got $value in status: $status" >&2
    exit 1
  }
}

wait_for_window_and_frames() {
  local status
  for _ in $(seq 1 80); do
    status="$(ctl status)"
    if [[ "$(field "$status" presentation)" == "Attached" ]] &&
      [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] &&
      [[ "$(field "$status" frames)" =~ ^[0-9]+$ ]] &&
      (( "$(field "$status" windows)" >= 1 )) &&
      (( "$(field "$status" frames)" >= 2 )); then
      echo "$status"
      return 0
    fi
    sleep 0.1
  done
  echo "ERR hosted client did not map and render frames" >&2
  ctl status >&2 || true
  tail -120 "$LOG" >&2 || true
  exit 1
}

STATUS1="$(wait_for_window_and_frames)"
FRAMES1="$(field "$STATUS1" frames)"
COMMITS1="$(field "$STATUS1" commits)"
require_field_ge "$STATUS1" windows 1
require_field_ge "$STATUS1" frames 2
require_field_ge "$STATUS1" commits 1
require_field_ge "$STATUS1" attach_count 1

ctl detach >/dev/null
sleep 0.4
STATUS2="$(ctl status)"
[[ "$(field "$STATUS2" presentation)" == "Detached" ]] || {
  echo "ERR expected Detached after detach: $STATUS2" >&2
  exit 1
}
require_field_ge "$STATUS2" windows 1
require_field_ge "$STATUS2" detach_count 1
[[ "$(field "$STATUS2" pending_input_events)" == "0" ]] || {
  echo "ERR detached frontend should not keep pending visible input latency: $STATUS2" >&2
  exit 1
}
[[ "$(field "$STATUS2" pending_commits)" == "0" ]] || {
  echo "ERR detached frontend should not keep pending visible commit latency: $STATUS2" >&2
  exit 1
}

ctl attach >/dev/null
sleep 0.8
STATUS3="$(ctl status)"
[[ "$(field "$STATUS3" presentation)" == "Attached" ]] || {
  echo "ERR expected Attached after reattach: $STATUS3" >&2
  exit 1
}
require_field_ge "$STATUS3" windows 1
require_field_ge "$STATUS3" attach_count 2
require_field_ge "$STATUS3" frames "$((FRAMES1 + 1))"
require_field_ge "$STATUS3" commits "$COMMITS1"
[[ "$(field "$STATUS3" pending_input_events)" == "0" ]] || {
  echo "ERR reattached frontend should not inherit stale pending visible input latency: $STATUS3" >&2
  exit 1
}

DMABUF="$(field "$STATUS3" dmabuf)"
DMABUF_FORMATS="$(field "$STATUS3" dmabuf_formats)"
if [[ "$DMABUF_FORMATS" == "0" ]]; then
  [[ "$DMABUF" == "none" ]] || {
    echo "ERR zero dmabuf formats must not advertise dmabuf: $STATUS3" >&2
    exit 1
  }
else
  [[ "$DMABUF" == "v3" || "$DMABUF" == "v4" ]] || {
    echo "ERR positive dmabuf formats should advertise v3/v4: $STATUS3" >&2
    exit 1
  }
fi

echo "OK smoke passed"
echo "client=$CLIENT"
echo "status_initial=$STATUS1"
echo "status_detached=$STATUS2"
echo "status_reattached=$STATUS3"
echo "log=$LOG"
echo "wayland_trace=$TRACE"

ctl quit >/dev/null || true
wait "$PID" || true
trap - EXIT
