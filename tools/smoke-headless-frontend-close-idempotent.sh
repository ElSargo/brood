#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

if ! command -v weston-simple-shm > /dev/null 2>&1; then
  echo "ERR weston-simple-shm is required for headless frontend-close smoke" >&2
  exit 2
fi

cargo build -p manifoldd --offline > /dev/null

TMPDIR="$(mktemp -d /tmp/manifold-headless-frontend-close.XXXXXX)"
DBUS_NAME="org.manifold.Test.p$$"
LOG="$TMPDIR/manifoldd.log"
CLIENT_LOG="$TMPDIR/client.log"
WRAPPER="$TMPDIR/client-wrapper.sh"

cleanup() {
  if [[ -n "${PID:-}" ]] && kill -0 "$PID" 2> /dev/null; then
    target/debug/manifold ctl --dbus-name "$DBUS_NAME" quit > /dev/null 2>&1 || true
    sleep 0.5
    kill "$PID" > /dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

cat > "$WRAPPER" << EOF
#!/usr/bin/env bash
exec weston-simple-shm >"$CLIENT_LOG" 2>&1
EOF
chmod +x "$WRAPPER"

env WAYLAND_DISPLAY=manifold-missing-wayland DISPLAY= \
  RUST_LOG="${RUST_LOG:-manifoldd=info,smithay=warn}" \
  target/debug/manifold run --headless --dbus-name "$DBUS_NAME" -c "$WRAPPER" > "$LOG" 2>&1 &
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
  for _ in $(seq 1 100); do
    status="$(ctl status)"
    if eval "$predicate"; then
      echo "$status"
      return 0
    fi
    sleep 0.1
  done
  echo "ERR timed out waiting for status predicate: $predicate" >&2
  ctl status >&2 || true
  cat "$CLIENT_LOG" >&2 || true
  tail -120 "$LOG" >&2 || true
  exit 1
}

STATUS_MAPPED="$(wait_status '[[ "$(field "$status" presentation)" == "Detached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" commits)" =~ ^[0-9]+$ ]] && (( "$(field "$status" commits)" >= 1 ))')"
COMMITS_BEFORE="$(numeric_field "$STATUS_MAPPED" commits)"
DETACHES_BEFORE="$(numeric_field "$STATUS_MAPPED" detach_count)"
ATTACHES_BEFORE="$(numeric_field "$STATUS_MAPPED" attach_count)"
ATTACH_FAILURES_BEFORE="$(numeric_field "$STATUS_MAPPED" presentation_attach_failures)"

CLOSE_RESPONSE_1="$(ctl close-frontend | tr -d '\r')"
CLOSE_RESPONSE_2="$(ctl frontend-close | tr -d '\r')"
[[ "$CLOSE_RESPONSE_1" == "OK frontend close requested" ]] || {
  echo "ERR unexpected close-frontend response: $CLOSE_RESPONSE_1" >&2
  exit 1
}
[[ "$CLOSE_RESPONSE_2" == "OK frontend close requested" ]] || {
  echo "ERR unexpected frontend-close response: $CLOSE_RESPONSE_2" >&2
  exit 1
}

STATUS_CLOSED="$(wait_status '[[ "$(field "$status" presentation)" == "Detached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" commits)" =~ ^[0-9]+$ ]] && (( "$(field "$status" commits)" >= COMMITS_BEFORE ))')"
[[ "$(numeric_field "$STATUS_CLOSED" detach_count)" == "$DETACHES_BEFORE" ]] || {
  echo "ERR detached close should not increment detach_count: before=$STATUS_MAPPED after=$STATUS_CLOSED" >&2
  exit 1
}
[[ "$(numeric_field "$STATUS_CLOSED" attach_count)" == "$ATTACHES_BEFORE" ]] || {
  echo "ERR detached close should not increment attach_count: before=$STATUS_MAPPED after=$STATUS_CLOSED" >&2
  exit 1
}
[[ "$(field "$STATUS_CLOSED" last_detach_latency_us)" == "none" ]] || {
  echo "ERR detached close should not record detach latency: $STATUS_CLOSED" >&2
  exit 1
}

ctl attach > /dev/null
STATUS_FAILED_ATTACH="$(wait_status '[[ "$(field "$status" presentation)" == "Detached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" presentation_attach_failures)" =~ ^[0-9]+$ ]] && (( "$(field "$status" presentation_attach_failures)" > ATTACH_FAILURES_BEFORE )) && [[ "$(field "$status" presentation_runtime_failures)" == "0" ]] && [[ "$(field "$status" presentation_last_error)" != "none" ]]')"
[[ "$(numeric_field "$STATUS_FAILED_ATTACH" detach_count)" == "$DETACHES_BEFORE" ]] || {
  echo "ERR failed attach should not change detach_count: before=$STATUS_MAPPED after=$STATUS_FAILED_ATTACH" >&2
  exit 1
}
[[ "$(numeric_field "$STATUS_FAILED_ATTACH" attach_count)" == "$ATTACHES_BEFORE" ]] || {
  echo "ERR failed attach should not count as applied attach: before=$STATUS_MAPPED after=$STATUS_FAILED_ATTACH" >&2
  exit 1
}
[[ "$(field "$STATUS_FAILED_ATTACH" last_attach_latency_us)" == "none" ]] || {
  echo "ERR failed attach should not record attach latency: $STATUS_FAILED_ATTACH" >&2
  exit 1
}
[[ "$(field "$STATUS_FAILED_ATTACH" last_attach_to_frame_us)" == "none" ]] || {
  echo "ERR failed attach should not record attach-to-frame latency: $STATUS_FAILED_ATTACH" >&2
  exit 1
}

echo "OK headless frontend-close idempotency smoke passed"
echo "status_mapped=$STATUS_MAPPED"
echo "status_after_close=$STATUS_CLOSED"
echo "status_after_failed_attach=$STATUS_FAILED_ATTACH"
echo "close_response_1=$CLOSE_RESPONSE_1"
echo "close_response_2=$CLOSE_RESPONSE_2"
echo "client_log=$CLIENT_LOG"
echo "log=$LOG"

ctl quit > /dev/null || true
wait "$PID" || true
trap - EXIT
