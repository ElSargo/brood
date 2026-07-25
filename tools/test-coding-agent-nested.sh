#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]]; then
  exec dbus-run-session -- "$0" "$@"
fi

for tool in cargo grim magick pkg-config sway wasm-tools wtype; do
  if ! command -v "$tool" > /dev/null 2>&1; then
    echo "ERR $tool is required for the coding-agent proof" >&2
    exit 2
  fi
done

OUT="${MANIFOLD_CODING_AGENT_OUT:-$(mktemp -d /tmp/manifold-coding-agent.XXXXXX)}"
PACKAGE="$OUT/package"
WORKSPACE="$OUT/workspace"
RUNTIME_DIR="$OUT/runtime"
COMPOSITOR_LOG="$OUT/compositor.log"
SERVER_LOG="$OUT/server.log"
CLIENT_LOG="$OUT/client.log"
CONFIG="$OUT/manifold.toml"
SWAY_CONFIG="$OUT/sway.conf"
DBUS_DESTINATION="org.manifold.CodingAgentTest.p$$"
mkdir -p "$PACKAGE" "$WORKSPACE/.manifold" "$RUNTIME_DIR"
chmod 700 "$RUNTIME_DIR"
git -C "$WORKSPACE" init -q

CLIENT_PID=""
SERVER_PID=""
COMPOSITOR_PID=""
cleanup() {
  local result=$?
  set +e
  for pid in "$CLIENT_PID" "$SERVER_PID" "$COMPOSITOR_PID"; do
    if [[ -n "$pid" ]] && kill -0 "$pid" 2> /dev/null; then
      kill "$pid" > /dev/null 2>&1
      wait "$pid" > /dev/null 2>&1
    fi
  done
  if ((result != 0)); then
    echo "client_log=$CLIENT_LOG" >&2
    tail -n 240 "$CLIENT_LOG" >&2
    echo "server_log=$SERVER_LOG" >&2
    tail -n 180 "$SERVER_LOG" >&2
    echo "compositor_log=$COMPOSITOR_LOG" >&2
    tail -n 100 "$COMPOSITOR_LOG" >&2
  fi
  return "$result"
}
trap cleanup EXIT

cargo build -p manifoldd -p manifold-client --bins
cargo build \
  --manifest-path plugins/coding-agent/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release \
  --locked
wasm-tools component new \
  plugins/coding-agent/target/wasm32-unknown-unknown/release/manifold_coding_agent_plugin.wasm \
  --output "$PACKAGE/manifold_coding_agent_plugin.component.wasm"
wasm-tools validate "$PACKAGE/manifold_coding_agent_plugin.component.wasm"
cargo build \
  --manifest-path plugins/default-workspace/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release \
  --locked
target/debug/manifold-client \
  --no-server \
  --wasm-key-policy workspace \
  "$ROOT/plugins/default-workspace/target/wasm32-unknown-unknown/release/manifold_default_workspace_plugin.wasm" \
  --check-wasm-key-policy

cat > "$CONFIG" <<EOF
[init]
session = "$PACKAGE/manifold_coding_agent_plugin.component.wasm"
client = "$PACKAGE/manifold_coding_agent_plugin.component.wasm"

[client.key-policy]
provider = "workspace"
path = "$ROOT/plugins/default-workspace/target/wasm32-unknown-unknown/release/manifold_default_workspace_plugin.wasm"
EOF

RUST_LOG="${RUST_LOG:-manifoldd=info,manifold_client=info,smithay=warn,wgpu_core=warn,wgpu_hal=warn}"
(
  cd "$WORKSPACE"
  env -u manifold_session \
    RUST_LOG="$RUST_LOG" \
    "$ROOT/target/debug/manifold-sessiond" \
    --headless \
    --config "$CONFIG" \
    --dbus-destination "$DBUS_DESTINATION" \
    --dbus-address "$DBUS_SESSION_BUS_ADDRESS"
) > "$SERVER_LOG" 2>&1 &
SERVER_PID=$!

ctl() {
  env -u manifold_session target/debug/manifold ctl \
    --dbus-destination "$DBUS_DESTINATION" \
    --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
    "$@"
}

for _ in $(seq 1 150); do
  ctl status > /dev/null 2>&1 && break
  kill -0 "$SERVER_PID" 2> /dev/null || {
    echo "ERR session server exited during startup" >&2
    exit 1
  }
  sleep 0.1
done
ctl status > /dev/null

cat > "$SWAY_CONFIG" <<'EOF'
xwayland disable
output HEADLESS-1 mode 1280x720
seat seat0 fallback true
default_border none
default_floating_border none
EOF

