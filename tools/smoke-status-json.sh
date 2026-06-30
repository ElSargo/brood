#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

require_host_presentation_environment

CLIENT="${MANIFOLD_SMOKE_CLIENT:-}"
if [[ -z "$CLIENT" ]]; then
  if command -v weston-simple-egl > /dev/null 2>&1; then
    CLIENT="$(command -v weston-simple-egl)"
  elif command -v weston-terminal > /dev/null 2>&1; then
    CLIENT="$(command -v weston-terminal)"
  else
    echo "ERR no smoke client found; install weston-simple-egl or set MANIFOLD_SMOKE_CLIENT" >&2
    exit 2
  fi
fi

cargo build -p manifoldd --offline > /dev/null

exec 9> /tmp/manifold-host-smoke.lock
flock 9

TMPDIR="$(mktemp -d /tmp/manifold-status-json-smoke.XXXXXX)"
DBUS_NAME="org.manifold.Test.p$$"
LOG="$TMPDIR/manifoldd.log"
WRAPPER="$TMPDIR/client-wrapper.sh"

cleanup() {
  if [[ -n "${PID:-}" ]] && kill -0 "$PID" 2> /dev/null; then
    target/debug/manifold ctl --dbus-name "$DBUS_NAME" quit > /dev/null 2>&1 || true
    sleep 0.5
    kill "$PID" > /dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

printf '#!/usr/bin/env bash\nexec %q\n' "$CLIENT" > "$WRAPPER"
chmod +x "$WRAPPER"

RUST_LOG="${RUST_LOG:-manifoldd=info,smithay=warn}" \
  target/debug/manifold run --dbus-name "$DBUS_NAME" -c "$WRAPPER" > "$LOG" 2>&1 &
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

wait_status_text() {
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
  echo "ERR timed out waiting for text status predicate: $predicate" >&2
  ctl status >&2 || true
  tail -120 "$LOG" >&2 || true
  exit 1
}

STATUS_JSON=""
for _ in $(seq 1 80); do
  STATUS_JSON="$(ctl status-json)"
  if python3 - "$STATUS_JSON" << 'PY'; then
import json
import sys

data = json.loads(sys.argv[1])
if data.get("presentation") == "Attached" and data.get("windows", 0) >= 1 and data.get("frames", 0) >= 2:
    sys.exit(0)
sys.exit(1)
PY
    break
  fi
  sleep 0.1
done

STATUS_TEXT_INITIAL="$(wait_status_text '[[ "$(field "$status" presentation)" == "Attached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" frames)" =~ ^[0-9]+$ ]] && (( "$(field "$status" frames)" >= 2 ))')"
INPUT_FRAME_COUNT_INITIAL="$(field "$STATUS_TEXT_INITIAL" input_frame_count)"
[[ "$INPUT_FRAME_COUNT_INITIAL" =~ ^[0-9]+$ ]] || {
  echo "ERR missing initial input_frame_count in text status: $STATUS_TEXT_INITIAL" >&2
  exit 1
}

ctl inject center-motion > /dev/null
ctl inject button 272 pressed > /dev/null
ctl inject button 272 released > /dev/null

STATUS_TEXT="$(wait_status_text '[[ "$(field "$status" presentation)" == "Attached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" input_frame_count)" =~ ^[0-9]+$ ]] && (( "$(field "$status" input_frame_count)" > INPUT_FRAME_COUNT_INITIAL )) && [[ "$(field "$status" last_input_to_frame_us)" =~ ^[0-9]+$ ]]')"
STATUS_JSON="$(ctl status-json)"
python3 - "$STATUS_TEXT" "$STATUS_JSON" << 'PY'
import json
import sys

status_text = sys.argv[1]
data = json.loads(sys.argv[2])
if not status_text.startswith("OK "):
    raise SystemExit(f"status text should start with OK: {status_text!r}")
text_fields = {}
for token in status_text.split()[1:]:
    if "=" not in token:
        raise SystemExit(f"unexpected status token without '=': {token!r} in {status_text!r}")
    key, value = token.split("=", 1)
    if not key:
        raise SystemExit(f"empty status key in token {token!r}")
    if key in text_fields:
        raise SystemExit(f"duplicate status key {key!r}: {status_text!r}")
    text_fields[key] = value

