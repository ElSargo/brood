#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug/manifold"
CLIENT="$ROOT/target/debug/manifold-client"

if [[ "${MANIFOLD_INIT_TEST_CONTROLLER:-0}" == 1 ]]; then
  if [[ "${MANIFOLD_INIT_TEST_DBUS_WRAPPED:-0}" != 1 ]]; then
    exec env -u MANIFOLDD_DBUS_ADDRESS -u MANIFOLDD_DBUS_DESTINATION -u manifold_session \
      dbus-run-session -- env MANIFOLD_INIT_TEST_DBUS_WRAPPED=1 "$0"
  fi

  exec > >(tee "$MANIFOLD_INIT_TEST_OUT/controller.log") 2>&1
  export XDG_CACHE_HOME="$MANIFOLD_INIT_TEST_OUT/cache"
  export MANIFOLD_CONFIG="$MANIFOLD_INIT_TEST_OUT/manifold.toml"
  mkdir -p "$XDG_CACHE_HOME"
  client_log="$MANIFOLD_INIT_TEST_OUT/client.log"
  server_log="$XDG_CACHE_HOME/manifold/session-server/global.log"

  "$CLIENT" >"$client_log" 2>&1 &
  client_pid=$!
  trap 'kill "$client_pid" 2>/dev/null || true' EXIT

  for _ in $(seq 1 300); do
    if [[ -f "$server_log" ]] \
      && grep -q 'connected to Manifold server' "$client_log" \
      && grep -q 'created presentation session' "$client_log" \
      && grep -q 'actor=manifold.session-init.1' "$server_log" \
      && grep -q 'command=foot' "$server_log"; then
      status="$($BIN ctl status 2>/dev/null || true)"
      if grep -q 'surface_sessions=1' <<<"$status" \
        && grep -q 'windows=1' <<<"$status"; then
        break
      fi
    fi
    if ! kill -0 "$client_pid" 2>/dev/null; then
      echo 'ERR manifold-client exited before the default terminal mapped' >&2
      sed -n '1,240p' "$client_log" >&2
      [[ -f "$server_log" ]] && sed -n '1,240p' "$server_log" >&2
      exit 1
    fi
    sleep 0.05
  done

  # Give the external presentation lane one frame round-trip after the first
  # hosted surface maps; the server and client are intentionally independent.
  sleep 1
  status="$($BIN ctl status)"
  grep -q 'surface_sessions=1' <<<"$status"
  grep -q 'windows=1' <<<"$status"
  grep -q 'connected to Manifold server' "$client_log"
  grep -q 'created presentation session' "$client_log"
  grep -q 'imported window DMA-BUF .*app_id=foot' "$client_log"
  grep -q 'actor=manifold.session-init.1' "$server_log"
  grep -q 'command=foot' "$server_log"

  echo 'Client-only actor init proof'
  echo
  grep -E 'connected to Manifold server|created presentation session' "$client_log"
  grep -E 'actor=manifold.session-init.1|command=foot' "$server_log"
  echo "$status"
  touch "$MANIFOLD_INIT_TEST_OUT/controller.ready"

  for _ in $(seq 1 300); do
    [[ -f "$MANIFOLD_INIT_TEST_OUT/controller.stop" ]] && break
    sleep 0.05
  done
  test -f "$MANIFOLD_INIT_TEST_OUT/controller.stop"

  kill "$client_pid" 2>/dev/null || true
  wait "$client_pid" || true
  "$BIN" ctl quit >/dev/null 2>&1 || true
  trap - EXIT
  touch "$MANIFOLD_INIT_TEST_OUT/controller.done"
  exit 0
fi

OUT="${MANIFOLD_INIT_TEST_OUT:-$(mktemp -d /tmp/manifold-actor-init.XXXXXX)}"
PACKAGE="$OUT/package"
mkdir -p "$PACKAGE"

nix develop --command cargo build -p manifoldd -p manifold-client --bins
nix develop --command cargo build \
  --manifest-path examples/wasm-init-actor/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release
nix develop --command cargo run --quiet -p manifold-wasm --example componentize -- \
  examples/wasm-init-actor/target/wasm32-unknown-unknown/release/manifold_wasm_init_actor_example.wasm \
  "$PACKAGE/init.component.wasm"

printf '[init]\nsession = "%s"\n' "$PACKAGE/init.component.wasm" >"$OUT/manifold.toml"

export MANIFOLD_CONFIG="$OUT/manifold.toml"
export MANIFOLD_INIT_TEST_CONTROLLER=1
export MANIFOLD_INIT_TEST_OUT="$OUT"
RUST_LOG=manifoldd=info nix develop --command manifold test nested \
  --force-outer \
  --keep-running \
  --out-dir "$OUT/nested" \
  --inner-command "weston-terminal --shell=$ROOT/tools/test-actor-init-nested.sh" \
  > >(tee "$OUT/nested-test.stdout") 2>&1 &
test_pid=$!
trap 'kill "$test_pid" 2>/dev/null || true' EXIT

for _ in $(seq 1 400); do
  [[ -f "$OUT/controller.ready" ]] && break
  sleep 0.05
done
test -f "$OUT/controller.ready"

inner_destination="$(sed -n 's/^inner_dbus_destination=//p' "$OUT/nested-test.stdout" | tail -1)"
outer_destination="$(sed -n 's/^outer_dbus_destination=//p' "$OUT/nested-test.stdout" | tail -1)"
RUST_LOG=off manifold ctl --dbus-destination "$inner_destination" debug dump "$OUT/nested/inner-dump" >/dev/null
touch "$OUT/controller.stop"
for _ in $(seq 1 300); do
  [[ -f "$OUT/controller.done" ]] && break
  sleep 0.05
done
test -f "$OUT/controller.done"

RUST_LOG=off manifold ctl --dbus-destination "$inner_destination" quit >/dev/null
RUST_LOG=off manifold ctl --dbus-destination "$outer_destination" quit >/dev/null
wait "$test_pid" || true
trap - EXIT

grep -q 'connected to Manifold server' "$OUT/client.log"
grep -q 'created presentation session' "$OUT/client.log"
grep -q 'imported window DMA-BUF .*app_id=foot' "$OUT/client.log"
grep -q 'actor=manifold.session-init.1' "$OUT/cache/manifold/session-server/global.log"
grep -q 'command=foot' "$OUT/cache/manifold/session-server/global.log"
grep -q 'surface_sessions=1' "$OUT/controller.log"
grep -q 'windows=1' "$OUT/controller.log"
test -s "$OUT/nested/inner-dump/composed.png"

echo "actor_init_nested=verified proof=$OUT/nested/inner-dump/composed.png client=$OUT/client.log server=$OUT/cache/manifold/session-server/global.log"