env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WLR_BACKENDS=headless \
  WLR_HEADLESS_OUTPUTS=1 \
  WLR_LIBINPUT_NO_DEVICES=1 \
  sway \
  --unsupported-gpu \
  --config "$SWAY_CONFIG" \
  > "$COMPOSITOR_LOG" 2>&1 &
COMPOSITOR_PID=$!

WAYLAND_SOCKET=""
for _ in $(seq 1 100); do
  WAYLAND_SOCKET="$(
    find "$RUNTIME_DIR" -maxdepth 1 -type s -name 'wayland-*' -printf '%f\n' | head -1
  )"
  [[ -n "$WAYLAND_SOCKET" ]] && break
  sleep 0.05
done
[[ -n "$WAYLAND_SOCKET" ]]

env -u manifold_session \
  XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  WGPU_BACKEND=vulkan \
  MANIFOLD_DMABUF_HAPPY_PATH=1 \
  MANIFOLD_TRACE_PLUGIN_INPUT=1 \
  RUST_LOG="$RUST_LOG" \
  target/debug/manifold-client \
  --config "$CONFIG" \
  --dbus-destination "$DBUS_DESTINATION" \
  --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
  > "$CLIENT_LOG" 2>&1 &
CLIENT_PID=$!

for _ in $(seq 1 300); do
  if rg -q "accepted plugin draw packet" "$CLIENT_LOG" \
    && rg -q "delivered presentation-to-session actor message" "$SERVER_LOG" \
    && rg -q "coding-agent-1" "$SERVER_LOG"; then
    break
  fi
  kill -0 "$CLIENT_PID" 2> /dev/null || {
    echo "ERR presentation client exited before the coding-agent UI rendered" >&2
    exit 1
  }
  sleep 0.1
done
rg -q "accepted plugin draw packet" "$CLIENT_LOG"
rg -q "delivered presentation-to-session actor message" "$SERVER_LOG"
rg -q "coding-agent-1" "$SERVER_LOG"

# wtype uses a compact generated keymap. Prime 29 distinct symbols so its
# virtual A lands on the normal evdev/XKB keycode 38 used by the key policy.
KEYMAP_PRIMER="bcdefghijklmnopqrstuvwxyz0123"
env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  wtype "$KEYMAP_PRIMER" -s 100 -M ctrl -M alt -k a -m alt -m ctrl

for _ in $(seq 1 300); do
  if rg -q "sent key-policy actor message actor=manifold.client-init" "$CLIENT_LOG" \
    && rg -q "started presentation WASM actor id=coding-agent-ui-2" "$CLIENT_LOG" \
    && rg -q 'presentation actor message sender=coding-agent-ui-2 recipient=init.*"kind":"registry-subscribe"' \
      "$CLIENT_LOG" \
    && rg -q "presentation key policy requested pane focus.*focused=Some.*changed=true" \
      "$CLIENT_LOG"; then
    break
  fi
  kill -0 "$CLIENT_PID" 2> /dev/null || {
    echo "ERR presentation client exited while launching another agent UI" >&2
    exit 1
  }
  sleep 0.1
done
rg -q "sent key-policy actor message actor=manifold.client-init" "$CLIENT_LOG"
rg -q "started presentation WASM actor id=coding-agent-ui-2" "$CLIENT_LOG"
rg -q 'presentation actor message sender=coding-agent-ui-2 recipient=init.*"kind":"registry-subscribe"' \
  "$CLIENT_LOG"
rg -q "presentation key policy requested pane focus.*focused=Some.*changed=true" "$CLIENT_LOG"

env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  wtype "x" -k BackSpace -s 100 -k F2

for _ in $(seq 1 300); do
  if rg -q "started component plugin actor actor=manifold.session.1.coding-agent-2" "$SERVER_LOG" \
    && rg -q 'session actor message.*"agent":"coding-agent-2"' "$CLIENT_LOG"; then
    break
  fi
  kill -0 "$CLIENT_PID" 2> /dev/null || {
    echo "ERR presentation client exited while creating the second agent" >&2
    exit 1
  }
  sleep 0.1
done
rg -q "started component plugin actor actor=manifold.session.1.coding-agent-2" "$SERVER_LOG"
rg -q 'session actor message.*"agent":"coding-agent-2"' "$CLIENT_LOG"
rg -q 'presentation actor message sender=coding-agent-ui-2 recipient=init.*"kind":"create-agent"' \
  "$CLIENT_LOG"

