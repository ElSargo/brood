#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

if [[ -z "${BROOD_RUNTIME_PLUGIN_PACKAGE:-}" ]]; then
  echo "ERR invoke this lane through 'brood test nested' with BROOD_RUNTIME_PLUGIN_ONLY=1" >&2
  exit 2
fi
if [[ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]]; then
  exec dbus-run-session -- "$0" "$@"
fi
for tool in jq python3 rg; do
  command -v "$tool" > /dev/null 2>&1 || {
    echo "ERR $tool is required for the runtime plugin proof" >&2
    exit 2
  }
done

PACKAGE="$(readlink -f "$BROOD_RUNTIME_PLUGIN_PACKAGE")"
OUT="${BROOD_RUNTIME_PLUGIN_OUT:-$(mktemp -d /tmp/brood-runtime-plugins.XXXXXX)}"
WORKSPACE="$OUT/workspace"
CONFIG="$OUT/brood.toml"
SERVER_LOG="$OUT/server.log"
DBUS_DESTINATION="org.brood.RuntimePluginTest.p$$"
mkdir -p "$WORKSPACE/.brood" "$WORKSPACE/src"
printf 'seed\n' > "$WORKSPACE/seed.txt"
printf 'remove\n' > "$WORKSPACE/src/remove-me.txt"

find_component() {
  local name="$1"
  local path
  path="$(find -L "$PACKAGE/plugins/$name" -type f -name '*.component.wasm' -print -quit)"
  [[ -n "$path" ]] || {
    echo "ERR packaged component is missing for $name" >&2
    exit 2
  }
  printf '%s\n' "$path"
}

find_preview1() {
  local name="$1"
  local path
  path="$(find -L "$PACKAGE/plugins/$name" -type f -path '*/wasm32-wasip1-threads/*' -name '*.wasm' -print -quit)"
  [[ -n "$path" ]] || {
    echo "ERR packaged Preview 1 module is missing for $name" >&2
    exit 2
  }
  printf '%s\n' "$path"
}

RUNTIME_COMPONENT="$(find_component component-runtime-io)"
WATCH_COMPONENT="$(find_component component-workspace-watch)"
PREVIEW1="$(find_preview1 workspace-file-log)"
cat > "$CONFIG" <<EOF
[module.component-runtime-io]
enabled = true
kind = "plugin"
path = "$RUNTIME_COMPONENT"
capabilities = ["filesystem:workspace-write", "subprocess:tr", "subprocess:sh", "pty", "network:loopback"]

[module.component-workspace-watch]
enabled = true
kind = "plugin"
path = "$WATCH_COMPONENT"
capabilities = ["filesystem:workspace-write"]
startup-message = "start"

[module.workspace-file-log]
enabled = true
kind = "plugin"
path = "$PREVIEW1"
capabilities = ["filesystem:workspace-write", "subprocess:tr", "subprocess:sh", "pty", "network:loopback"]
startup-message = "start"
EOF

SERVER_PID=""
PEER_PID=""
cleanup() {
  local result=$?
  set +e
  terminate_process_bounded "$PEER_PID"
  terminate_process_bounded "$SERVER_PID"
  if ((result != 0)); then
    echo "server_log=$SERVER_LOG" >&2
    tail -n 200 "$SERVER_LOG" >&2 || true
  fi
  return "$result"
}
trap cleanup EXIT

(
  cd "$WORKSPACE"
  env -u brood_session \
    RUST_LOG="${RUST_LOG:-broodd=info,brood_wasm=info}" \
    "$PACKAGE/bin/brood" run \
      --no-session-runner \
      --headless \
      --config "$CONFIG" \
      --dbus-destination "$DBUS_DESTINATION" \
      --dbus-address "$DBUS_SESSION_BUS_ADDRESS"
) > "$SERVER_LOG" 2>&1 &
SERVER_PID=$!

ctl() {
  env -u brood_session "$PACKAGE/bin/brood" ctl \
    --dbus-destination "$DBUS_DESTINATION" \
    --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
    "$@"
}

plug() {
  env -u brood_session "$PACKAGE/bin/brood" plug \
    --dbus-destination "$DBUS_DESTINATION" \
    --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
    "$@"
}

wait_for_file_text() {
  local path="$1"
  local text="$2"
  for _ in $(seq 1 300); do
    [[ -f "$path" ]] && rg -F -q "$text" "$path" && return
    kill -0 "$SERVER_PID" 2> /dev/null || break
    sleep 0.05
  done
  echo "ERR timed out waiting for '$text' in $path" >&2
  [[ -f "$path" ]] && tail -n 80 "$path" >&2
  return 1
}

for _ in $(seq 1 200); do
  ctl status > /dev/null 2>&1 && break
  kill -0 "$SERVER_PID" 2> /dev/null || {
    echo "ERR runtime plugin server exited during startup" >&2
    exit 1
  }
  sleep 0.05
