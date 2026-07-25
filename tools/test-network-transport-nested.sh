#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]]; then
  exec dbus-run-session -- "$0" "$@"
fi

for tool in weston-simple-egl; do
  if ! command -v "$tool" > /dev/null 2>&1; then
    echo "ERR $tool is required for the network transport proof" >&2
    exit 2
  fi
done

cargo build -p brood-remote -p broodd -p brood-client --bins
cargo build \
  --manifest-path examples/wasm-component-actor/Cargo.toml \
  --locked \
  --target wasm32-unknown-unknown \
  --release

TEST_TMP="$(mktemp -d /tmp/brood-network-transport.XXXXXX)"
RUNTIME_DIR="$TEST_TMP/runtime"
SERVER_LOG="$TEST_TMP/server.log"
RELAY_LOG="$TEST_TMP/relay.log"
PROBE_LOG="$TEST_TMP/probe.log"
SERVER_CERTIFICATE="$TEST_TMP/server.der"
SERVER_PRIVATE_KEY="$TEST_TMP/server-key.der"
CLIENT_CERTIFICATE="$TEST_TMP/client.der"
CLIENT_PRIVATE_KEY="$TEST_TMP/client-key.der"
ARTIFACT="$TEST_TMP/component-actor.component.wasm"
RELAY_ARTIFACT_CACHE="$TEST_TMP/relay-artifacts"
CLIENT_ARTIFACT_CACHE="$TEST_TMP/client-artifacts"
DBUS_DESTINATION="org.brood.NetworkTransportTest.p$$"
mkdir -m 700 "$RUNTIME_DIR"

cargo run --quiet -p brood-wasm --example componentize -- \
  examples/wasm-component-actor/target/wasm32-unknown-unknown/release/brood_component_actor_example.wasm \
  "$ARTIFACT"

cleanup() {
  local result=$?

  for pid in "${PROBE_PID:-}" "${RELAY_PID:-}" "${SERVER_PID:-}"; do
    if [[ -n "$pid" ]] && kill -0 "$pid" 2> /dev/null; then
      kill "$pid" > /dev/null 2>&1 || true
    fi
  done

  if ((result != 0)); then
    echo "probe_log=$PROBE_LOG" >&2
    tail -n 160 "$PROBE_LOG" >&2 || true
    echo "relay_log=$RELAY_LOG" >&2
    tail -n 160 "$RELAY_LOG" >&2 || true
    echo "server_log=$SERVER_LOG" >&2
    tail -n 160 "$SERVER_LOG" >&2 || true
  fi

  rm -rf -- "$TEST_TMP"
  return "$result"
}
trap cleanup EXIT

target/debug/brood-remote generate-certificate \
  --server-name localhost \
  --certificate "$SERVER_CERTIFICATE" \
  --private-key "$SERVER_PRIVATE_KEY"
target/debug/brood-remote generate-certificate \
  --server-name brood-client \
  --certificate "$CLIENT_CERTIFICATE" \
  --private-key "$CLIENT_PRIVATE_KEY"

env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  RUST_LOG="${RUST_LOG:-broodd=info,smithay=warn}" \
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

target/debug/brood-remote relay \
  --listen 127.0.0.1:0 \
  --certificate "$SERVER_CERTIFICATE" \
  --private-key "$SERVER_PRIVATE_KEY" \
  --trusted-client-certificate "$CLIENT_CERTIFICATE" \
  --dbus-destination "$DBUS_DESTINATION" \
  --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
  --offer-client-artifact "network-client-proof=$ARTIFACT" \
  --allow-session-artifact network-session-proof \
  --artifact-cache "$RELAY_ARTIFACT_CACHE" \
  > "$RELAY_LOG" 2>&1 &
RELAY_PID=$!

for _ in $(seq 1 100); do
  rg -q '^LISTEN=' "$RELAY_LOG" && break
  kill -0 "$RELAY_PID" 2> /dev/null || {
    echo "ERR remote relay exited during startup" >&2
    exit 1
  }
  sleep 0.1
done
CONNECT="$(
  sed -n 's/^LISTEN=\([^ ]*\).*/\1/p' "$RELAY_LOG" |
    head -n 1
)"
[[ -n "$CONNECT" ]] || {
  echo "ERR timed out waiting for the remote relay listener" >&2
  exit 1
}

target/debug/brood-remote probe \
  --connect "$CONNECT" \
  --server-name localhost \
  --certificate "$SERVER_CERTIFICATE" \
  --client-certificate "$CLIENT_CERTIFICATE" \
  --client-private-key "$CLIENT_PRIVATE_KEY" \
  --offer-session-artifact "network-session-proof=$ARTIFACT" \
  --allow-client-artifact network-client-proof \
  --artifact-cache "$CLIENT_ARTIFACT_CACHE" \
  > "$PROBE_LOG" 2>&1 &
PROBE_PID=$!

for _ in $(seq 1 100); do
  rg -q 'installed remote session artifact id=network-session-proof' "$RELAY_LOG" && break
  kill -0 "$PROBE_PID" 2> /dev/null || {
    echo "ERR remote probe exited before the relay attached" >&2
    exit 1
  }
  sleep 0.1
done
rg -q 'installed remote session artifact id=network-session-proof' "$RELAY_LOG" || {
  echo "ERR timed out waiting for the connected artifact exchange" >&2
  exit 1
}

ctl spawn weston-simple-egl > /dev/null
for _ in $(seq 1 100); do
  if ctl status | rg -q 'windows=[1-9]'; then
    break
  fi
  sleep 0.1
done
ctl status | rg -q 'windows=[1-9]' || {
  echo "ERR managed client did not map in the nested session" >&2
  exit 1
}

wait "$PROBE_PID"
PROBE_PID=""

rg -q '^OK remote transport ' "$PROBE_LOG" || {
  echo "ERR remote probe did not complete the transport workflow" >&2
  exit 1
}
rg -q 'artifacts_sent=1 artifacts_received=1' "$PROBE_LOG" || {
  echo "ERR remote probe did not verify both artifact directions" >&2
  exit 1
}
ctl actors list | rg -q 'network-session-proof' || {
  echo "ERR client-offered artifact did not become a live session actor" >&2
  exit 1
}
mapfile -t RECEIVED_ARTIFACTS < <(find "$CLIENT_ARTIFACT_CACHE" -type f -name artifact.wasm)
[[ "${#RECEIVED_ARTIFACTS[@]}" -eq 1 ]] || {
  echo "ERR expected one verified client artifact cache entry" >&2
  exit 1
}
target/debug/brood-client \
  --no-server \
  --no-init \
  --wasm-actor "network-client-proof=${RECEIVED_ARTIFACTS[0]}" \
  --check-wasm-actors \
  >> "$PROBE_LOG" 2>&1
rg -q 'presentation WASM actor check passed' "$PROBE_LOG" || {
  echo "ERR server-offered artifact did not load in the client runtime" >&2
  exit 1
}
wait "$RELAY_PID"
RELAY_PID=""

cat "$PROBE_LOG"
