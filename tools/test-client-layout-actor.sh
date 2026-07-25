#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]]; then
  exec dbus-run-session -- "$0" "$@"
fi

for tool in weston weston-simple-egl gtk4-demo python3; do
  if ! command -v "$tool" > /dev/null 2>&1; then
    echo "ERR $tool is required for the client layout-actor proof" >&2
    exit 2
  fi
done

cargo build -p brood-client -p broodd --bins
cargo build \
  --manifest-path plugins/default-workspace/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release
cargo build \
  --manifest-path plugins/session-switcher/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release
cargo run --quiet -p brood-wasm --example componentize -- \
  plugins/session-switcher/target/wasm32-unknown-unknown/release/brood_session_switcher_plugin.wasm \
  plugins/session-switcher/brood_session_switcher_plugin.component.wasm
cargo build \
  --manifest-path plugins/coding-agent/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release \
  --locked
cargo run --quiet -p brood-wasm --example componentize -- \
  plugins/coding-agent/target/wasm32-unknown-unknown/release/brood_coding_agent_plugin.wasm \
  plugins/session-switcher/brood_coding_agent_plugin.component.wasm

target/debug/brood-client \
  --no-server \
  --wasm-key-policy workspace "$ROOT/plugins/default-workspace/target/wasm32-unknown-unknown/release/brood_default_workspace_plugin.wasm" \
  --check-wasm-key-policy

TEST_TMP="$(mktemp -d /tmp/brood-client-layout-actor.XXXXXX)"
RUNTIME_DIR="$TEST_TMP/runtime"
WESTON_LOG="$TEST_TMP/weston.log"
SERVER_LOG="$TEST_TMP/server.log"
CLIENT_LOG="$TEST_TMP/client.log"
DBUS_DESTINATION="org.brood.LayoutActorTest.p$$"
WESTON_SOCKET="brood-layout-actor-$$"
WORKSPACE="$ROOT/plugins/default-workspace/target/wasm32-unknown-unknown/release/brood_default_workspace_plugin.wasm"
mkdir -m 700 "$RUNTIME_DIR"

cleanup() {
  local result=$?

  for pid in "${CLIENT_PID:-}" "${SERVER_PID:-}" "${WESTON_PID:-}"; do
    if [[ -n "$pid" ]] && kill -0 "$pid" 2> /dev/null; then
      kill "$pid" > /dev/null 2>&1 || true
    fi
  done

  if ((result != 0)); then
    echo "client_log=$CLIENT_LOG" >&2
    tail -n 160 "$CLIENT_LOG" >&2 || true
    echo "server_log=$SERVER_LOG" >&2
    tail -n 160 "$SERVER_LOG" >&2 || true
    echo "weston_log=$WESTON_LOG" >&2
    tail -n 80 "$WESTON_LOG" >&2 || true
  fi

  rm -rf -- "$TEST_TMP"
  return "$result"
}
trap cleanup EXIT

env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  weston \
  --backend=headless \
  --renderer=gl \
  --width=1000 \
  --height=700 \
  --socket="$WESTON_SOCKET" \
  --no-config \
  --log="$WESTON_LOG" \
  > /dev/null 2>&1 &
WESTON_PID=$!

for _ in $(seq 1 100); do
  [[ -S "$RUNTIME_DIR/$WESTON_SOCKET" ]] && break
  kill -0 "$WESTON_PID" 2> /dev/null || {
    echo "ERR headless Weston exited during startup" >&2
    exit 1
  }
  sleep 0.1
done
[[ -S "$RUNTIME_DIR/$WESTON_SOCKET" ]] || {
  echo "ERR timed out waiting for the headless Weston socket" >&2
  exit 1
}

RUST_LOG="${RUST_LOG:-broodd=info,brood_client=info,smithay=warn,wgpu_core=warn,wgpu_hal=warn}"
env RUST_LOG="$RUST_LOG" \
  target/debug/brood-sessiond \
  --headless \
  --no-init \
  --no-default-modules \
  --dbus-destination "$DBUS_DESTINATION" \
  --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
  > "$SERVER_LOG" 2>&1 &
