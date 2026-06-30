#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

require_host_presentation_environment

REQUIRE_EXPLICIT_SYNC="${MANIFOLD_DMABUF_REQUIRE_EXPLICIT_SYNC:-0}"
case "$REQUIRE_EXPLICIT_SYNC" in
  0 | 1) ;;
  *)
    echo "ERR MANIFOLD_DMABUF_REQUIRE_EXPLICIT_SYNC must be 0 or 1" >&2
    exit 2
    ;;
esac

IMPORT_CLIENT="${MANIFOLD_DMABUF_IMPORT_CLIENT:-}"
IMPORT_ARGS=()
if [[ -z "$IMPORT_CLIENT" ]]; then
  if command -v weston-simple-dmabuf-egl > /dev/null 2>&1; then
    IMPORT_CLIENT="$(command -v weston-simple-dmabuf-egl)"
    if [[ "$REQUIRE_EXPLICIT_SYNC" == "1" ]]; then
      IMPORT_ARGS=(--explicit-sync=1)
    else
      IMPORT_ARGS=(--explicit-sync=0)
    fi
  else
    echo "SKIP no dmabuf import client found; install weston-simple-dmabuf-egl or set MANIFOLD_DMABUF_IMPORT_CLIENT" >&2
    exit 77
  fi
elif [[ -n "${MANIFOLD_DMABUF_IMPORT_ARGS:-}" ]]; then
  # shellcheck disable=SC2206
  IMPORT_ARGS=(${MANIFOLD_DMABUF_IMPORT_ARGS})
fi

FEEDBACK_CLIENT="${MANIFOLD_DMABUF_FEEDBACK_CLIENT:-}"
if [[ -z "$FEEDBACK_CLIENT" ]] && command -v weston-simple-dmabuf-feedback > /dev/null 2>&1; then
  FEEDBACK_CLIENT="$(command -v weston-simple-dmabuf-feedback)"
fi

cargo build -p manifoldd --offline > /dev/null

exec 9> /tmp/manifold-host-smoke.lock
flock 9

TMPDIR="$(mktemp -d /tmp/manifold-dmabuf-positive.XXXXXX)"
DBUS_NAME="org.manifold.Test.p$$"
LOG="$TMPDIR/manifoldd.log"
TRACE="$TMPDIR/client-wayland.log"
IMPORT_LOG="$TMPDIR/import-client.log"
IMPORT_WRAPPER="$TMPDIR/import-client-wrapper.sh"
FEEDBACK_LOG="$TMPDIR/feedback-client.log"
FEEDBACK_TRACE="$TMPDIR/feedback-client-wayland.log"
FEEDBACK_WRAPPER="$TMPDIR/feedback-client-wrapper.sh"

cleanup() {
  if [[ -n "${PID:-}" ]] && kill -0 "$PID" 2> /dev/null; then
    target/debug/manifold ctl --dbus-name "$DBUS_NAME" quit > /dev/null 2>&1 || true
    sleep 0.5
    kill "$PID" > /dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

{
  printf '#!/usr/bin/env bash\nexec env WAYLAND_DEBUG=1'
  printf ' %q' "$IMPORT_CLIENT" "${IMPORT_ARGS[@]}"
  printf ' >%q 2>%q\n' "$IMPORT_LOG" "$TRACE"
} > "$IMPORT_WRAPPER"
chmod +x "$IMPORT_WRAPPER"

RUST_LOG="${RUST_LOG:-manifoldd=info,smithay=warn}" \
  target/debug/manifold run --dbus-name "$DBUS_NAME" -c "$IMPORT_WRAPPER" > "$LOG" 2>&1 &
PID=$!

wait_for_manifoldd_control "$DBUS_NAME" "$PID" "$LOG"

ctl() {
  target/debug/manifold ctl --dbus-name "$DBUS_NAME" "$@"
}

STATUS_JSON=""
for _ in $(seq 1 80); do
  STATUS_JSON="$(ctl status-json)"
  if python3 - "$STATUS_JSON" << 'PY'; then
import json
import sys

data = json.loads(sys.argv[1])
if data.get("dmabuf_formats", 0) == 0:
    sys.exit(0)
if data.get("windows", 0) >= 1 and data.get("frames", 0) >= 2:
    sys.exit(0)
sys.exit(1)
PY
    break
  fi
  sleep 0.1
done

STATUS_JSON="$(ctl status-json)"
set +e
python3 "$ROOT/tools/validate-dmabuf-status.py" \
  --status-json "$STATUS_JSON" \
  --trace "$TRACE" \
  --require-explicit-sync "$REQUIRE_EXPLICIT_SYNC"
rc=$?
set -e
if [[ "$rc" == "77" ]]; then
  echo "status_json=$STATUS_JSON"
  echo "import_client=$IMPORT_CLIENT ${IMPORT_ARGS[*]}"
  echo "require_explicit_sync=$REQUIRE_EXPLICIT_SYNC"
  echo "log=$LOG"
  echo "import_log=$IMPORT_LOG"
  echo "wayland_trace=$TRACE"
  exit 77
fi
if [[ "$rc" != "0" ]]; then
  exit "$rc"
fi

DMABUF_VERSION="$(
  python3 - "$STATUS_JSON" << 'PY'
import json
import sys

print(json.loads(sys.argv[1])["dmabuf"])
PY
)"

