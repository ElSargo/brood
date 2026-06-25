#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

if ! command -v weston-simple-shm >/dev/null 2>&1; then
  echo "ERR weston-simple-shm is required for headless control error smoke" >&2
  exit 2
fi

cargo build -p manifoldd --offline >/dev/null

TMPDIR="$(mktemp -d /tmp/manifold-control-errors.XXXXXX)"
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

expect_ctl_error() {
  local name="$1"
  shift
  local output

  if output="$(target/debug/manifoldd ctl --dbus-name "$DBUS_NAME" "$@" 2>&1)"; then
    echo "ERR $name expected ctl failure, got success: $output" >&2
    tail -120 "$LOG" >&2 || true
    exit 1
  fi
  printf '%s\n' "$output"
}

expect_json_error() {
  local name="$1"
  local payload="$2"
  local output

  if output="$(target/debug/manifoldd ctl --dbus-name "$DBUS_NAME" --json "$payload" 2>&1)"; then
    echo "ERR $name expected JSON ctl failure, got success: $output" >&2
    tail -120 "$LOG" >&2 || true
    exit 1
  fi
  printf '%s\n' "$output"
}

STATUS_BEFORE="$(wait_status '[[ "$(field "$status" presentation)" == "Detached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" commits)" =~ ^[0-9]+$ ]] && (( "$(field "$status" commits)" >= 1 ))')"
ATTACHES_BEFORE="$(numeric_field "$STATUS_BEFORE" attach_count)"
DETACHES_BEFORE="$(numeric_field "$STATUS_BEFORE" detach_count)"
WINDOWS_BEFORE="$(numeric_field "$STATUS_BEFORE" windows)"
COMMITS_BEFORE="$(numeric_field "$STATUS_BEFORE" commits)"
INPUT_BEFORE="$(numeric_field "$STATUS_BEFORE" input_events)"

EMPTY_RESPONSE="$(expect_json_error empty-json '')"
MALFORMED_JSON_RESPONSE="$(expect_json_error malformed-json '[')"
BAD_JSON_TYPE_RESPONSE="$(expect_json_error bad-json-type '{}')"
UNKNOWN_RESPONSE="$(expect_ctl_error unknown-command no-such-command)"
SPAWN_RESPONSE="$(expect_ctl_error spawn-missing-command spawn)"
INJECT_RESPONSE="$(expect_ctl_error bad-inject inject motion na 1)"
ATTACH_ARGS_RESPONSE="$(expect_ctl_error attach-extra-arg attach nope)"

STATUS_AFTER="$(wait_status '[[ "$(field "$status" presentation)" == "Detached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= WINDOWS_BEFORE )) && [[ "$(field "$status" commits)" =~ ^[0-9]+$ ]] && (( "$(field "$status" commits)" >= COMMITS_BEFORE ))')"

[[ "$(numeric_field "$STATUS_AFTER" attach_count)" == "$ATTACHES_BEFORE" ]] || {
  echo "ERR invalid control commands changed attach_count: before=$STATUS_BEFORE after=$STATUS_AFTER" >&2
  exit 1
}
[[ "$(numeric_field "$STATUS_AFTER" detach_count)" == "$DETACHES_BEFORE" ]] || {
  echo "ERR invalid control commands changed detach_count: before=$STATUS_BEFORE after=$STATUS_AFTER" >&2
  exit 1
}
[[ "$(numeric_field "$STATUS_AFTER" windows)" -ge "$WINDOWS_BEFORE" ]] || {
  echo "ERR invalid control commands lost nested windows: before=$STATUS_BEFORE after=$STATUS_AFTER" >&2
  exit 1
}
[[ "$(numeric_field "$STATUS_AFTER" input_events)" == "$INPUT_BEFORE" ]] || {
  echo "ERR invalid inject command changed input counters: before=$STATUS_BEFORE after=$STATUS_AFTER" >&2
  exit 1
}

echo "OK headless control error smoke passed"
echo "status_before=$STATUS_BEFORE"
echo "status_after=$STATUS_AFTER"
echo "empty_response=$EMPTY_RESPONSE"
echo "malformed_json_response=$MALFORMED_JSON_RESPONSE"
echo "bad_json_type_response=$BAD_JSON_TYPE_RESPONSE"
echo "unknown_response=$UNKNOWN_RESPONSE"
echo "spawn_response=$SPAWN_RESPONSE"
echo "inject_response=$INJECT_RESPONSE"
echo "attach_args_response=$ATTACH_ARGS_RESPONSE"
echo "client_log=$CLIENT_LOG"
echo "log=$LOG"

ctl quit >/dev/null || true
wait "$PID" || true
trap - EXIT