SERVER_PID=$!

ctl() {
  target/debug/brood ctl \
    --dbus-destination "$DBUS_DESTINATION" \
    --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
    "$@"
}

for _ in $(seq 1 100); do
  ctl status > /dev/null 2>&1 && break
  kill -0 "$SERVER_PID" 2> /dev/null || {
    echo "ERR session server exited during startup" >&2
    exit 1
  }
  sleep 0.1
done
ctl status > /dev/null 2>&1 || {
  echo "ERR timed out waiting for the session server" >&2
  exit 1
}

env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WESTON_SOCKET" \
  WGPU_BACKEND=vulkan \
  BROOD_DMABUF_HAPPY_PATH=1 \
  RUST_LOG="$RUST_LOG" \
  target/debug/brood-client \
  --dbus-destination "$DBUS_DESTINATION" \
  --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
  --init-actor "$ROOT/plugins/session-switcher/brood_session_switcher_plugin.component.wasm" \
  --wasm-actor "workspace=$WORKSPACE" \
  --wasm-key-policy workspace "$WORKSPACE" \
  > "$CLIENT_LOG" 2>&1 &
CLIENT_PID=$!

for _ in $(seq 1 150); do
  rg -q "configured presentation surface" "$CLIENT_LOG" && break
  kill -0 "$CLIENT_PID" 2> /dev/null || {
    echo "ERR presentation client exited during startup" >&2
    exit 1
  }
  sleep 0.1
done
rg -q "configured presentation surface" "$CLIENT_LOG" || {
  echo "ERR timed out waiting for the presentation client" >&2
  exit 1
}

ctl spawn env GTK_A11Y=none gtk4-demo > /dev/null
ctl spawn weston-simple-egl > /dev/null

layout_converged() {
  python3 - "$CLIENT_LOG" "$SERVER_LOG" <<'PY'
import re
import sys

requested = {}
imported = {}
for line in open(sys.argv[1], encoding="utf-8"):
    request = re.search(
        r"requested managed window resize window_id=(\d+) logical_size=(\d+)x(\d+)",
        line,
    )
    if request:
        requested[int(request.group(1))] = (int(request.group(2)), int(request.group(3)))
    frame = re.search(
        r"imported window DMA-BUF window_id=(\d+) app_id=\S* resource_id=\d+ size=(\d+)x(\d+)",
        line,
    )
    if frame:
        imported[int(frame.group(1))] = (int(frame.group(2)), int(frame.group(3)))

negotiated = {}
for line in open(sys.argv[2], encoding="utf-8"):
    resize = re.search(
        r"negotiated presentation-client pane size pane=(\d+).*"
        r"configured_width=(\d+) configured_height=(\d+)",
        line,
    )
    if resize:
        negotiated[int(resize.group(1))] = (int(resize.group(2)), int(resize.group(3)))

expected = {
    window_id: negotiated.get(window_id, size)
    for window_id, size in requested.items()
}

def close_enough(actual, target):
    return (
        actual is not None
        # Terminal buffers may quantize to their cell grid and client-side
        # decoration extent. GTK's explicit negotiation is still checked
        # exactly by the final proof below.
        and abs(actual[0] - target[0]) <= 16
        and abs(actual[1] - target[1]) <= 16
    )

sys.exit(
    0
    if len(requested) >= 2
    and negotiated
    and all(close_enough(imported.get(window_id), size) for window_id, size in expected.items())
    else 1
)
PY
}

for _ in $(seq 1 200); do
  if rg -q "presentation layout source changed source=actor" "$CLIENT_LOG" \
    && layout_converged; then
    break
  fi
  kill -0 "$CLIENT_PID" 2> /dev/null || {
    echo "ERR presentation client exited before layout convergence" >&2
    exit 1
  }
  sleep 0.1
done

if ! rg -q "presentation layout source changed source=actor" "$CLIENT_LOG"; then
  echo "ERR presentation client did not accept the actor layout" >&2
  exit 1