if [[ "$DMABUF_VERSION" == "v4" ]]; then
  [[ -n "$FEEDBACK_CLIENT" ]] || {
    echo "ERR dmabuf v4 advertised but no feedback client found; install weston-simple-dmabuf-feedback or set MANIFOLD_DMABUF_FEEDBACK_CLIENT" >&2
    exit 1
  }
  printf '#!/usr/bin/env bash\nexec env WAYLAND_DEBUG=1 %q >%q 2>%q\n' "$FEEDBACK_CLIENT" "$FEEDBACK_LOG" "$FEEDBACK_TRACE" > "$FEEDBACK_WRAPPER"
  chmod +x "$FEEDBACK_WRAPPER"
  ctl spawn "$FEEDBACK_WRAPPER" > /dev/null

  for _ in $(seq 1 80); do
    if [[ -s "$FEEDBACK_TRACE" ]] && grep -q "zwp_linux_dmabuf_feedback_v1" "$FEEDBACK_TRACE"; then
      break
    fi
    sleep 0.1
  done
  grep -q "zwp_linux_dmabuf_feedback_v1" "$FEEDBACK_TRACE" || {
    echo "ERR dmabuf v4 feedback client did not receive zwp_linux_dmabuf_feedback_v1 events" >&2
    echo "status_json=$STATUS_JSON" >&2
    cat "$FEEDBACK_TRACE" >&2 || true
    exit 1
  }
  FEEDBACK_EVENT_COUNT="$(grep -Ec 'zwp_linux_dmabuf_feedback_v1@[0-9]+\.(done|format_table|main_device|tranche_done|tranche_formats|tranche_target_device|tranche_flags)' "$FEEDBACK_TRACE" || true)"
  [[ "$FEEDBACK_EVENT_COUNT" != "0" ]] || {
    echo "ERR dmabuf v4 feedback client saw the feedback interface but no feedback events" >&2
    echo "status_json=$STATUS_JSON" >&2
    cat "$FEEDBACK_TRACE" >&2 || true
    exit 1
  }
fi

echo "OK positive dmabuf validation passed"
echo "import_client=$IMPORT_CLIENT ${IMPORT_ARGS[*]}"
echo "require_explicit_sync=$REQUIRE_EXPLICIT_SYNC"
echo "status_json=$STATUS_JSON"
echo "log=$LOG"
echo "import_log=$IMPORT_LOG"
echo "wayland_trace=$TRACE"
if [[ "$DMABUF_VERSION" == "v4" ]]; then
  echo "feedback_client=$FEEDBACK_CLIENT"
  echo "feedback_event_count=$FEEDBACK_EVENT_COUNT"
  echo "feedback_log=$FEEDBACK_LOG"
  echo "feedback_trace=$FEEDBACK_TRACE"
fi

ctl quit > /dev/null || true
wait "$PID" || true
trap - EXIT
