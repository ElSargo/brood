#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

require_host_presentation_environment

CLIENT="${MANIFOLD_SMOKE_CLIENT:-}"
if [[ -z "$CLIENT" ]]; then
  if command -v weston-simple-egl >/dev/null 2>&1; then
    CLIENT="$(command -v weston-simple-egl)"
  elif command -v weston-terminal >/dev/null 2>&1; then
    CLIENT="$(command -v weston-terminal)"
  else
    echo "ERR no smoke client found; install weston-simple-egl or set MANIFOLD_SMOKE_CLIENT" >&2
    exit 2
  fi
fi

CYCLES="${MANIFOLD_RESUME_STRESS_CYCLES:-5}"
if ! [[ "$CYCLES" =~ ^[0-9]+$ ]] || (( CYCLES < 1 )); then
  echo "ERR MANIFOLD_RESUME_STRESS_CYCLES must be a positive integer" >&2
  exit 2
fi
MAX_STRESS_DETACH_LATENCY_US="${MANIFOLD_RESUME_STRESS_MAX_DETACH_LATENCY_US:-75000}"
MAX_STRESS_ATTACH_LATENCY_US="${MANIFOLD_RESUME_STRESS_MAX_ATTACH_LATENCY_US:-75000}"
for setting in MAX_STRESS_DETACH_LATENCY_US MAX_STRESS_ATTACH_LATENCY_US; do
  value="${!setting}"
  if ! [[ "$value" =~ ^[0-9]+$ ]] || (( value < 1 )); then
    echo "ERR $setting must be a positive integer" >&2
    exit 2
  fi
done

cargo build -p manifoldd --offline >/dev/null

exec 9>/tmp/manifold-host-smoke.lock
flock 9

TMPDIR="$(mktemp -d /tmp/manifold-resume-stress.XXXXXX)"
DBUS_NAME="org.manifold.Test.p$$"
LOG="$TMPDIR/manifoldd.log"
TRACE="$TMPDIR/client-wayland.log"
WRAPPER="$TMPDIR/client-wrapper.sh"

cleanup() {
  if [[ -n "${PID:-}" ]] && kill -0 "$PID" 2>/dev/null; then
    target/debug/manifoldd ctl --dbus-name "$DBUS_NAME" quit >/dev/null 2>&1 || true
    sleep 0.5
    kill "$PID" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

printf '#!/usr/bin/env bash\nexec env WAYLAND_DEBUG=1 %q 2>%q\n' "$CLIENT" "$TRACE" >"$WRAPPER"
chmod +x "$WRAPPER"

RUST_LOG="${RUST_LOG:-manifoldd=info,smithay=warn}" \
  target/debug/manifoldd run --dbus-name "$DBUS_NAME" -c "$WRAPPER" >"$LOG" 2>&1 &
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

require_numeric_field_ge() {
  local status="$1"
  local key="$2"
  local min="$3"
  local value
  value="$(field "$status" "$key")"
  [[ "$value" =~ ^[0-9]+$ ]] || {
    echo "ERR missing numeric $key in status: $status" >&2
    exit 1
  }
  (( value >= min )) || {
    echo "ERR expected $key >= $min, got $value in status: $status" >&2
    exit 1
  }
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
  for _ in $(seq 1 120); do
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
  exit 1
}

INITIAL="$(wait_status '[[ "$(field "$status" presentation)" == "Attached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" frames)" =~ ^[0-9]+$ ]] && (( "$(field "$status" frames)" >= 2 )) && [[ "$(field "$status" commits)" =~ ^[0-9]+$ ]] && (( "$(field "$status" commits)" >= 1 ))')"
FRAMES="$(field "$INITIAL" frames)"
COMMITS="$(field "$INITIAL" commits)"
CALLBACKS="$(field "$INITIAL" client_frame_callbacks)"
ATTACHES="$(field "$INITIAL" attach_count)"
DETACHES="$(field "$INITIAL" detach_count)"

require_numeric_field_ge "$INITIAL" windows 1
require_numeric_field_ge "$INITIAL" frames 2
require_numeric_field_ge "$INITIAL" commits 1
require_numeric_field_ge "$INITIAL" attach_count 1

echo "initial=$INITIAL"

