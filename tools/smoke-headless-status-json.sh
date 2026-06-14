#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

if ! command -v weston-simple-shm >/dev/null 2>&1; then
  echo "ERR weston-simple-shm is required for headless status-json smoke" >&2
  exit 2
fi

cargo build -p manifoldd --offline >/dev/null

TMPDIR="$(mktemp -d /tmp/manifold-headless-status-json.XXXXXX)"
SOCK="$TMPDIR/control.sock"
LOG="$TMPDIR/manifoldd.log"
CLIENT_LOG="$TMPDIR/client.log"
WRAPPER="$TMPDIR/client-wrapper.sh"

cleanup() {
  if [[ -n "${PID:-}" ]] && kill -0 "$PID" 2>/dev/null; then
    target/debug/manifoldd ctl --control-socket "$SOCK" quit >/dev/null 2>&1 || true
    sleep 0.5
    kill "$PID" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

printf '#!/usr/bin/env bash\nexec weston-simple-shm >%q 2>&1\n' "$CLIENT_LOG" >"$WRAPPER"
chmod +x "$WRAPPER"

RUST_LOG="${RUST_LOG:-manifoldd=info,smithay=warn}" \
  target/debug/manifoldd run --headless --control-socket "$SOCK" -c "$WRAPPER" >"$LOG" 2>&1 &
PID=$!

wait_for_manifoldd_control "$SOCK" "$PID" "$LOG"

ctl() {
  target/debug/manifoldd ctl --control-socket "$SOCK" "$@"
}

for _ in $(seq 1 100); do
  STATUS_JSON="$(ctl status-json)"
  if python3 - "$STATUS_JSON" <<'PY'
import json
import sys

data = json.loads(sys.argv[1])
if data.get("presentation") == "Detached" and data.get("windows", 0) >= 1 and data.get("commits", 0) >= 1:
    sys.exit(0)
sys.exit(1)
PY
  then
    break
  fi
  sleep 0.1
done

ctl inject center-motion >/dev/null

for _ in $(seq 1 100); do
  STATUS_JSON="$(ctl status-json)"
  if python3 - "$STATUS_JSON" <<'PY'
import json
import sys

data = json.loads(sys.argv[1])
if data.get("pointer_motion_events", 0) >= 1 and data.get("pointer_target_hits", 0) >= 1:
    sys.exit(0)
sys.exit(1)
PY
  then
    break
  fi
  sleep 0.1
done

STATUS_JSON="$(ctl status-json)"
python3 - "$STATUS_JSON" <<'PY'
import json
import sys

data = json.loads(sys.argv[1])
required = {
    "wayland_display": str,
    "wayland_client_insert_errors": int,
    "wayland_dispatch_errors": int,
    "xdg_configure_errors": int,
    "presentation": str,
    "presentation_attach_failures": int,
    "presentation_runtime_failures": int,
    "presentation_last_error": str,
    "windows": int,
    "browsers": int,
    "frames": int,
    "client_frame_callbacks": int,
    "commits": int,
    "pending_commits": int,
    "commit_frame_count": int,
    "last_commit_batch_commits": int,
    "max_commit_batch_commits": int,
    "max_commit_to_frame_us": int,
    "last_frame_us": int,
    "max_frame_us": int,
    "last_frame_interval_us": int,
    "max_frame_interval_us": int,
    "input_events": int,
    "keyboard_events": int,
    "pointer_motion_events": int,
    "pointer_button_events": int,
    "pointer_axis_events": int,
    "pointer_target_hits": int,
    "pointer_target_misses": int,
    "focus_clears": int,
    "focus_repairs": int,
    "window_lifecycle_refreshes": int,
    "window_lifecycle_focus_clears": int,
    "window_lifecycle_focus_repairs": int,
    "pending_input_events": int,
    "input_frame_count": int,
    "last_input_batch_events": int,
    "max_input_batch_events": int,
    "max_input_to_frame_us": int,
    "attach_count": int,
    "detach_count": int,
    "max_attach_latency_us": int,
    "max_attach_to_frame_us": int,
    "max_detach_latency_us": int,
    "shm_formats": int,
    "egl_wl_bind": str,
    "dmabuf": str,
    "dmabuf_formats": int,
    "dmabuf_disabled": str,
    "dmabuf_attempts": int,
    "dmabuf_accepted": int,
    "dmabuf_rejected": int,
    "dmabuf_last_import": str,
    "dmabuf_last_error": str,
    "dmabuf_explicit_sync_release_points": int,
    "dmabuf_explicit_sync_blockers": int,
    "dmabuf_implicit_blockers": int,
    "dmabuf_blocker_failures": int,
    "dmabuf_last_blocker_error": str,
    "render_node": str,
    "explicit_sync": str,
    "explicit_sync_disabled": str,
}
for key, expected_type in required.items():
    if key not in data:
        raise SystemExit(f"missing {key}: {data}")
    if not isinstance(data[key], expected_type):
        raise SystemExit(f"{key} has wrong type {type(data[key]).__name__}: {data}")

if "last_input_to_frame_us" not in data:
    raise SystemExit(f"missing last_input_to_frame_us: {data}")
if data["last_input_to_frame_us"] is not None and not isinstance(data["last_input_to_frame_us"], int):
    raise SystemExit(f"last_input_to_frame_us has wrong type: {data}")
if "last_commit_to_frame_us" not in data:
    raise SystemExit(f"missing last_commit_to_frame_us: {data}")
if data["last_commit_to_frame_us"] is not None and not isinstance(data["last_commit_to_frame_us"], int):
    raise SystemExit(f"last_commit_to_frame_us has wrong type: {data}")
for key in ("last_attach_latency_us", "last_attach_to_frame_us", "last_detach_latency_us"):
    if key not in data:
        raise SystemExit(f"missing {key}: {data}")
    if data[key] is not None and not isinstance(data[key], int):
        raise SystemExit(f"{key} has wrong type: {data}")
if data["presentation"] != "Detached":
    raise SystemExit(f"headless compositor should remain detached: {data}")
if data["wayland_client_insert_errors"] != 0 or data["wayland_dispatch_errors"] != 0 or data["xdg_configure_errors"] != 0:
    raise SystemExit(f"healthy headless smoke should not report Wayland runtime errors: {data}")
if data["presentation_attach_failures"] != 0 or data["presentation_runtime_failures"] != 0 or data["presentation_last_error"] != "none":
    raise SystemExit(f"headless status-json smoke should not attach the frontend: {data}")
if data["windows"] < 1 or data["commits"] < 1:
    raise SystemExit(f"expected mapped real nested client with commits: {data}")
if data["frames"] != 0 or data["client_frame_callbacks"] != 0:
    raise SystemExit(f"headless compositor should not render frontend frames: {data}")
if data["pending_commits"] != 0 or data["commit_frame_count"] != 0 or data["last_commit_to_frame_us"] is not None or data["max_commit_to_frame_us"] != 0:
    raise SystemExit(f"headless compositor should not report visible commit-to-frame latency: {data}")
if data["shm_formats"] < 2:
    raise SystemExit(f"expected mandatory shm formats in headless mode: {data}")
if data["pointer_motion_events"] < 1 or data["pointer_target_hits"] < 1:
    raise SystemExit(f"expected injected center-motion to target nested surface: {data}")
if data["pending_input_events"] != 0 or data["input_frame_count"] != 0 or data["last_input_to_frame_us"] is not None or data["max_input_to_frame_us"] != 0:
    raise SystemExit(f"headless compositor should not report visible input-to-frame latency: {data}")
if data["dmabuf"] != "none" or data["dmabuf_formats"] != 0:
    raise SystemExit(f"headless compositor should not advertise presentation dmabuf before attach: {data}")
if data["dmabuf_attempts"] != 0 or data["dmabuf_accepted"] != 0 or data["dmabuf_rejected"] != 0:
    raise SystemExit(f"headless compositor should not receive dmabuf imports without dmabuf global: {data}")
if data["dmabuf_last_import"] != "none" or data["dmabuf_last_error"] != "none":
    raise SystemExit(f"headless compositor should not report dmabuf import details: {data}")
if data["dmabuf_explicit_sync_release_points"] != 0 or data["dmabuf_explicit_sync_blockers"] != 0 or data["dmabuf_implicit_blockers"] != 0:
    raise SystemExit(f"headless compositor should not install dmabuf blockers without dmabuf global: {data}")
if data["dmabuf_blocker_failures"] != 0 or data["dmabuf_last_blocker_error"] != "none":
    raise SystemExit(f"headless compositor should not report dmabuf blocker errors: {data}")
if data["explicit_sync"] == "none":
    if data["explicit_sync_disabled"] == "none":
        raise SystemExit(f"disabled explicit sync path should report a reason: {data}")
else:
    if data["explicit_sync_disabled"] != "none":
        raise SystemExit(f"advertised explicit sync should not report a disabled reason: {data}")
PY

echo "OK headless status-json smoke passed"
echo "status_json=$STATUS_JSON"
echo "client_log=$CLIENT_LOG"
echo "log=$LOG"

ctl quit >/dev/null || true
wait "$PID" || true
trap - EXIT
