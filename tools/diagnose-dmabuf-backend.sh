#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

require_host_presentation_environment

cargo build -p broodd --offline > /dev/null

TMPDIR="$(mktemp -d /tmp/brood-dmabuf-backend.XXXXXX)"
DBUS_NAME="org.brood.Test.p$$"
LOG="$TMPDIR/broodd.log"

cleanup() {
  if [[ -n "${PID:-}" ]] && kill -0 "$PID" 2> /dev/null; then
    target/debug/brood ctl --dbus-name "$DBUS_NAME" quit > /dev/null 2>&1 || true
  fi
  terminate_process_bounded "${PID:-}" 20 0.025
}
trap cleanup EXIT

RUST_LOG="${RUST_LOG:-broodd=info,smithay=warn}" \
  target/debug/brood run --dbus-name "$DBUS_NAME" > "$LOG" 2>&1 &
PID=$!

wait_for_broodd_control "$DBUS_NAME" "$PID" "$LOG"

ctl() {
  target/debug/brood ctl --dbus-name "$DBUS_NAME" "$@"
}

field() {
  local status="$1"
  local key="$2"
  sed -n "s/.* ${key}=\\([^ ]*\\).*/\\1/p" <<< "$status"
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
    (("$(field "$STATUS" frames)" >= 1)); then
    break
  fi
  sleep 0.1
done

STATUS="$(ctl status)"
DMABUF="$(require_field "$STATUS" dmabuf)"
DMABUF_FORMATS="$(require_field "$STATUS" dmabuf_formats)"
DMABUF_DISABLED="$(require_field "$STATUS" dmabuf_disabled)"
EXPLICIT_SYNC="$(require_field "$STATUS" explicit_sync)"
EXPLICIT_SYNC_DISABLED="$(require_field "$STATUS" explicit_sync_disabled)"
EGL_WL_BIND="$(require_field "$STATUS" egl_wl_bind)"
RENDER_NODE="$(require_field "$STATUS" render_node)"
SHM_FORMATS="$(require_field "$STATUS" shm_formats)"

DRI_RENDER_NODES="none"
if compgen -G "/dev/dri/renderD*" > /dev/null; then
  DRI_RENDER_NODES="$(printf '%s ' /dev/dri/renderD*)"
  DRI_RENDER_NODES="${DRI_RENDER_NODES% }"
fi

CLASSIFIER_OUTPUT="$(
  python3 "$ROOT/tools/classify-dmabuf-backend.py" \
    --dmabuf "$DMABUF" \
    --dmabuf-formats "$DMABUF_FORMATS" \
    --explicit-sync "$EXPLICIT_SYNC" \
    --explicit-sync-disabled "$EXPLICIT_SYNC_DISABLED"
)"
CLASSIFICATION="$(sed -n 's/^classification=//p' <<< "$CLASSIFIER_OUTPUT")"
SUMMARY="$(sed -n 's/^summary=//p' <<< "$CLASSIFIER_OUTPUT")"
NEXT_STEP="$(sed -n 's/^next=//p' <<< "$CLASSIFIER_OUTPUT")"

echo "OK dmabuf backend diagnostic complete"
echo "classification=$CLASSIFICATION"
echo "summary=$SUMMARY"
echo "dmabuf=$DMABUF"
echo "dmabuf_formats=$DMABUF_FORMATS"
echo "dmabuf_disabled=$DMABUF_DISABLED"
echo "explicit_sync=$EXPLICIT_SYNC"
echo "explicit_sync_disabled=$EXPLICIT_SYNC_DISABLED"
echo "egl_wl_bind=$EGL_WL_BIND"
echo "render_node=$RENDER_NODE"
echo "shm_formats=$SHM_FORMATS"
echo "dev_dri_render_nodes=$DRI_RENDER_NODES"
echo "wayland_display=${WAYLAND_DISPLAY:-unset}"
echo "xdg_runtime_dir=${XDG_RUNTIME_DIR:-unset}"
echo "display=${DISPLAY:-unset}"
echo "wslg=${WSL2_GUI_APPS_ENABLED:-unset}"
echo "mesa_d3d12_adapter=${MESA_D3D12_DEFAULT_ADAPTER_NAME:-unset}"
echo "status=$STATUS"
echo "log=$LOG"

echo "next=$NEXT_STEP"

ctl quit > /dev/null || true
wait "$PID" || true
trap - EXIT