capture() {
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
    WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    grim "$1"
  [[ -s "$1" ]]
}

capture "$OUT/agent-idle.png"

PROMPT='create .manifold/agent-actor-proof.txt containing exactly: actor-ui-worker-ok'
env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  wtype "x" -k BackSpace -k Tab -s 100 "$PROMPT" -s 100 -k Return

for _ in $(seq 1 300); do
  if rg -q 'presentation actor message.*recipient=coding-agent-2.*"kind":"prompt"' "$CLIENT_LOG"; then
    break
  fi
  sleep 0.1
done
rg -q 'presentation actor message.*recipient=coding-agent-2.*"kind":"prompt"' "$CLIENT_LOG"

for _ in $(seq 1 900); do
  if [[ -f "$WORKSPACE/.manifold/agent-actor-proof.txt" ]] \
    && rg -qx "actor-ui-worker-ok" "$WORKSPACE/.manifold/agent-actor-proof.txt"; then
    break
  fi
  kill -0 "$CLIENT_PID" 2> /dev/null || {
    echo "ERR presentation client exited during the agent turn" >&2
    exit 1
  }
  sleep 0.2
done
rg -qx "actor-ui-worker-ok" "$WORKSPACE/.manifold/agent-actor-proof.txt"

for _ in $(seq 1 600); do
  idle_updates="$(
    rg -c 'session actor message.*"status":"idle"' "$CLIENT_LOG" || true
  )"
  if rg -q "component actor worker finished" "$SERVER_LOG" \
    && ((idle_updates >= 3)); then
    break
  fi
  kill -0 "$CLIENT_PID" 2> /dev/null || {
    echo "ERR presentation client exited before the agent turn completed" >&2
    exit 1
  }
  sleep 0.2
done
rg -q "component actor worker finished" "$SERVER_LOG"
idle_updates="$(rg -c 'session actor message.*"status":"idle"' "$CLIENT_LOG" || true)"
((idle_updates >= 3))

env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  wtype "x" -k BackSpace -s 100 -k F6
for _ in $(seq 1 300); do
  if rg -q 'presentation actor message sender=coding-agent-ui-2 recipient=coding-agent-2.*"kind":"configure"' \
    "$CLIENT_LOG"; then
    break
  fi
  sleep 0.1
done
rg -q 'presentation actor message sender=coding-agent-ui-2 recipient=coding-agent-2.*"kind":"configure"' \
  "$CLIENT_LOG"
if rg -q "presentation init child actor failed|component actor mailbox rejected message: channel closed" \
  "$CLIENT_LOG"; then
  echo "ERR coding-agent UI actor failed during the workload" >&2
  exit 1
fi

sleep 3
capture "$OUT/agent-complete.png"

python3 - "$OUT/agent-idle.png" "$OUT/agent-complete.png" <<'PY'
import pathlib
import subprocess
import sys

for value in sys.argv[1:]:
    path = pathlib.Path(value)
    data = path.read_bytes()
    if not data.startswith(b"\x89PNG\r\n\x1a\n") or len(data) < 4096:
        raise SystemExit(f"invalid graphical proof: {path}")

metric = subprocess.run(
    ["magick", "compare", "-metric", "RMSE", sys.argv[1], sys.argv[2], "null:"],
    text=True,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    check=False,
).stderr
normalized = float(metric.rsplit("(", 1)[1].rstrip(")\n"))
if normalized < 0.005:
    raise SystemExit(f"idle and completed agent captures are too similar: RMSE={normalized}")
print(f"agent_capture_rmse={normalized:.6f}")
PY

STATUS="$(ctl status)"
rg -q "presentation=Attached" <<< "$STATUS"
rg -q "session-to-presentation" "$SERVER_LOG"

cat > "$OUT/proof.md" <<EOF
# Coding agent actor proof

- input: real text and Enter events through a headless Sway presentation
- UI: Ctrl+Alt+A launched and focused a second client-realm canvas actor
- agents: two independently addressed session-realm actors, enumerated and switched through the UI
- work: a private actor worker launched Codex with workspace-write authority
- result: .manifold/agent-actor-proof.txt contains actor-ui-worker-ok
- survival: the focused UI accepted a model change after the completed turn
- captures: agent-idle.png and agent-complete.png
- status: $STATUS
EOF

ctl quit > /dev/null 2>&1 || true
echo "coding_agent_actor=verified"
echo "proof=$OUT/proof.md"
echo "idle=$OUT/agent-idle.png"
echo "complete=$OUT/agent-complete.png"
