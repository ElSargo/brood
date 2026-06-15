#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

if ! command -v weston-simple-shm >/dev/null 2>&1; then
  echo "ERR weston-simple-shm is required for headless status parity smoke" >&2
  exit 2
fi

cargo build -p manifoldd --offline >/dev/null

TMPDIR="$(mktemp -d /tmp/manifold-headless-status-parity.XXXXXX)"
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

STATUS_TEXT="$(ctl status)"
STATUS_JSON="$(ctl status-json)"

python3 - "$STATUS_TEXT" "$STATUS_JSON" <<'PY'
import json
import sys

status_text = sys.argv[1]
status_json = sys.argv[2]
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

data = json.loads(status_json)
json_fields = set(data)
text_keys = set(text_fields)

missing_from_text = sorted(json_fields - text_keys)
missing_from_json = sorted(text_keys - json_fields)
if missing_from_text or missing_from_json:
    raise SystemExit(
        "status/status-json schema mismatch: "
        f"missing_from_text={missing_from_text} missing_from_json={missing_from_json} "
        f"text={status_text!r} json={status_json!r}"
    )

for key in ("wayland_display", "presentation", "presentation_last_error", "dmabuf", "dmabuf_disabled", "dmabuf_last_import", "dmabuf_last_error", "dmabuf_last_blocker_error", "render_node", "explicit_sync", "explicit_sync_disabled"):
    if str(data[key]) != text_fields[key]:
        raise SystemExit(f"stable string field {key} differs: text={text_fields[key]!r} json={data[key]!r}")

optional_latency_keys = {
    "last_commit_to_frame_us",
    "last_commit_to_submit_us",
    "last_client_frame_callback_to_commit_us",
    "last_input_to_frame_us",
    "last_input_to_submit_us",
    "last_input_to_commit_us",
    "last_input_commit_to_submit_us",
    "last_attach_latency_us",
    "last_attach_to_frame_us",
    "last_detach_latency_us",
}
for key in optional_latency_keys:
    text_value = text_fields[key]
    json_value = data[key]
    if json_value is None:
        if text_value != "none":
            raise SystemExit(f"{key} should be text none when JSON null: {text_value!r}")
    elif str(json_value) != text_value:
        raise SystemExit(f"{key} differs: text={text_value!r} json={json_value!r}")

for key, value in data.items():
    if key in optional_latency_keys or isinstance(value, str):
        continue
    if isinstance(value, bool):
        expected = "true" if value else "false"
        if text_fields[key] != expected:
            raise SystemExit(f"boolean field {key} differs: text={text_fields[key]!r} json={value!r}")
        continue
    if not isinstance(value, int):
        raise SystemExit(f"unexpected non-int/non-string JSON field {key}: {value!r}")
    if str(value) != text_fields[key]:
        raise SystemExit(f"numeric field {key} differs: text={text_fields[key]!r} json={value!r}")
PY

echo "OK headless status parity smoke passed"
echo "status_text=$STATUS_TEXT"
echo "status_json=$STATUS_JSON"
echo "client_log=$CLIENT_LOG"
echo "log=$LOG"

ctl quit >/dev/null || true
wait "$PID" || true
trap - EXIT