json_fields = set(data)
text_keys = set(text_fields)
missing_from_text = sorted(json_fields - text_keys)
missing_from_json = sorted(text_keys - json_fields)
if missing_from_text or missing_from_json:
    raise SystemExit(
        "attached status/status-json schema mismatch: "
        f"missing_from_text={missing_from_text} missing_from_json={missing_from_json} "
        f"text={status_text!r} json={data!r}"
    )

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
    "max_commit_to_submit_us": int,
    "client_frame_callback_commit_count": int,
    "last_client_frame_callback_batch_callbacks": int,
    "max_client_frame_callback_batch_callbacks": int,
    "max_client_frame_callback_to_commit_us": int,
    "client_frame_deferrals": int,
    "client_deferred_frames": int,
    "last_frame_us": int,
    "max_frame_us": int,
    "last_frame_interval_us": int,
    "max_frame_interval_us": int,
    "input_frame_deferrals": int,
    "input_deferred_frames": int,
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
    "max_input_to_submit_us": int,
    "input_commit_frame_count": int,
    "input_without_commit_frame_count": int,
    "max_input_to_commit_us": int,
    "max_input_commit_to_submit_us": int,
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
    if expected_type is int and not text_fields[key].isdigit():
        raise SystemExit(f"text status field {key} should be numeric: {text_fields[key]!r}")

stable_string_fields = {
    "wayland_display",
    "presentation",
    "presentation_last_error",
    "egl_wl_bind",
    "dmabuf",
    "dmabuf_disabled",
    "dmabuf_last_import",
    "dmabuf_last_error",
    "dmabuf_last_blocker_error",
    "render_node",
    "explicit_sync",
    "explicit_sync_disabled",
}
for key in stable_string_fields:
    if str(data[key]) != text_fields[key]:
        raise SystemExit(f"{key} differs between status surfaces: text={text_fields[key]!r} json={data[key]!r}")

if "last_input_to_frame_us" not in data:
    raise SystemExit(f"missing last_input_to_frame_us: {data}")
if data["last_input_to_frame_us"] is not None and not isinstance(data["last_input_to_frame_us"], int):
    raise SystemExit(f"last_input_to_frame_us has wrong type: {data}")
if "last_commit_to_frame_us" not in data:
    raise SystemExit(f"missing last_commit_to_frame_us: {data}")
if data["last_commit_to_frame_us"] is not None and not isinstance(data["last_commit_to_frame_us"], int):
    raise SystemExit(f"last_commit_to_frame_us has wrong type: {data}")
for key in (
    "last_commit_to_submit_us",
    "last_client_frame_callback_to_commit_us",
    "last_input_to_submit_us",
    "last_input_to_commit_us",
    "last_input_commit_to_submit_us",
    "last_attach_latency_us",
    "last_attach_to_frame_us",
    "last_detach_latency_us",
):
    if key not in data:
        raise SystemExit(f"missing {key}: {data}")
    if data[key] is not None and not isinstance(data[key], int):
        raise SystemExit(f"{key} has wrong type: {data}")

optional_latency_keys = {
    "last_input_to_frame_us",
    "last_commit_to_frame_us",
    "last_input_to_submit_us",
    "last_commit_to_submit_us",
    "last_input_to_commit_us",
    "last_input_commit_to_submit_us",
    "last_client_frame_callback_to_commit_us",
    "last_attach_latency_us",
    "last_attach_to_frame_us",
    "last_detach_latency_us",
}
for key in optional_latency_keys:
    text_value = text_fields[key]
    json_value = data[key]
    if text_value != "none" and not text_value.isdigit():
        raise SystemExit(f"text status field {key} should be none or numeric: {text_value!r}")
    if json_value is None:
        continue
    if not isinstance(json_value, int):
        raise SystemExit(f"{key} has wrong JSON type: {json_value!r}")

if data["presentation"] != "Attached":
    raise SystemExit(f"expected Attached presentation: {data}")
if data["wayland_client_insert_errors"] != 0 or data["wayland_dispatch_errors"] != 0 or data["xdg_configure_errors"] != 0:
    raise SystemExit(f"healthy attached smoke should not report Wayland runtime errors: {data}")
if data["presentation_attach_failures"] != 0:
    raise SystemExit(f"attached smoke should not report presentation attach failures: {data}")
if data["presentation_runtime_failures"] != 0:
    raise SystemExit(f"attached smoke should not report presentation runtime failures: {data}")
if data["presentation_last_error"] != "none":
    raise SystemExit(f"attached smoke should not report a presentation error: {data}")
if data["windows"] < 1:
    raise SystemExit(f"expected at least one nested window: {data}")
if data["frames"] < 2:
    raise SystemExit(f"expected rendered frames: {data}")
if data["client_frame_callbacks"] < 1:
    raise SystemExit(f"expected client frame callbacks: {data}")
