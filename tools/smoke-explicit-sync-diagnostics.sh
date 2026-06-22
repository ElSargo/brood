#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

require_host_presentation_environment

WAYLAND_INFO="${MANIFOLD_WAYLAND_INFO:-}"
if [[ -z "$WAYLAND_INFO" ]]; then
  if command -v wayland-info >/dev/null 2>&1; then
    WAYLAND_INFO="$(command -v wayland-info)"
  else
    echo "ERR no wayland-info found; install wayland-utils or set MANIFOLD_WAYLAND_INFO" >&2
    exit 2
  fi
fi

cargo build -p manifoldd --offline >/dev/null

exec 9>/tmp/manifold-host-smoke.lock
flock 9

TMPDIR="$(mktemp -d /tmp/manifold-explicit-sync-smoke.XXXXXX)"
SOCK="$TMPDIR/control.sock"
LOG="$TMPDIR/manifoldd.log"
INFO="$TMPDIR/wayland-info.txt"

cleanup() {
  if [[ -n "${PID:-}" ]] && kill -0 "$PID" 2>/dev/null; then
    target/debug/manifoldd ctl --control-socket "$SOCK" quit >/dev/null 2>&1 || true
    sleep 0.5
    kill "$PID" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

RUST_LOG="${RUST_LOG:-manifoldd=info,smithay=warn}" \
  target/debug/manifoldd run --control-socket "$SOCK" >"$LOG" 2>&1 &
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

require_field() {
  local status="$1"
  local key="$2"
  local value
  value="$(field "$status" "$key")"
  [[ -n "$value" ]] || {
    echo "ERR missing $key in status: $status" >&2
    exit 1
  }
  printf '%s' "$value"
}

for _ in $(seq 1 80); do
  STATUS="$(ctl status)"
  if [[ "$(field "$STATUS" presentation)" == "Attached" ]] &&
    [[ "$(field "$STATUS" frames)" =~ ^[0-9]+$ ]] &&
    (( "$(field "$STATUS" frames)" >= 1 )); then
    break
  fi
  sleep 0.1
done

STATUS="$(ctl status)"
WAYLAND_DISPLAY_NAME="$(require_field "$STATUS" wayland_display)"
DMABUF="$(require_field "$STATUS" dmabuf)"
DMABUF_FORMATS="$(require_field "$STATUS" dmabuf_formats)"
DMABUF_DISABLED="$(require_field "$STATUS" dmabuf_disabled)"
EXPLICIT_SYNC="$(require_field "$STATUS" explicit_sync)"
EXPLICIT_SYNC_DISABLED="$(require_field "$STATUS" explicit_sync_disabled)"

WAYLAND_DISPLAY="$WAYLAND_DISPLAY_NAME" "$WAYLAND_INFO" >"$INFO" 2>&1 || {
  echo "ERR wayland-info failed against nested compositor" >&2
  echo "status=$STATUS" >&2
  cat "$INFO" >&2 || true
  exit 1
}

DMABUF_GLOBAL_COUNT="$(grep -c "zwp_linux_dmabuf_v1" "$INFO" || true)"
SYNCOBJ_GLOBAL_COUNT="$(grep -c "wp_linux_drm_syncobj_manager_v1" "$INFO" || true)"

if [[ "$DMABUF_FORMATS" == "0" ]]; then
  [[ "$DMABUF" == "none" ]] || {
    echo "ERR zero dmabuf formats must not advertise dmabuf: $STATUS" >&2
    exit 1
  }
  [[ "$DMABUF_DISABLED" != "none" ]] || {
    echo "ERR disabled dmabuf path should report a reason: $STATUS" >&2
    exit 1
  }
  [[ "$DMABUF_GLOBAL_COUNT" == "0" ]] || {
    echo "ERR wayland-info saw zwp_linux_dmabuf_v1 despite dmabuf_formats=0" >&2
    echo "status=$STATUS" >&2
    cat "$INFO" >&2
    exit 1
  }
else
  [[ "$DMABUF" == "v3" || "$DMABUF" == "v4" ]] || {
    echo "ERR positive dmabuf formats should advertise v3/v4: $STATUS" >&2
    exit 1
  }
  [[ "$DMABUF_GLOBAL_COUNT" != "0" ]] || {
    echo "ERR wayland-info did not see zwp_linux_dmabuf_v1 despite dmabuf=$DMABUF" >&2
    echo "status=$STATUS" >&2
    cat "$INFO" >&2
    exit 1
  }
fi

case "$EXPLICIT_SYNC" in
  none)
    [[ "$EXPLICIT_SYNC_DISABLED" != "none" ]] || {
      echo "ERR disabled explicit sync path should report a reason: $STATUS" >&2
      exit 1
    }
    if [[ "$DMABUF" == "v4" ]]; then
      case "$EXPLICIT_SYNC_DISABLED" in
        drm_render_node_path_unavailable|drm_render_node_open_failed|drm_syncobj_eventfd_unsupported) ;;
        *)
          echo "ERR dmabuf v4 with disabled explicit sync should report a DRM/syncobj reason: $STATUS" >&2
          exit 1
          ;;
      esac
    fi
    [[ "$SYNCOBJ_GLOBAL_COUNT" == "0" ]] || {
      echo "ERR wayland-info saw wp_linux_drm_syncobj_manager_v1 despite explicit_sync=none" >&2
      echo "status=$STATUS" >&2
      cat "$INFO" >&2
      exit 1
    }
    ;;
  v1)
    [[ "$DMABUF" == "v4" ]] || {
      echo "ERR explicit sync requires dmabuf v4: $STATUS" >&2
      exit 1
    }
    [[ "$EXPLICIT_SYNC_DISABLED" == "none" ]] || {
      echo "ERR advertised explicit sync should not report a disabled reason: $STATUS" >&2
      exit 1
    }
    [[ "$SYNCOBJ_GLOBAL_COUNT" != "0" ]] || {
      echo "ERR wayland-info did not see wp_linux_drm_syncobj_manager_v1 despite explicit_sync=v1" >&2
      echo "status=$STATUS" >&2
      cat "$INFO" >&2
      exit 1
    }
    ;;
  *)
    echo "ERR unexpected explicit_sync value: $STATUS" >&2
    exit 1
    ;;
esac

echo "OK explicit sync diagnostics smoke passed"
echo "status=$STATUS"
echo "dmabuf_global_count=$DMABUF_GLOBAL_COUNT"
echo "syncobj_global_count=$SYNCOBJ_GLOBAL_COUNT"
echo "wayland_info=$INFO"
echo "log=$LOG"

ctl quit >/dev/null || true
wait "$PID" || true
trap - EXIT
