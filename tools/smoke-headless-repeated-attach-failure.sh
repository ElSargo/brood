#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

if ! command -v weston-simple-shm >/dev/null 2>&1; then
  echo "ERR weston-simple-shm is required for repeated attach-failure smoke" >&2
  exit 2
fi

cargo build -p manifoldd --offline >/dev/null

TMPDIR="$(mktemp -d /tmp/manifold-repeated-attach-failure.XXXXXX)"
DBUS_NAME="org.manifold.Test.p$$"
LOG="$TMPDIR/manifoldd.log"
CLIENT_LOG="$TMPDIR/client.log"
WRAPPER="$TMPDIR/client-wrapper.sh"

cleanup() {
  if [[ -n "${PID:-}" ]] && kill -0 "$PID" 2>/dev/null; then
    target/debug/manifoldd ctl --dbus-name "$DBUS_NAME" quit >/dev/null 2>&1 || true
    sleep 0.5
    kill "$PID" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

printf '#!/usr/bin/env bash\nexec weston-simple-shm >%q 2>&1\n' "$CLIENT_LOG" >"$WRAPPER"
chmod +x "$WRAPPER"

env WAYLAND_DISPLAY=manifold-missing-wayland DISPLAY= \
  RUST_LOG="${RUST_LOG:-manifoldd=info,smithay=warn}" \
  target/debug/manifoldd run --headless --dbus-name "$DBUS_NAME" -c "$WRAPPER" >"$LOG" 2>&1 &
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
ATTACHES_BEFORE="$(numeric_field "$STATUS_MAPPED" attach_count)"
DETACHES_BEFORE="$(numeric_field "$STATUS_MAPPED" detach_count)"
FAILURES_BEFORE="$(numeric_field "$STATUS_MAPPED" presentation_attach_failures)"
COMMITS_BEFORE="$(numeric_field "$STATUS_MAPPED" commits)"

ctl attach >/dev/null
STATUS_AFTER_FIRST="$(wait_status '[[ "$(field "$status" presentation)" == "Detached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" presentation_attach_failures)" =~ ^[0-9]+$ ]] && (( "$(field "$status" presentation_attach_failures)" >= FAILURES_BEFORE + 1 )) && [[ "$(field "$status" presentation_last_error)" != "none" ]]')"

ctl attach >/dev/null
STATUS_AFTER_SECOND="$(wait_status '[[ "$(field "$status" presentation)" == "Detached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" presentation_attach_failures)" =~ ^[0-9]+$ ]] && (( "$(field "$status" presentation_attach_failures)" >= FAILURES_BEFORE + 2 )) && [[ "$(field "$status" presentation_last_error)" != "none" ]]')"

[[ "$(numeric_field "$STATUS_AFTER_SECOND" attach_count)" == "$ATTACHES_BEFORE" ]] || {
  echo "ERR failed attach retries should not count as applied attaches: before=$STATUS_MAPPED after=$STATUS_AFTER_SECOND" >&2
  exit 1
}
[[ "$(numeric_field "$STATUS_AFTER_SECOND" detach_count)" == "$DETACHES_BEFORE" ]] || {
  echo "ERR failed attach retries should not change detach_count: before=$STATUS_MAPPED after=$STATUS_AFTER_SECOND" >&2
  exit 1
}
[[ "$(field "$STATUS_AFTER_SECOND" presentation_runtime_failures)" == "0" ]] || {
  echo "ERR failed attach retries should not count as runtime redraw failures: $STATUS_AFTER_SECOND" >&2
  exit 1
}
[[ "$(field "$STATUS_AFTER_SECOND" last_attach_latency_us)" == "none" ]] || {
  echo "ERR failed attach retries should not record attach latency: $STATUS_AFTER_SECOND" >&2
  exit 1
}
[[ "$(field "$STATUS_AFTER_SECOND" last_attach_to_frame_us)" == "none" ]] || {
  echo "ERR failed attach retries should not record attach-to-frame latency: $STATUS_AFTER_SECOND" >&2
  exit 1
}
[[ "$(numeric_field "$STATUS_AFTER_SECOND" commits)" -ge "$COMMITS_BEFORE" ]] || {
  echo "ERR nested client commits regressed after failed attach retries: before=$STATUS_MAPPED after=$STATUS_AFTER_SECOND" >&2
  exit 1
}

echo "OK headless repeated attach-failure smoke passed"
echo "status_mapped=$STATUS_MAPPED"
echo "status_after_first=$STATUS_AFTER_FIRST"
echo "status_after_second=$STATUS_AFTER_SECOND"
echo "client_log=$CLIENT_LOG"
echo "log=$LOG"

ctl quit >/dev/null || true
wait "$PID" || true
trap - EXIT