if data["commits"] < 1:
    raise SystemExit(f"expected client commits: {data}")
if data["commit_frame_count"] < 1:
    raise SystemExit(f"expected at least one visible commit-to-frame sample: {data}")
if data["last_commit_batch_commits"] < 1 or data["max_commit_batch_commits"] < data["last_commit_batch_commits"]:
    raise SystemExit(f"expected visible commit batching diagnostics: {data}")
if data["last_commit_to_frame_us"] is None:
    raise SystemExit(f"expected visible commit-to-frame latency: {data}")
if data["max_commit_to_frame_us"] < data["last_commit_to_frame_us"]:
    raise SystemExit(f"expected max commit-to-frame latency to cover last latency: {data}")
if data["last_commit_to_submit_us"] is None:
    raise SystemExit(f"expected visible commit-to-submit latency: {data}")
if data["max_commit_to_submit_us"] < data["last_commit_to_submit_us"]:
    raise SystemExit(f"expected max commit-to-submit latency to cover last latency: {data}")
if data["input_events"] < 3 or data["pointer_motion_events"] < 1 or data["pointer_button_events"] < 2:
    raise SystemExit(f"expected attached input counters after injected input: {data}")
if data["pointer_target_hits"] < 1:
    raise SystemExit(f"expected targeted pointer input after injected input: {data}")
if data["input_frame_count"] < 1:
    raise SystemExit(f"expected at least one visible input-to-frame sample: {data}")
if data["last_input_to_frame_us"] is None:
    raise SystemExit(f"expected visible input-to-frame latency: {data}")
if data["max_input_to_frame_us"] < data["last_input_to_frame_us"]:
    raise SystemExit(f"expected max input-to-frame latency to cover last latency: {data}")
if data["last_input_to_submit_us"] is None:
    raise SystemExit(f"expected visible input-to-submit latency: {data}")
if data["max_input_to_submit_us"] < data["last_input_to_submit_us"]:
    raise SystemExit(f"expected max input-to-submit latency to cover last latency: {data}")
if data["attach_count"] < 1:
    raise SystemExit(f"expected attach_count >= 1: {data}")
if data["last_attach_latency_us"] is None:
    raise SystemExit(f"expected startup attach latency: {data}")
if data["last_attach_to_frame_us"] is None:
    raise SystemExit(f"expected startup attach-to-frame latency: {data}")
if data["shm_formats"] < 2:
    raise SystemExit(f"expected shm formats: {data}")
if data["dmabuf_formats"] == 0:
    if data["dmabuf"] != "none":
        raise SystemExit(f"zero dmabuf formats must not advertise dmabuf: {data}")
    if data["dmabuf_disabled"] == "none":
        raise SystemExit(f"disabled dmabuf path should report a reason: {data}")
    if data["dmabuf_attempts"] != 0 or data["dmabuf_accepted"] != 0 or data["dmabuf_rejected"] != 0:
        raise SystemExit(f"disabled dmabuf path should not receive import attempts: {data}")
    if data["dmabuf_last_import"] != "none" or data["dmabuf_last_error"] != "none":
        raise SystemExit(f"disabled dmabuf path should not report last import details: {data}")
    if data["dmabuf_explicit_sync_release_points"] != 0 or data["dmabuf_explicit_sync_blockers"] != 0 or data["dmabuf_implicit_blockers"] != 0:
        raise SystemExit(f"disabled dmabuf path should not install blockers: {data}")
    if data["dmabuf_blocker_failures"] != 0 or data["dmabuf_last_blocker_error"] != "none":
        raise SystemExit(f"disabled dmabuf path should not report blocker errors: {data}")
else:
    if data["dmabuf"] not in {"v3", "v4"}:
        raise SystemExit(f"positive dmabuf formats should advertise v3/v4: {data}")
    if data["dmabuf_blocker_failures"] != 0:
        raise SystemExit(f"dmabuf blocker installation should not fail: {data}")
if data["explicit_sync"] == "none":
    if data["explicit_sync_disabled"] == "none":
        raise SystemExit(f"disabled explicit sync path should report a reason: {data}")
else:
    if data["explicit_sync_disabled"] != "none":
        raise SystemExit(f"advertised explicit sync should not report a disabled reason: {data}")
PY

echo "OK status-json smoke passed"
echo "client=$CLIENT"
echo "status_text=$STATUS_TEXT"
echo "status_json=$STATUS_JSON"
echo "log=$LOG"

ctl quit > /dev/null || true
wait "$PID" || true
trap - EXIT
