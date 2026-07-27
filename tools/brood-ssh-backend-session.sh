#!/usr/bin/env bash
set -euo pipefail

action="${1:-}"
state="${2:-}"

if [[ -z "$action" || -z "$state" ]]; then
  echo "usage: brood-ssh-backend-session <start|stop> STATE [BACKEND_ROOT PACKAGE_ID]" >&2
  exit 2
fi

if [[ "$action" == "stop" ]]; then
  if [[ -r "$state/session.pid" ]]; then
    recorded_pid="$(cat "$state/session.pid")"
    kill "$recorded_pid" 2>/dev/null || true
    for _ in $(seq 1 100); do
      kill -0 "$recorded_pid" 2>/dev/null || break
      sleep 0.05
    done
  fi
  exit 0
fi

if [[ "$action" != "start" || "$#" -ne 4 ]]; then
  echo "usage: brood-ssh-backend-session start STATE BACKEND_ROOT PACKAGE_ID" >&2
  exit 2
fi

backend="$3"
package="$4"
mkdir -p "$state"
state="$(cd "$state" && pwd)"
runtime="$state/runtime"
server_log="$state/server.log"
pid_file="$state/session.pid"
dbus_destination="org.brood.Ssh.Remote"
session_pid=""
relay_pid=""

mkdir -p "$runtime" "$state/artifacts"
chmod 700 "$state" "$runtime"
if [[ -r "$pid_file" ]]; then
  previous_pid="$(cat "$pid_file")"
  if kill -0 "$previous_pid" 2>/dev/null; then
    kill "$previous_pid" 2>/dev/null || true
    for _ in $(seq 1 100); do
      kill -0 "$previous_pid" 2>/dev/null || break
      sleep 0.05
    done
  fi
fi
printf '%s\n' "$$" >"$pid_file"
export XDG_RUNTIME_DIR="$runtime"

cleanup() {
  for child in "$relay_pid" "$session_pid"; do
    if [[ -n "$child" ]] && kill -0 "$child" 2>/dev/null; then
      kill "$child" 2>/dev/null || true
    fi
  done
  for child in "$relay_pid" "$session_pid"; do
    if [[ -n "$child" ]]; then
      wait "$child" 2>/dev/null || true
    fi
  done
  if [[ -r "$pid_file" && "$(cat "$pid_file")" == "$$" ]]; then
    rm -f "$pid_file"
  fi
}
trap cleanup EXIT
trap 'exit 143' HUP INT TERM

for program in brood brood-remote brood-sessiond brood-runner-exec; do
  dependencies="$(ldd "$backend/bin/$program" 2>&1 || true)"
  if grep -Fq 'not found' <<<"$dependencies"; then
    echo "portable backend executable has unavailable dependencies: $program" >&2
    printf '%s\n' "$dependencies" >&2
    exit 1
  fi
done
if ! command -v foot >/dev/null 2>&1; then
  echo "warning: foot is not installed on the remote host; the default init terminal will fail" >&2
fi

"$backend/bin/brood-sessiond" \
  --headless \
  --no-default-modules \
  --config "$backend/brood.toml" \
  --dbus-destination "$dbus_destination" \
  >"$server_log" 2>&1 &
session_pid=$!

ready=0
for _ in $(seq 1 150); do
  if "$backend/bin/brood" ctl \
    --dbus-destination "$dbus_destination" \
    status >/dev/null 2>&1; then
    ready=1
    break
  fi
  if ! kill -0 "$session_pid" 2>/dev/null; then
    echo "remote session backend exited during startup; log=$server_log" >&2
    tail -n 120 "$server_log" >&2 || true
    exit 1
  fi
  sleep 0.1
done

if [[ "$ready" != 1 ]]; then
  echo "timed out waiting for remote session backend; log=$server_log" >&2
  tail -n 120 "$server_log" >&2 || true
  exit 1
fi

"$backend/bin/brood-remote" relay \
  --listen 0.0.0.0:0 \
  --trusted-client-certificate "$state/client.der" \
  --certificate "$state/server.der" \
  --private-key "$state/server-key.der" \
  --dbus-destination "$dbus_destination" \
  --allow-session-package "$package" \
  --artifact-cache "$state/artifacts" &
relay_pid=$!

while kill -0 "$relay_pid" 2>/dev/null; do
  if IFS= read -r -t 1 _; then
    :
  else
    read_status=$?
    if [[ "$read_status" -eq 1 ]]; then
      exit 0
    fi
  fi
done

wait "$relay_pid"
