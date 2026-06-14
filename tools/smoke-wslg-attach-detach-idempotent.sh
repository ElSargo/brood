#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

require_host_presentation_environment

CLIENT="${MANIFOLD_SMOKE_CLIENT:-}"
if [[ -z "$CLIENT" ]]; then
  if command -v weston-simple-shm >/dev/null 2>&1; then
    CLIENT="$(command -v weston-simple-shm)"
  elif command -v weston-simple-egl >/dev/null 2>&1; then
    CLIENT="$(command -v weston-simple-egl)"
  else
    echo "ERR no smoke client found; install weston-simple-shm or set MANIFOLD_SMOKE_CLIENT" >&2
    exit 2
  fi
fi

MAX_REATTACH_LATENCY_US="${MANIFOLD_MAX_IDEMPOTENT_REATTACH_LATENCY_US:-75000}"
if ! [[ "$MAX_REATTACH_LATENCY_US" =~ ^[0-9]+$ ]] || (( MAX_REATTACH_LATENCY_US < 1 )); then
  echo "ERR MANIFOLD_MAX_IDEMPOTENT_REATTACH_LATENCY_US must be a positive integer" >&2
  exit 2
fi
MAX_DETACH_LATENCY_US="${MANIFOLD_MAX_IDEMPOTENT_DETACH_LATENCY_US:-75000}"
if ! [[ "$MAX_DETACH_LATENCY_US" =~ ^[0-9]+$ ]] || (( MAX_DETACH_LATENCY_US < 1 )); then
  echo "ERR MANIFOLD_MAX_IDEMPOTENT_DETACH_LATENCY_US must be a positive integer" >&2
  exit 2
fi

cargo build -p manifoldd --offline >/dev/null

exec 9>/tmp/manifold-wslg-smoke.lock
flock 9

TMPDIR="$(mktemp -d /tmp/manifold-attach-detach-idempotent.XXXXXX)"
SOCK="$TMPDIR/control.sock"
LOG="$TMPDIR/manifoldd.log"
TRACE="$TMPDIR/client-wayland.log"
WRAPPER="$TMPDIR/client-wrapper.sh"

cleanup() {
  if [[ -n "${PID:-}" ]] && kill -0 "$PID" 2>/dev/null; then
    target/debug/manifoldd ctl --control-socket "$SOCK" quit >/dev/null 2>&1 || true
    sleep 0.5
    kill "$PID" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

printf '#!/usr/bin/env bash\nexec env WAYLAND_DEBUG=1 %q 2>%q\n' "$CLIENT" "$TRACE" >"$WRAPPER"
chmod +x "$WRAPPER"

RUST_LOG="${RUST_LOG:-manifoldd=info,smithay=warn}" \
  target/debug/manifoldd run --control-socket "$SOCK" -c "$WRAPPER" >"$LOG" 2>&1 &
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

require_count_unchanged() {
  local before="$1"
  local after="$2"
  local key="$3"
  local before_value
  local after_value
  before_value="$(numeric_field "$before" "$key")"
  after_value="$(numeric_field "$after" "$key")"
  [[ "$after_value" == "$before_value" ]] || {
    echo "ERR expected $key unchanged: before=$before after=$after" >&2
    exit 1
  }
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
  tail -120 "$LOG" >&2 || true
  cat "$TRACE" >&2 || true
  exit 1
}

STATUS_INITIAL="$(wait_status '[[ "$(field "$status" presentation)" == "Attached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" frames)" =~ ^[0-9]+$ ]] && (( "$(field "$status" frames)" >= 2 )) && [[ "$(field "$status" commits)" =~ ^[0-9]+$ ]] && (( "$(field "$status" commits)" >= 1 ))')"
ATTACHES_INITIAL="$(numeric_field "$STATUS_INITIAL" attach_count)"
DETACHES_INITIAL="$(numeric_field "$STATUS_INITIAL" detach_count)"
FRAMES_INITIAL="$(numeric_field "$STATUS_INITIAL" frames)"

ctl attach >/dev/null
ctl attach >/dev/null
sleep 0.2
STATUS_AFTER_ATTACHED_ATTACH="$(ctl status)"
[[ "$(field "$STATUS_AFTER_ATTACHED_ATTACH" presentation)" == "Attached" ]] || {
  echo "ERR attach while attached changed presentation state: $STATUS_AFTER_ATTACHED_ATTACH" >&2
  exit 1
}
require_count_unchanged "$STATUS_INITIAL" "$STATUS_AFTER_ATTACHED_ATTACH" attach_count
require_count_unchanged "$STATUS_INITIAL" "$STATUS_AFTER_ATTACHED_ATTACH" detach_count
[[ "$(numeric_field "$STATUS_AFTER_ATTACHED_ATTACH" windows)" -ge 1 ]] || {
  echo "ERR attach while attached lost nested window: $STATUS_AFTER_ATTACHED_ATTACH" >&2
  exit 1
}

