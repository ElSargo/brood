#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

KEEP_RUNNING=0
SAMPLE_SECONDS="${MANIFOLD_NESTED_SAMPLE_SECONDS:-2}"
DEV_CLIENT="${MANIFOLD_NESTED_DEV_CLIENT:-foot}"
OUT_DIR="${MANIFOLD_NESTED_OUT_DIR:-}"

usage() {
  cat <<'EOF'
usage: tools/dev-nested-manifold.sh [--keep-running] [--out-dir DIR] [--dev-client CMD]

Starts a stable Manifold instance on the host display, then starts a dev
Manifold instance as a hosted app inside the stable instance. The dev instance
can then host its own nested client. The harness records status-json samples
from both instances for basic frame/latency analysis.

Options:
  --keep-running    leave both instances running and print DBus destinations
  --out-dir DIR     write logs/status under DIR instead of mktemp
  --dev-client CMD  command to spawn inside the dev Manifold instance

Environment:
  MANIFOLD_NESTED_SAMPLE_SECONDS  status sampling duration, default 2
  MANIFOLD_NESTED_DEV_CLIENT      default dev client command, default foot
  MANIFOLD_NESTED_OUT_DIR         default output directory
EOF
}

while (($#)); do
  case "$1" in
    --keep-running)
      KEEP_RUNNING=1
      shift
      ;;
    --out-dir)
      [[ $# -ge 2 ]] || {
        echo "ERR --out-dir requires a directory" >&2
        exit 2
      }
      OUT_DIR="$2"
      shift 2
      ;;
    --dev-client)
      [[ $# -ge 2 ]] || {
        echo "ERR --dev-client requires a command" >&2
        exit 2
      }
      DEV_CLIENT="$2"
      shift 2
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "ERR unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

require_host_presentation_environment

cargo build -p manifoldd --offline >/dev/null

exec 9>/tmp/manifold-nested-dev.lock
flock 9

if [[ -z "$OUT_DIR" ]]; then
  OUT_DIR="$(mktemp -d /tmp/manifold-nested-dev.XXXXXX)"
else
  mkdir -p "$OUT_DIR"
fi

STABLE_DBUS_DESTINATION="org.manifold.Dev.p$$.stable"
DEV_DBUS_DESTINATION="org.manifold.Dev.p$$.dev"
STABLE_LOG="$OUT_DIR/stable.log"
DEV_LOG="$OUT_DIR/dev.log"
STABLE_STATUS_JSONL="$OUT_DIR/stable-status.jsonl"
DEV_STATUS_JSONL="$OUT_DIR/dev-status.jsonl"

cleanup() {
  if [[ "$KEEP_RUNNING" == "1" ]]; then
    return
  fi
  target/debug/manifoldd ctl --dbus-name "$DEV_DBUS_DESTINATION" quit >/dev/null 2>&1 || true
  target/debug/manifoldd ctl --dbus-name "$STABLE_DBUS_DESTINATION" quit >/dev/null 2>&1 || true
  sleep 0.4
  if [[ -n "${DEV_PID:-}" ]] && kill -0 "$DEV_PID" 2>/dev/null; then
    kill "$DEV_PID" >/dev/null 2>&1 || true
  fi
  if [[ -n "${STABLE_PID:-}" ]] && kill -0 "$STABLE_PID" 2>/dev/null; then
    kill "$STABLE_PID" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

ctl_stable() {
  target/debug/manifoldd ctl --dbus-name "$STABLE_DBUS_DESTINATION" "$@"
}

ctl_dev() {
  target/debug/manifoldd ctl --dbus-name "$DEV_DBUS_DESTINATION" "$@"
}

ctl_stable_json() {
  target/debug/manifoldd ctl --dbus-name "$STABLE_DBUS_DESTINATION" --json "$1"
}

ctl_dev_json() {
  target/debug/manifoldd ctl --dbus-name "$DEV_DBUS_DESTINATION" --json "$1"
}

json_field() {
  local json="$1"
  local key="$2"
  python - "$json" "$key" <<'PY'
import json
import sys

data = json.loads(sys.argv[1])
value = data.get(sys.argv[2])
if value is None:
    print("null")
elif isinstance(value, bool):
    print("true" if value else "false")
else:
    print(value)
PY
}

wait_status_json() {
  local label="$1"
  local command="$2"
  local predicate="$3"
  local log="$4"
  local status

  for _ in $(seq 1 160); do
    if ! status="$($command status-json 2>/dev/null)"; then
      sleep 0.1
      continue
    fi
    if python - "$status" "$predicate" <<'PY'
import json
import sys

status = json.loads(sys.argv[1])
predicate = sys.argv[2]
if not eval(predicate, {"__builtins__": {}}, {"s": status}):
    raise SystemExit(1)
PY
    then
      echo "$status"
      return 0
    fi
    sleep 0.1
  done

  echo "ERR timed out waiting for $label" >&2
  $command status-json >&2 || true
  tail -120 "$log" >&2 || true
  exit 1
}

sample_statuses() {
  local deadline
  deadline=$((SECONDS + SAMPLE_SECONDS))
  : >"$STABLE_STATUS_JSONL"
  : >"$DEV_STATUS_JSONL"
  while ((SECONDS < deadline)); do
    local now
    now="$(date +%s.%N)"
    if stable_status="$(ctl_stable status-json 2>/dev/null)"; then
      printf '{"sample_time":%s,"role":"stable","status":%s}\n' "$now" "$stable_status" \
        >>"$STABLE_STATUS_JSONL"
    fi
    if dev_status="$(ctl_dev status-json 2>/dev/null)"; then
      printf '{"sample_time":%s,"role":"dev","status":%s}\n' "$now" "$dev_status" \
        >>"$DEV_STATUS_JSONL"
    fi
    sleep 0.1
  done
}

RUST_LOG="${RUST_LOG:-manifoldd=info,smithay=warn}" \
  target/debug/manifoldd run --dbus-name "$STABLE_DBUS_DESTINATION" >"$STABLE_LOG" 2>&1 &
STABLE_PID=$!

wait_for_manifoldd_control "$STABLE_DBUS_DESTINATION" "$STABLE_PID" "$STABLE_LOG"

DEV_SPAWN_JSON="$(python - "$DEV_DBUS_DESTINATION" "$DEV_LOG" <<'PY'
import json
import shlex
import sys

destination, log = sys.argv[1], sys.argv[2]
command = (
    "exec target/debug/manifoldd run "
    f"--dbus-name {shlex.quote(destination)} "
    f">{shlex.quote(log)} 2>&1"
)
print(json.dumps([
    "spawn",
    "bash",
    "-lc",
    command,
]))
PY
)"
ctl_stable_json "$DEV_SPAWN_JSON" >/dev/null

wait_for_manifoldd_control "$DEV_DBUS_DESTINATION" "$STABLE_PID" "$STABLE_LOG"

STABLE_READY="$(
  wait_status_json \
    "stable to host dev presentation" \
    ctl_stable \
    's.get("presentation") == "Attached" and s.get("windows", 0) >= 1 and s.get("frames", 0) >= 2' \
    "$STABLE_LOG"
)"