done
ctl status > /dev/null
for _ in $(seq 1 300); do
  ACTORS="$(ctl actors list 2> /dev/null || true)"
  if rg -q 'id=component-runtime-io ' <<< "$ACTORS" \
    && rg -q 'id=component-workspace-watch ' <<< "$ACTORS" \
    && rg -q 'id=workspace-file-log ' <<< "$ACTORS"; then
    break
  fi
  sleep 0.05
done
rg -q 'id=component-runtime-io ' <<< "$ACTORS"
rg -q 'id=component-workspace-watch ' <<< "$ACTORS"
rg -q 'id=workspace-file-log ' <<< "$ACTORS"
printf '%s\n' "$ACTORS" > "$OUT/actors.txt"

COMPONENT_WATCH="$WORKSPACE/.brood/component-workspace-watch.md"
PREVIEW_LOG="$WORKSPACE/.brood/workspace-file-log.log"
PREVIEW_MIRROR="$WORKSPACE/.brood/workspace-file-log.mirror.log"
wait_for_file_text "$COMPONENT_WATCH" '| rescan | `.` |'
wait_for_file_text "$PREVIEW_LOG" $'initial\tseed.txt'
wait_for_file_text "$PREVIEW_MIRROR" $'initial\tseed.txt'

printf 'created\n' > "$WORKSPACE/created.txt"
printf 'modified\n' >> "$WORKSPACE/seed.txt"
rm "$WORKSPACE/src/remove-me.txt"
python3 - "$WORKSPACE" <<'PY'
import os
import sys

root = os.fsencode(sys.argv[1])
for name in (b"collision-\xff.txt", b"collision-\xfe.txt"):
    descriptor = os.open(os.path.join(root, name), os.O_WRONLY | os.O_CREAT, 0o600)
    os.write(descriptor, b"distinct native path\n")
    os.close(descriptor)
PY

wait_for_file_text "$COMPONENT_WATCH" '| created | `created.txt` |'
wait_for_file_text "$COMPONENT_WATCH" '| modified | `seed.txt` |'
wait_for_file_text "$COMPONENT_WATCH" '| removed | `src/remove-me.txt` |'
wait_for_file_text "$COMPONENT_WATCH" '| created | `collision-\xFF.txt` |'
wait_for_file_text "$COMPONENT_WATCH" '| created | `collision-\xFE.txt` |'
for log in "$PREVIEW_LOG" "$PREVIEW_MIRROR"; do
  wait_for_file_text "$log" $'created\tcreated.txt'
  wait_for_file_text "$log" $'modified\tseed.txt'
  wait_for_file_text "$log" $'removed\tsrc/remove-me.txt'
  wait_for_file_text "$log" $'created\tcollision-%FF.txt'
  wait_for_file_text "$log" $'created\tcollision-%FE.txt'
done
wait_for_file_text "$WORKSPACE/.brood/workspace-file-index.tsv" 'collision-%FF.txt'
wait_for_file_text "$WORKSPACE/.brood/workspace-file-index.tsv" 'collision-%FE.txt'

STATUS_BEFORE="$(ctl status-json)"
printf '%s\n' "$STATUS_BEFORE" > "$OUT/status-before-churn.json"
for _ in $(seq 1 16); do
  ctl actors send workspace-file-log probe-watch-teardown > /dev/null
done
MAILBOX_TARGET="$(jq '.preview1_compat_mailbox_dispatched + 16' <<< "$STATUS_BEFORE")"
for _ in $(seq 1 300); do
  STATUS_AFTER="$(ctl status-json)"
  if jq -e --argjson target "$MAILBOX_TARGET" '
    .preview1_compat_mailbox_dispatched >= $target
    and .preview1_compat_pending_notification_events == 0
    and .preview1_compat_filesystem_watches == 2
    and .filesystem_watcher_registrations == 1
    and .filesystem_watcher_subscriptions == 3
  ' <<< "$STATUS_AFTER" > /dev/null; then
    break
  fi
  sleep 0.05
done
jq -e --argjson target "$MAILBOX_TARGET" '
  .preview1_compat_mailbox_dispatched >= $target
  and .preview1_compat_pending_notification_events == 0
  and .preview1_compat_filesystem_watches == 2
  and .filesystem_watcher_registrations == 1
  and .filesystem_watcher_subscriptions == 3
' <<< "$STATUS_AFTER" > /dev/null
printf '%s\n' "$STATUS_AFTER" > "$OUT/status-after-churn.json"
printf 'still live\n' > "$WORKSPACE/after-watch-churn.txt"
wait_for_file_text "$COMPONENT_WATCH" '| created | `after-watch-churn.txt` |'
wait_for_file_text "$PREVIEW_LOG" $'created\tafter-watch-churn.txt'
wait_for_file_text "$PREVIEW_MIRROR" $'created\tafter-watch-churn.txt'