for cycle in $(seq 1 "$CYCLES"); do
  ctl detach >/dev/null
  DETACHED="$(wait_status '[[ "$(field "$status" presentation)" == "Detached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" detach_count)" =~ ^[0-9]+$ ]] && (( "$(field "$status" detach_count)" > DETACHES ))')"
  require_numeric_field_ge "$DETACHED" windows 1
  require_numeric_field_ge "$DETACHED" client_frame_callbacks 0
  LAST_DETACH_LATENCY="$(numeric_field "$DETACHED" last_detach_latency_us)"
  (( LAST_DETACH_LATENCY <= MAX_STRESS_DETACH_LATENCY_US )) || {
    echo "ERR cycle $cycle detach latency ${LAST_DETACH_LATENCY}us exceeds ${MAX_STRESS_DETACH_LATENCY_US}us" >&2
    echo "status=$DETACHED" >&2
    exit 1
  }
  DETACHES="$(field "$DETACHED" detach_count)"
  CALLBACKS_DETACHED="$(field "$DETACHED" client_frame_callbacks)"

  ctl attach >/dev/null
  ATTACHED="$(wait_status '[[ "$(field "$status" presentation)" == "Attached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" attach_count)" =~ ^[0-9]+$ ]] && (( "$(field "$status" attach_count)" > ATTACHES )) && [[ "$(field "$status" frames)" =~ ^[0-9]+$ ]] && (( "$(field "$status" frames)" > FRAMES )) && [[ "$(field "$status" client_frame_callbacks)" =~ ^[0-9]+$ ]] && (( "$(field "$status" client_frame_callbacks)" > CALLBACKS_DETACHED ))')"
  require_numeric_field_ge "$ATTACHED" windows 1
  require_numeric_field_ge "$ATTACHED" commits "$COMMITS"
  LAST_ATTACH_LATENCY="$(numeric_field "$ATTACHED" last_attach_latency_us)"
  LAST_ATTACH_TO_FRAME_LATENCY="$(numeric_field "$ATTACHED" last_attach_to_frame_us)"
  (( LAST_ATTACH_LATENCY <= MAX_STRESS_ATTACH_LATENCY_US )) || {
    echo "ERR cycle $cycle attach latency ${LAST_ATTACH_LATENCY}us exceeds ${MAX_STRESS_ATTACH_LATENCY_US}us" >&2
    echo "status=$ATTACHED" >&2
    exit 1
  }
  (( LAST_ATTACH_TO_FRAME_LATENCY <= MAX_STRESS_ATTACH_LATENCY_US )) || {
    echo "ERR cycle $cycle attach-to-frame latency ${LAST_ATTACH_TO_FRAME_LATENCY}us exceeds ${MAX_STRESS_ATTACH_LATENCY_US}us" >&2
    echo "status=$ATTACHED" >&2
    exit 1
  }
  ATTACHES="$(field "$ATTACHED" attach_count)"
  FRAMES="$(field "$ATTACHED" frames)"
  COMMITS="$(field "$ATTACHED" commits)"
  CALLBACKS="$(field "$ATTACHED" client_frame_callbacks)"

  echo "cycle_${cycle}_detached=$DETACHED"
  echo "cycle_${cycle}_attached=$ATTACHED"
  echo "cycle_${cycle}_last_detach_latency_us=$LAST_DETACH_LATENCY"
  echo "cycle_${cycle}_last_attach_latency_us=$LAST_ATTACH_LATENCY"
  echo "cycle_${cycle}_last_attach_to_frame_us=$LAST_ATTACH_TO_FRAME_LATENCY"
done

FINAL="$(ctl status)"
require_numeric_field_ge "$FINAL" windows 1
require_numeric_field_ge "$FINAL" frames "$FRAMES"
require_numeric_field_ge "$FINAL" commits "$COMMITS"
require_numeric_field_ge "$FINAL" client_frame_callbacks "$CALLBACKS"
require_numeric_field_ge "$FINAL" attach_count "$ATTACHES"
require_numeric_field_ge "$FINAL" detach_count "$DETACHES"

echo "OK WSLg resume stress smoke passed"
echo "cycles=$CYCLES"
echo "max_stress_detach_latency_budget_us=$MAX_STRESS_DETACH_LATENCY_US"
echo "max_stress_attach_latency_budget_us=$MAX_STRESS_ATTACH_LATENCY_US"
echo "client=$CLIENT"
echo "status_final=$FINAL"
echo "log=$LOG"
echo "wayland_trace=$TRACE"

ctl quit >/dev/null || true
wait "$PID" || true
trap - EXIT
