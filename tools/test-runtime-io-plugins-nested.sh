#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug/manifold"

ctl() {
  RUST_LOG=off timeout 3s "$BIN" ctl "$@"
}

wait_for_file() {
  local path="$1"
  for _ in $(seq 1 300); do
    [[ -s "$path" ]] && return 0
    sleep 0.05
  done
  echo "ERR timed out waiting for $path" >&2
  ctl status-json >&2 || true
  return 1
}

run_controller() {
  local workspace="$1"
  local port_file="$workspace/.manifold/runtime-io-peer.port"
  local peer_log="$workspace/.manifold/runtime-io-peer.log"
  mkdir -p "$workspace/.manifold"
  python3 - "$port_file" "$peer_log" <<'PY' &
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
    connection, address = server.accept()
    with connection:
        chunks = []
        while True:
            chunk = connection.recv(4096)
            if not chunk:
                break
            chunks.append(chunk)
        request = b"".join(chunks).decode("utf-8").strip()
        response = f"peer-observed:{request}\n"
        connection.sendall(response.encode("utf-8"))
        with open(log_file, "a", encoding="utf-8") as output:
            output.write(f"request={request}\tresponse={response.strip()}\n")
server.close()
PY
  peer_pid=$!
  wait_for_file "$port_file"
  address="127.0.0.1:$(cat "$port_file")"

  ctl actors send component-runtime-io "$address" >/dev/null
  ctl actors send preview1-runtime-io "runtime-io:$address" >/dev/null

  component_report="$workspace/.manifold/runtime-io-transcript.md"
  preview_report="$workspace/.manifold/preview1-runtime-io-transcript.md"
  wait_for_file "$component_report"
  wait_for_file "$preview_report"
  wait "$peer_pid"

  grep -q 'Resolved command: `/' "$component_report"
  grep -Fq 'Stdin: `alpha beta\n`' "$component_report"
  grep -Fq 'Stdout: `ALPHA BETA\n`' "$component_report"
  grep -q 'Nonzero child exit: `7`' "$component_report"
  grep -q 'stdin-is-tty stdout-is-tty terminal-prompt>' "$component_report"
  grep -q 'terminal-reply=typed-through-pty' "$component_report"
  grep -Fq 'Received: `peer-observed:hello-from-plugin\n`' "$component_report"
  grep -q '1. `20ms` timer expired' "$component_report"
  grep -q '2. `80ms` timer expired' "$component_report"

  grep -q 'Resolved command: `/' "$preview_report"
  grep -Fq 'Stdin: `preview one\n`' "$preview_report"
  grep -Fq 'Stdout: `PREVIEW ONE\n`' "$preview_report"
  grep -q 'Nonzero child exit: `7`' "$preview_report"
  grep -q 'stdin-is-tty stdout-is-tty terminal-prompt>' "$preview_report"
  grep -q 'terminal-reply=preview1-typed-through-pty' "$preview_report"
  grep -Fq 'Received: `peer-observed:hello-from-preview1\n`' "$preview_report"
  grep -q '1. `20ms` timer expired' "$preview_report"
  grep -q '2. `80ms` timer expired' "$preview_report"

  grep -q $'^request=hello-from-plugin\tresponse=peer-observed:hello-from-plugin$' "$peer_log"
  grep -q $'^request=hello-from-preview1\tresponse=peer-observed:hello-from-preview1$' "$peer_log"
  echo "runtime_io_controller=ok component=$component_report preview1=$preview_report peer=$peer_log"
}

if [[ "${1:-}" == "--controller" ]]; then
  [[ $# -eq 2 ]]
  run_controller "$2"
  exit
fi

cd "$ROOT"
nix develop --command cargo build --quiet -p manifoldd --bins
(
  cd plugins/component-runtime-io
  nix develop "$ROOT" --command cargo build --quiet --release --locked --target wasm32-unknown-unknown
  nix develop "$ROOT" --command wasm-tools component new target/wasm32-unknown-unknown/release/manifold_component_runtime_io_plugin.wasm \
    --output target/wasm32-unknown-unknown/release/manifold_component_runtime_io_plugin.component.wasm
  nix develop "$ROOT" --command wasm-tools validate target/wasm32-unknown-unknown/release/manifold_component_runtime_io_plugin.component.wasm
)
(
  cd plugins/workspace-file-log
  nix develop "$ROOT" --command cargo build --quiet --release --locked
)

OUT="${MANIFOLD_RUNTIME_IO_OUT:-$(mktemp -d /tmp/manifold-runtime-io.XXXXXX)}"
WORKSPACE="$OUT/workspace"
mkdir -p "$WORKSPACE"
COMPONENT="$ROOT/plugins/component-runtime-io/target/wasm32-unknown-unknown/release/manifold_component_runtime_io_plugin.component.wasm"
PREVIEW="$ROOT/plugins/workspace-file-log/target/wasm32-wasip1-threads/release/manifold_workspace_file_log_plugin.wasm"
CONFIG="$OUT/manifold.toml"
printf '%s\n' \
  '[module.component-runtime-io]' \
  'enabled = true' \
  'kind = "plugin"' \
  "path = \"$COMPONENT\"" \
  'capabilities = ["filesystem:workspace-write", "subprocess:tr", "subprocess:sh", "pty", "network:loopback"]' \
  '' \
  '[module.preview1-runtime-io]' \
  'enabled = true' \
  'kind = "plugin"' \
  "path = \"$PREVIEW\"" \
  'capabilities = ["filesystem:workspace-write", "subprocess:tr", "subprocess:sh", "pty", "network:loopback"]' > "$CONFIG"

TEST_LOG="$OUT/test.log"
(
  cd "$WORKSPACE"
  RUST_LOG="${RUST_LOG:-manifoldd=info,manifold_wasm=info}" \
  timeout --kill-after=5s 120s nix develop "$ROOT" --command "$BIN" test nested \
    --force-outer \
    --out-dir "$OUT/nested" \
    --inner-config "$CONFIG" \
    --inner-command "$ROOT/tools/test-runtime-io-plugins-nested.sh --controller $WORKSPACE"
) | tee "$TEST_LOG"

grep -qx 'topology=outer-supervisor' "$TEST_LOG"
grep -qx 'outer_presentation=offscreen' "$TEST_LOG"
grep -q 'runtime_io_controller=ok' "$OUT/nested/inner.log"
echo "runtime_io_nested=ok artifacts=$OUT"