if [[ "${BROOD_RUNTIME_PLUGIN_WATCH_ONLY:-0}" != 1 ]]; then
  PORT_FILE="$WORKSPACE/.brood/runtime-io-peer.port"
  PEER_LOG="$WORKSPACE/.brood/runtime-io-peer.log"
  python3 - "$PORT_FILE" "$PEER_LOG" <<'PY' &
import socket
import sys

port_file, log_file = sys.argv[1:]
server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
server.bind(("127.0.0.1", 0))
server.listen(2)
with open(port_file, "w", encoding="ascii") as output:
    output.write(str(server.getsockname()[1]))
for _ in range(2):
    connection, _ = server.accept()
    with connection:
        request = connection.recv(4096).decode("utf-8").strip()
        response = f"peer-observed:{request}\n"
        connection.sendall(response.encode("utf-8"))
        with open(log_file, "a", encoding="utf-8") as output:
            output.write(f"request={request}\tresponse={response.strip()}\n")
server.close()
PY
  PEER_PID=$!
  wait_for_file_text "$PORT_FILE" ''
  ADDRESS="127.0.0.1:$(< "$PORT_FILE")"
  ctl actors send component-runtime-io "$ADDRESS" > /dev/null
  ctl actors send workspace-file-log "runtime-io:$ADDRESS" > /dev/null
  COMPONENT_RUNTIME="$WORKSPACE/.brood/runtime-io-transcript.md"
  PREVIEW_RUNTIME="$WORKSPACE/.brood/preview1-runtime-io-transcript.md"
  wait_for_file_text "$COMPONENT_RUNTIME" 'terminal-reply=typed-through-pty'
  wait_for_file_text "$PREVIEW_RUNTIME" 'terminal-reply=preview1-typed-through-pty'
  wait "$PEER_PID"
  PEER_PID=""
  for report in "$COMPONENT_RUNTIME" "$PREVIEW_RUNTIME"; do
    rg -q 'Resolved command: `/' "$report"
    rg -F -q 'Nonzero child exit: `7`' "$report"
    rg -q 'stdin-is-tty stdout-is-tty terminal-prompt>' "$report"
    rg -F -q '1. `20ms` timer expired' "$report"
    rg -F -q '2. `80ms` timer expired' "$report"
  done
  rg -F -q 'Received: `peer-observed:hello-from-plugin\n`' "$COMPONENT_RUNTIME"
  rg -F -q 'Received: `peer-observed:hello-from-preview1\n`' "$PREVIEW_RUNTIME"
  rg -q $'^request=hello-from-plugin\tresponse=peer-observed:hello-from-plugin$' "$PEER_LOG"
  rg -q $'^request=hello-from-preview1\tresponse=peer-observed:hello-from-preview1$' "$PEER_LOG"
fi

FINAL_STATUS="$(ctl status-json)"
printf '%s\n' "$FINAL_STATUS" > "$OUT/status-workload-complete.json"
jq -e '
  .plugin_actor_failures == 0
  and .hosted_runtime_io_resources <= 32
  and .hosted_runtime_io_streams <= 16
  and .hosted_runtime_io_children <= 6
  and .hosted_runtime_io_timers <= 4
  and .hosted_runtime_io_buffered_incoming_bytes == 0
  and .hosted_runtime_io_buffered_outgoing_bytes == 0
' <<< "$FINAL_STATUS" > /dev/null

# Hot replacement must synchronously retire the Component generation's WIT
# watch, notification, and pollable before publishing its successor. The two
# Preview 1 subscriptions remain alive throughout. A manual hot plug does not
# inherit the configured actor's filesystem grant, so the replacement is left
# quiescent rather than manufacturing a different capability contract.
plug --id component-workspace-watch "$WATCH_COMPONENT" > /dev/null
REPLACED_STATUS="$(ctl status-json)"
jq -e '
  .plugin_actor_failures == 0
  and .filesystem_watcher_registrations == 1
  and .filesystem_watcher_subscriptions == 2
' <<< "$REPLACED_STATUS" > /dev/null
printf '%s\n' "$REPLACED_STATUS" > "$OUT/status-component-replaced.json"

cat > "$OUT/proof.md" <<EOF
# Runtime plugin proof

- Component filesystem history: $COMPONENT_WATCH
- Preview 1 primary history: $PREVIEW_LOG
- Preview 1 mirror history: $PREVIEW_MIRROR
- Resource state before churn: $OUT/status-before-churn.json
- Resource state after churn: $OUT/status-after-churn.json
- Resource state after worker completion: $OUT/status-workload-complete.json
- Component watch replacement and synchronous old-generation teardown: $OUT/status-component-replaced.json
- Synthetic mutations include create, modify, remove, distinct non-UTF-8 names, and post-teardown-churn create.
- Runtime I/O transcripts and the independent peer log are retained when the runtime-io lane is enabled.
EOF

echo "runtime_plugins_nested=verified out=$OUT"
echo "proof=$OUT/proof.md"
