#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

cargo build -p manifoldd --bins >/dev/null
cargo build --manifest-path examples/wasm-key-handler/Cargo.toml --target wasm32-unknown-unknown --release >/dev/null
cargo build --manifest-path examples/wasm-map-hook/Cargo.toml --target wasm32-unknown-unknown --release >/dev/null

KEY_WASM="$ROOT/examples/wasm-key-handler/target/wasm32-unknown-unknown/release/manifold_wasm_key_handler.wasm"
MAP_WASM="$ROOT/examples/wasm-map-hook/target/wasm32-unknown-unknown/release/manifold_wasm_map_hook.wasm"

TMPDIR="$(mktemp -d /tmp/manifold-wasm-plug.XXXXXX)"
CONF="$TMPDIR/session.conf"
BUS_INFO="$TMPDIR/bus.info"
LOG="$TMPDIR/manifoldd.log"
DBUS_NAME="org.manifold.Test.WasmPlug.p$$"

cat >"$CONF" <<EOF
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:tmpdir=$TMPDIR</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow user="*"/>
    <allow own="*"/>
    <allow send_destination="*"/>
    <allow receive_sender="*"/>
  </policy>
</busconfig>
EOF

dbus-daemon --config-file="$CONF" --fork --print-address=1 --print-pid=1 >"$BUS_INFO"
DBUS_ADDRESS="$(sed -n '1p' "$BUS_INFO")"
DBUS_PID="$(sed -n '2p' "$BUS_INFO")"

cleanup() {
  target/debug/manifoldd ctl --dbus-address "$DBUS_ADDRESS" --dbus-name "$DBUS_NAME" quit >/dev/null 2>&1 || true
  if [[ -n "${MANIFOLD_PID:-}" ]] && kill -0 "$MANIFOLD_PID" 2>/dev/null; then
    kill "$MANIFOLD_PID" >/dev/null 2>&1 || true
  fi
  kill "$DBUS_PID" >/dev/null 2>&1 || true
}
trap cleanup EXIT

RUST_LOG="${RUST_LOG:-manifoldd=info,smithay=warn}" \
  target/debug/manifoldd run --headless --no-session-runner --no-default-modules --dbus-address "$DBUS_ADDRESS" --dbus-name "$DBUS_NAME" >"$LOG" 2>&1 &
MANIFOLD_PID=$!

wait_for_control() {
  for _ in $(seq 1 100); do
    if target/debug/manifoldd ctl --dbus-address "$DBUS_ADDRESS" --dbus-name "$DBUS_NAME" status-json >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.1
  done
  echo "ERR timed out waiting for Manifold DBus service" >&2
  tail -120 "$LOG" >&2 || true
  exit 1
}

wait_for_control

KEY_OUT="$(target/debug/manifoldd plug --dbus-address "$DBUS_ADDRESS" --dbus-name "$DBUS_NAME" "$KEY_WASM")"
[[ "$KEY_OUT" == OK\ plug\ kind=fast-key-handler\ path=* ]] || {
  echo "ERR key handler plug returned unexpected output: $KEY_OUT" >&2
  exit 1
}

MAP_OUT="$(target/debug/manifoldd plug --dbus-address "$DBUS_ADDRESS" --dbus-name "$DBUS_NAME" --kind fast-map-hook "$MAP_WASM")"
[[ "$MAP_OUT" == OK\ plug\ kind=fast-map-hook\ path=* ]] || {
  echo "ERR map hook plug returned unexpected output: $MAP_OUT" >&2
  exit 1
}

AUTO_MAP_OUT="$(target/debug/manifoldd ctl --dbus-address "$DBUS_ADDRESS" --dbus-name "$DBUS_NAME" plug "$MAP_WASM")"
[[ "$AUTO_MAP_OUT" == OK\ plug\ kind=fast-map-hook\ path=* ]] || {
  echo "ERR raw ctl auto map plug returned unexpected output: $AUTO_MAP_OUT" >&2
  exit 1
}

target/debug/manifoldd ctl --dbus-address "$DBUS_ADDRESS" --dbus-name "$DBUS_NAME" quit >/dev/null || true
wait "$MANIFOLD_PID" || true
kill "$DBUS_PID" >/dev/null 2>&1 || true
trap - EXIT

echo "OK WASM plug smoke passed"
echo "key_wasm=$KEY_WASM"
echo "map_wasm=$MAP_WASM"
