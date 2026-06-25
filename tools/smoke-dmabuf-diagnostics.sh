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

exec 9>/tmp/manifold-host-smoke.lock
flock 9

TMPDIR="$(mktemp -d /tmp/manifold-dmabuf-smoke.XXXXXX)"
DBUS_NAME="org.manifold.Test.p$$"
LOG="$TMPDIR/manifoldd.log"
TRACE="$TMPDIR/client-wayland.log"
WRAPPER="$TMPDIR/client-wrapper.sh"

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

for _ in $(seq 1 80); do
  STATUS="$(ctl status)"
  if [[ "$(field "$STATUS" windows)" =~ ^[0-9]+$ ]] &&
    (( "$(field "$STATUS" windows)" >= 1 )) &&
    [[ "$(field "$STATUS" frames)" =~ ^[0-9]+$ ]] &&
    (( "$(field "$STATUS" frames)" >= 2 )); then
    break
  fi
  sleep 0.1
done

STATUS="$(ctl status)"
require_field_ge "$STATUS" windows 1
require_field_ge "$STATUS" frames 2
require_field_ge "$STATUS" shm_formats 2

DMABUF="$(field "$STATUS" dmabuf)"
DMABUF_FORMATS="$(field "$STATUS" dmabuf_formats)"
DMABUF_DISABLED="$(field "$STATUS" dmabuf_disabled)"
EGL_WL_BIND="$(field "$STATUS" egl_wl_bind)"
DMABUF_TRACE_COUNT=0
if [[ -s "$TRACE" ]]; then
  DMABUF_TRACE_COUNT="$(grep -c "zwp_linux_dmabuf" "$TRACE" || true)"
fi

if [[ "$DMABUF_FORMATS" == "0" ]]; then
  [[ "$DMABUF" == "none" ]] || {
    echo "ERR zero dmabuf formats must not advertise dmabuf: $STATUS" >&2
    exit 1
  }
  [[ "$DMABUF_DISABLED" != "none" ]] || {
    echo "ERR disabled dmabuf path should report a reason: $STATUS" >&2
    exit 1
  }
  [[ "$DMABUF_TRACE_COUNT" == "0" ]] || {
    echo "ERR client saw zwp_linux_dmabuf even though dmabuf_formats=0" >&2
    echo "status=$STATUS" >&2
    grep "zwp_linux_dmabuf" "$TRACE" >&2 || true
    exit 1
  }
else
  [[ "$DMABUF" == "v3" || "$DMABUF" == "v4" ]] || {
    echo "ERR positive dmabuf formats should advertise v3/v4: $STATUS" >&2
    exit 1
  }
  [[ "$DMABUF_DISABLED" == "none" || "$DMABUF_DISABLED" == v3_* ]] || {
    echo "ERR unexpected dmabuf diagnostic reason: $STATUS" >&2
    exit 1
  }
  [[ "$DMABUF_TRACE_COUNT" != "0" ]] || {
    echo "ERR client did not see zwp_linux_dmabuf despite advertised dmabuf formats" >&2
    echo "status=$STATUS" >&2
    cat "$TRACE" >&2 || true
    exit 1
  }
fi

echo "OK dmabuf diagnostics smoke passed"
echo "client=$CLIENT"
echo "status=$STATUS"
echo "egl_wl_bind=$EGL_WL_BIND"
echo "dmabuf_trace_count=$DMABUF_TRACE_COUNT"
echo "log=$LOG"
echo "wayland_trace=$TRACE"

ctl quit >/dev/null || true
wait "$PID" || true
trap - EXIT