ctl detach >/dev/null
STATUS_DETACHED="$(wait_status '[[ "$(field "$status" presentation)" == "Detached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" detach_count)" =~ ^[0-9]+$ ]] && (( "$(field "$status" detach_count)" == DETACHES_INITIAL + 1 ))')"
DETACHES_APPLIED="$(numeric_field "$STATUS_DETACHED" detach_count)"
LAST_DETACH_LATENCY_US="$(numeric_field "$STATUS_DETACHED" last_detach_latency_us)"
(( LAST_DETACH_LATENCY_US <= MAX_DETACH_LATENCY_US )) || {
  echo "ERR detach latency ${LAST_DETACH_LATENCY_US}us exceeds ${MAX_DETACH_LATENCY_US}us" >&2
  echo "status=$STATUS_DETACHED" >&2
  exit 1
}

ctl detach >/dev/null
ctl close-frontend >/dev/null
ctl frontend-close >/dev/null
sleep 0.2
STATUS_AFTER_DETACHED_DETACH="$(ctl status)"
[[ "$(field "$STATUS_AFTER_DETACHED_DETACH" presentation)" == "Detached" ]] || {
  echo "ERR detach while detached changed presentation state: $STATUS_AFTER_DETACHED_DETACH" >&2
  exit 1
}
[[ "$(numeric_field "$STATUS_AFTER_DETACHED_DETACH" detach_count)" == "$DETACHES_APPLIED" ]] || {
  echo "ERR detach while detached changed detach_count: before=$STATUS_DETACHED after=$STATUS_AFTER_DETACHED_DETACH" >&2
  exit 1
}
require_count_unchanged "$STATUS_DETACHED" "$STATUS_AFTER_DETACHED_DETACH" attach_count
[[ "$(numeric_field "$STATUS_AFTER_DETACHED_DETACH" windows)" -ge 1 ]] || {
  echo "ERR detach while detached lost nested window: $STATUS_AFTER_DETACHED_DETACH" >&2
  exit 1
}

ctl attach >/dev/null
STATUS_REATTACHED="$(wait_status '[[ "$(field "$status" presentation)" == "Attached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" attach_count)" =~ ^[0-9]+$ ]] && (( "$(field "$status" attach_count)" == ATTACHES_INITIAL + 1 )) && [[ "$(field "$status" frames)" =~ ^[0-9]+$ ]] && (( "$(field "$status" frames)" > FRAMES_INITIAL ))')"
LAST_ATTACH_LATENCY_US="$(numeric_field "$STATUS_REATTACHED" last_attach_latency_us)"
LAST_ATTACH_TO_FRAME_US="$(numeric_field "$STATUS_REATTACHED" last_attach_to_frame_us)"
(( LAST_ATTACH_LATENCY_US <= MAX_REATTACH_LATENCY_US )) || {
  echo "ERR reattach latency ${LAST_ATTACH_LATENCY_US}us exceeds ${MAX_REATTACH_LATENCY_US}us" >&2
  echo "status=$STATUS_REATTACHED" >&2
  exit 1
}
(( LAST_ATTACH_TO_FRAME_US <= MAX_REATTACH_LATENCY_US )) || {
  echo "ERR reattach-to-frame latency ${LAST_ATTACH_TO_FRAME_US}us exceeds ${MAX_REATTACH_LATENCY_US}us" >&2
  echo "status=$STATUS_REATTACHED" >&2
  exit 1
}

ctl attach >/dev/null
ctl attach >/dev/null
sleep 0.2
STATUS_AFTER_REATTACHED_ATTACH="$(ctl status)"
[[ "$(field "$STATUS_AFTER_REATTACHED_ATTACH" presentation)" == "Attached" ]] || {
  echo "ERR attach after reattach changed presentation state: $STATUS_AFTER_REATTACHED_ATTACH" >&2
  exit 1
}
require_count_unchanged "$STATUS_REATTACHED" "$STATUS_AFTER_REATTACHED_ATTACH" attach_count
require_count_unchanged "$STATUS_REATTACHED" "$STATUS_AFTER_REATTACHED_ATTACH" detach_count
[[ "$(numeric_field "$STATUS_AFTER_REATTACHED_ATTACH" windows)" -ge 1 ]] || {
  echo "ERR attach after reattach lost nested window: $STATUS_AFTER_REATTACHED_ATTACH" >&2
  exit 1
}

echo "OK WSLg attach/detach idempotency smoke passed"
echo "client=$CLIENT"
echo "status_initial=$STATUS_INITIAL"
echo "status_after_attached_attach=$STATUS_AFTER_ATTACHED_ATTACH"
echo "status_detached=$STATUS_DETACHED"
echo "status_after_detached_detach=$STATUS_AFTER_DETACHED_DETACH"
echo "status_reattached=$STATUS_REATTACHED"
echo "status_after_reattached_attach=$STATUS_AFTER_REATTACHED_ATTACH"
echo "max_detach_latency_budget_us=$MAX_DETACH_LATENCY_US"
echo "max_reattach_latency_budget_us=$MAX_REATTACH_LATENCY_US"
echo "log=$LOG"
echo "wayland_trace=$TRACE"

ctl quit >/dev/null || true
wait "$PID" || true
trap - EXIT
