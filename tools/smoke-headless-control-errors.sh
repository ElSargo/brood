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

env WAYLAND_DISPLAY=manifold-missing-wayland DISPLAY= \
  RUST_LOG="${RUST_LOG:-manifoldd=info,smithay=warn}" \
  target/debug/manifoldd run --headless --control-socket "$SOCK" -c "$WRAPPER" >"$LOG" 2>&1 &
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

send_raw() {
  local name="$1"
  local payload_hex="$2"
  local output

  output="$(python3 - "$SOCK" "$payload_hex" <<'PY'
import socket
import sys

sock_path = sys.argv[1]
payload = bytes.fromhex(sys.argv[2])
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
    sock.connect(sock_path)
    sock.sendall(payload)
    sock.shutdown(socket.SHUT_WR)
    response = sock.recv(4096)
sys.stdout.buffer.write(response)
PY
)"
  output="$(tr -d '\r' <<<"$output")"
  [[ "$output" == ERR* ]] || {
    echo "ERR $name expected an ERR response, got: $output" >&2
    tail -120 "$LOG" >&2 || true
    exit 1
  }
  printf '%s\n' "$output"
}

send_oversized_raw() {
  local output

  output="$(python3 - "$SOCK" <<'PY'
import socket
import sys

sock_path = sys.argv[1]
payload = b"x" * (70 * 1024) + b"\n"
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
    sock.connect(sock_path)
    sock.sendall(payload)
    sock.shutdown(socket.SHUT_WR)
    response = sock.recv(4096)
sys.stdout.buffer.write(response)
PY
)"
  output="$(tr -d '\r' <<<"$output")"
  [[ "$output" == ERR*control\ command\ too\ long* ]] || {
    echo "ERR oversized command expected a size-limit ERR response, got: $output" >&2
    tail -120 "$LOG" >&2 || true
    exit 1
  }
  printf '%s\n' "$output"
}

send_idle_raw() {
  local output

  output="$(python3 - "$SOCK" <<'PY'
import socket
import sys
import time

sock_path = sys.argv[1]
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
    sock.connect(sock_path)
    time.sleep(0.5)
    response = sock.recv(4096)
sys.stdout.buffer.write(response)
PY
)"
  output="$(tr -d '\r' <<<"$output")"
  [[ "$output" == ERR*timed\ out\ reading\ control\ command* ]] || {
    echo "ERR idle command expected a read-timeout ERR response, got: $output" >&2
    tail -120 "$LOG" >&2 || true
    exit 1
  }
  printf '%s\n' "$output"
}

STATUS_BEFORE="$(wait_status '[[ "$(field "$status" presentation)" == "Detached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" commits)" =~ ^[0-9]+$ ]] && (( "$(field "$status" commits)" >= 1 ))')"
ATTACHES_BEFORE="$(numeric_field "$STATUS_BEFORE" attach_count)"
DETACHES_BEFORE="$(numeric_field "$STATUS_BEFORE" detach_count)"
WINDOWS_BEFORE="$(numeric_field "$STATUS_BEFORE" windows)"
COMMITS_BEFORE="$(numeric_field "$STATUS_BEFORE" commits)"
INPUT_BEFORE="$(numeric_field "$STATUS_BEFORE" input_events)"

EMPTY_RESPONSE="$(send_raw empty-line 0a)"
MALFORMED_JSON_RESPONSE="$(send_raw malformed-json 5b0a)"
BAD_JSON_TYPE_RESPONSE="$(send_raw bad-json-type 7b7d0a)"
BAD_UTF8_RESPONSE="$(send_raw bad-utf8 ff0a)"
UNKNOWN_RESPONSE="$(send_raw unknown-command 5b226e6f2d737563682d636f6d6d616e64225d0a)"
SPAWN_RESPONSE="$(send_raw spawn-missing-command 5b22737061776e225d0a)"
INJECT_RESPONSE="$(send_raw bad-inject 5b22696e6a656374222c226d6f74696f6e222c226e61222c2231225d0a)"
ATTACH_ARGS_RESPONSE="$(send_raw attach-extra-arg 5b22617474616368222c226e6f7065225d0a)"
OVERSIZED_RESPONSE="$(send_oversized_raw)"
IDLE_RESPONSE="$(send_idle_raw)"

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
echo "bad_utf8_response=$BAD_UTF8_RESPONSE"
echo "unknown_response=$UNKNOWN_RESPONSE"
echo "spawn_response=$SPAWN_RESPONSE"
echo "inject_response=$INJECT_RESPONSE"
echo "attach_args_response=$ATTACH_ARGS_RESPONSE"
echo "oversized_response=$OVERSIZED_RESPONSE"
echo "idle_response=$IDLE_RESPONSE"
echo "client_log=$CLIENT_LOG"
echo "log=$LOG"

ctl quit >/dev/null || true
wait "$PID" || true
trap - EXIT