DEV_CLIENT_JSON="$(python - "$DEV_CLIENT" <<'PY'
import json
import shlex
import sys

print(json.dumps(["spawn", *shlex.split(sys.argv[1])]))
PY
)"
ctl_dev_json "$DEV_CLIENT_JSON" >/dev/null

DEV_READY="$(
  wait_status_json \
    "dev to host nested client" \
    ctl_dev \
    's.get("presentation") == "Attached" and s.get("windows", 0) >= 1 and s.get("frames", 0) >= 2' \
    "$DEV_LOG"
)"

sample_statuses

echo "OK nested Manifold dev harness ready"
echo "out_dir=$OUT_DIR"
echo "stable_dbus_destination=$STABLE_DBUS_DESTINATION"
echo "dev_dbus_destination=$DEV_DBUS_DESTINATION"
echo "stable_log=$STABLE_LOG"
echo "dev_log=$DEV_LOG"
echo "stable_status_jsonl=$STABLE_STATUS_JSONL"
echo "dev_status_jsonl=$DEV_STATUS_JSONL"
echo "dev_client=$DEV_CLIENT"
echo "stable_windows=$(json_field "$STABLE_READY" windows)"
echo "stable_frames=$(json_field "$STABLE_READY" frames)"
echo "dev_windows=$(json_field "$DEV_READY" windows)"
echo "dev_frames=$(json_field "$DEV_READY" frames)"

if [[ "$KEEP_RUNNING" == "1" ]]; then
  echo "keep_running=1"
  echo "stable_pid=$STABLE_PID"
  echo "Stop with:"
  echo "  target/debug/manifoldd ctl --dbus-name $DEV_DBUS_DESTINATION quit"
  echo "  target/debug/manifoldd ctl --dbus-name $STABLE_DBUS_DESTINATION quit"
  wait "$STABLE_PID"
else
  ctl_dev quit >/dev/null || true
  ctl_stable quit >/dev/null || true
  wait "$STABLE_PID" || true
  trap - EXIT
fi