fi
if ! rg -q "granted root layout lease actor=workspace" "$CLIENT_LOG"; then
  echo "ERR presentation client did not grant the workspace actor a root layout lease" >&2
  exit 1
fi
if ! rg -q "granted claimed root layout lease actor=session-switcher" "$CLIENT_LOG"; then
  echo "ERR session switcher did not claim the root layout lease" >&2
  exit 1
fi
if ! rg -q "granted rest layout lease .* actor=workspace" "$CLIENT_LOG"; then
  echo "ERR session switcher did not delegate its rest slot to the workspace" >&2
  exit 1
fi
if ! rg -q "accepted leased actor layout" "$CLIENT_LOG"; then
  echo "ERR presentation client did not accept a lease-authorized layout result" >&2
  exit 1
fi
if ! layout_converged; then
  echo "ERR managed clients did not converge on their accepted layout sizes" >&2
  exit 1
fi

python3 - "$CLIENT_LOG" "$SERVER_LOG" <<'PY'
import re
import sys

requested = {}
imported = {}
for line in open(sys.argv[1], encoding="utf-8"):
    request = re.search(
        r"requested managed window resize window_id=(\d+) logical_size=(\d+)x(\d+)",
        line,
    )
    if request:
        requested[int(request.group(1))] = (int(request.group(2)), int(request.group(3)))

    frame = re.search(
        r"imported window DMA-BUF window_id=(\d+) app_id=\S* resource_id=\d+ size=(\d+)x(\d+)",
        line,
    )
    if frame:
        imported[int(frame.group(1))] = (int(frame.group(2)), int(frame.group(3)))

negotiated = {}
for line in open(sys.argv[2], encoding="utf-8"):
    resize = re.search(
        r"negotiated presentation-client pane size pane=(\d+).*"
        r"configured_width=(\d+) configured_height=(\d+)",
        line,
    )
    if resize:
        negotiated[int(resize.group(1))] = (int(resize.group(2)), int(resize.group(3)))

if len(requested) < 2:
    raise SystemExit(f"expected two client resize requests, got {requested}")
if not negotiated:
    raise SystemExit("expected a constrained real client to negotiate its pane size")

latest = dict(sorted(requested.items())[-2:])
missing = set(latest) - set(imported)
if missing:
    raise SystemExit(f"missing imported frames for resized windows: {sorted(missing)}")

expected_sizes = {
    window_id: negotiated.get(window_id, requested_size)
    for window_id, requested_size in latest.items()
}
for window_id, expected in expected_sizes.items():
    if imported[window_id] != expected:
        raise SystemExit(
            f"window {window_id} did not converge: requested={latest[window_id]} "
            f"configured={expected} imported={imported[window_id]}"
        )

for window_id, configured in negotiated.items():
    if window_id not in latest:
        continue
    requested_width, requested_height = latest[window_id]
    configured_width, configured_height = configured
    aspect_error = abs(
        configured_width * requested_height - configured_height * requested_width
    )
    if aspect_error >= max(requested_width, requested_height):
        raise SystemExit(
            f"window {window_id} lost requested aspect: "
            f"requested={latest[window_id]} configured={configured}"
        )

print(
    f"converged_layout requested={latest} configured={expected_sizes} imported={imported}"
)
PY

STATUS="$(ctl status)"
WINDOWS="$(sed -n 's/.* windows=\([0-9][0-9]*\) .*/\1/p' <<<"$STATUS")"
if [[ -z "$WINDOWS" ]] || ((WINDOWS < 2)); then
  echo "ERR expected at least the two explicitly spawned managed clients: $STATUS" >&2
  exit 1
fi

rg -q "loaded presentation key policy provider=workspace" "$CLIENT_LOG"

echo "OK client-local layout actor, constrained resize, and key decision proof passed"
echo "status=$STATUS"
echo "workspace_actor=$WORKSPACE"

ctl client close > /dev/null 2>&1 || true
ctl quit > /dev/null 2>&1 || true
trap - EXIT
cleanup
