#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

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
  --manifest-path plugins/status-bar/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release \
  --locked
cargo run --quiet -p brood-wasm --example componentize -- \
  plugins/status-bar/target/wasm32-unknown-unknown/release/brood_status_bar_plugin.wasm \
  plugins/session-switcher/brood_status_bar_plugin.component.wasm
cargo build \
  --manifest-path plugins/inbox/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release \
  --locked
cargo run --quiet -p brood-wasm --example componentize -- \
  plugins/inbox/target/wasm32-unknown-unknown/release/brood_inbox_plugin.wasm \
  plugins/session-switcher/brood_inbox_plugin.component.wasm
cargo build \
  --manifest-path plugins/picker/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release \
  --locked
cargo run --quiet -p brood-wasm --example componentize -- \
  plugins/picker/target/wasm32-unknown-unknown/release/brood_picker_plugin.wasm \
  plugins/session-switcher/brood_picker_plugin.component.wasm
cargo build \
  --manifest-path plugins/keybindings/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release \
  --locked
cargo run --quiet -p brood-wasm --example componentize -- \
  plugins/keybindings/target/wasm32-unknown-unknown/release/brood_keybindings_plugin.wasm \
  plugins/session-switcher/brood_keybindings_plugin.component.wasm
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

  terminate_process_bounded "${CLIENT_PID:-}"
  terminate_process_bounded "${SERVER_PID:-}"
  terminate_process_bounded "${WESTON_PID:-}"

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
  BROOD_KEYBINDINGS_PATH="$TEST_TMP/keybindings.json" \
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
        r"(?:imported window DMA-BUF|uploaded window shared-memory image) "
        r"window_id=(\d+) app_id=\S* resource_id=\d+ size=(\d+)x(\d+)",
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
    and all(
        close_enough(imported.get(window_id), requested_size)
        or any(
            close_enough(imported.get(window_id), configured)
            for configured in negotiated.values()
        )
        for window_id, requested_size in requested.items()
    )
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
        r"(?:imported window DMA-BUF|uploaded window shared-memory image) "
        r"window_id=(\d+) app_id=\S* resource_id=\d+ size=(\d+)x(\d+)",
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

missing = set(requested) - set(imported)
if missing:
    raise SystemExit(f"missing imported frames for resized windows: {sorted(missing)}")

for window_id, requested_size in requested.items():
    accepted_sizes = {requested_size, *negotiated.values()}
    if imported[window_id] not in accepted_sizes:
        raise SystemExit(
            f"window {window_id} did not converge: requested={requested_size} "
            f"configured={sorted(negotiated.values())} imported={imported[window_id]}"
        )

requested_width, requested_height = next(iter(requested.values()))
for configured_width, configured_height in negotiated.values():
    aspect_error = abs(
        configured_width * requested_height - configured_height * requested_width
    )
    if aspect_error >= max(requested_width, requested_height):
        raise SystemExit(
            "a constrained window lost the requested aspect: "
            f"requested={(requested_width, requested_height)} "
            f"configured={(configured_width, configured_height)}"
        )

print(
    f"converged_layout requested={requested} "
    f"configured={sorted(negotiated.values())} imported={imported}"
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
