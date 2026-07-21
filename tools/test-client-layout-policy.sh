#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]]; then
  exec dbus-run-session -- "$0" "$@"
fi

for tool in weston weston-simple-egl python3; do
  if ! command -v "$tool" > /dev/null 2>&1; then
    echo "ERR $tool is required for the client layout-policy proof" >&2
    exit 2
  fi
done

cargo build -p manifold-client -p manifoldd --bins
cargo build \
  --manifest-path plugins/default-workspace/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release

TEST_TMP="$(mktemp -d /tmp/manifold-client-layout-policy.XXXXXX)"
RUNTIME_DIR="$TEST_TMP/runtime"
WESTON_LOG="$TEST_TMP/weston.log"
SERVER_LOG="$TEST_TMP/server.log"
CLIENT_LOG="$TEST_TMP/client.log"
DBUS_DESTINATION="org.manifold.LayoutPolicyTest.p$$"
WESTON_SOCKET="manifold-layout-policy-$$"
POLICY="$ROOT/plugins/default-workspace/target/wasm32-unknown-unknown/release/manifold_default_workspace_plugin.wasm"
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

RUST_LOG="${RUST_LOG:-manifoldd=info,manifold_client=info,smithay=warn,wgpu_core=warn,wgpu_hal=warn}"
env RUST_LOG="$RUST_LOG" \
  target/debug/manifold-sessiond \
  --offscreen-presentation \
  --no-default-modules \
  --dbus-destination "$DBUS_DESTINATION" \
  --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
  > "$SERVER_LOG" 2>&1 &
SERVER_PID=$!

ctl() {
  target/debug/manifold ctl \
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
  MANIFOLD_DMABUF_HAPPY_PATH=1 \
  RUST_LOG="$RUST_LOG" \
  target/debug/manifold-client \
  --dbus-destination "$DBUS_DESTINATION" \
  --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
  --wasm-layout-policy tile "$POLICY" \
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

ctl spawn weston-simple-egl > /dev/null
ctl spawn weston-simple-egl > /dev/null

layout_converged() {
  python3 - "$CLIENT_LOG" <<'PY'
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
        r"imported window DMA-BUF window_id=(\d+) resource_id=\d+ size=(\d+)x(\d+)",
        line,
    )
    if frame:
        imported[int(frame.group(1))] = (int(frame.group(2)), int(frame.group(3)))

sys.exit(
    0
    if len(requested) >= 2
    and all(imported.get(window_id) == size for window_id, size in requested.items())
    else 1
)
PY
}

for _ in $(seq 1 200); do
  if rg -q "presentation layout source changed source=wasm-policy" "$CLIENT_LOG" \
    && layout_converged; then
    break
  fi
  kill -0 "$CLIENT_PID" 2> /dev/null || {
    echo "ERR presentation client exited before layout convergence" >&2
    exit 1
  }
  sleep 0.1
done

if ! rg -q "presentation layout source changed source=wasm-policy" "$CLIENT_LOG"; then
  echo "ERR presentation client did not accept the WASM layout policy" >&2
  exit 1
fi
if ! layout_converged; then
  echo "ERR managed clients did not converge on the requested layout sizes" >&2
  exit 1
fi

python3 - "$CLIENT_LOG" <<'PY'
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
        r"imported window DMA-BUF window_id=(\d+) resource_id=\d+ size=(\d+)x(\d+)",
        line,
    )
    if frame:
        imported[int(frame.group(1))] = (int(frame.group(2)), int(frame.group(3)))

if len(requested) < 2:
    raise SystemExit(f"expected two client resize requests, got {requested}")

latest = dict(sorted(requested.items())[-2:])
missing = set(latest) - set(imported)
if missing:
    raise SystemExit(f"missing imported frames for resized windows: {sorted(missing)}")

for window_id, expected in latest.items():
    if imported[window_id] != expected:
        raise SystemExit(
            f"window {window_id} did not converge: requested={expected} imported={imported[window_id]}"
        )

sizes = set(latest.values())
if len(sizes) != 1:
    raise SystemExit(f"tile provider did not allocate equal pane sizes: {latest}")

print(f"converged_layout={latest}")
PY

STATUS="$(ctl status)"
[[ "$STATUS" =~ windows=2([[:space:]]|$) ]] || {
  echo "ERR expected two managed clients: $STATUS" >&2
  exit 1
}

echo "OK client-local WASM layout policy proof passed"
echo "status=$STATUS"
echo "policy=$POLICY"

ctl client close > /dev/null 2>&1 || true
ctl quit > /dev/null 2>&1 || true
trap - EXIT
cleanup
