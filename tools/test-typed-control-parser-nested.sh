#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug/manifold"

ctl() {
  RUST_LOG=off timeout 3s "$BIN" ctl "$@"
}

expect_error() {
  local output
  if output="$(ctl "$@" 2>&1)"; then
    echo "ERR expected ctl failure: $*" >&2
    return 1
  fi
  printf '%s' "$output"
}

run_controller() {
  local workspace="$1"
  local component="$2"
  local proof="$workspace/.manifold/typed-control-parser-proof.md"
  local browser_json browser_error ambiguous_error inject_error
  local plug_response actor_response status_before status_after status_plugin

  mkdir -p "$workspace/.manifold"
  browser_json="$(ctl --json '["browser","list-json"]')"
  [[ "$browser_json" == '[]' ]]

  browser_error="$(expect_error browser list-json unexpected)"
  [[ "$browser_error" == *'invalid arguments for browser list-json'* ]]
  ambiguous_error="$(expect_error browser open --url https://one.test/ https://two.test/)"
  [[ "$ambiguous_error" == *'browser open URL may only be specified once'* ]]

  status_before="$(ctl status-json)"
  inject_error="$(expect_error inject motion 10 20 unexpected)"
  [[ "$inject_error" == *'usage: inject motion <x> <y>'* ]]
  status_after="$(ctl status-json)"
  python3 - "$status_before" "$status_after" <<'PY'
import json
import sys

before, after = map(json.loads, sys.argv[1:])
assert after["input_events"] == before["input_events"], (before, after)
PY

  plug_response="$(ctl plug --kind plugin --id typed-parser-proof "$component")"
  [[ "$plug_response" == "OK plug kind=plugin id=typed-parser-proof path=$component" ]]
  actor_response="$(ctl actors list)"
  [[ "$actor_response" == *'actor id=typed-parser-proof kind=wasm-component'* ]]

  # Drive the loaded component through its actual mailbox and retain the
  # actor's observable admission counter in the proof.
  ctl actors send typed-parser-proof spawn-worker >/dev/null
  for _ in $(seq 1 100); do
    actor_response="$(ctl actors list)"
    [[ "$actor_response" == *'actor id=typed-parser-proof kind=wasm-component enqueued_messages=1'* ]] && break
    sleep 0.05
  done
  [[ "$actor_response" == *'actor id=typed-parser-proof kind=wasm-component enqueued_messages=1'* ]]
  status_plugin="$(ctl status-json)"

  python3 - "$status_before" "$status_after" "$status_plugin" <<'PY'
import json
import sys

before, after, plugin = map(json.loads, sys.argv[1:])
assert after["input_events"] == before["input_events"]
assert plugin["plugin_actor_messages"] >= 1, plugin
assert plugin["plugin_actor_last_status"] == "typed-parser-proof:message_queued", plugin
PY

  cat >"$proof" <<EOF
# Typed control parser proof

The raw JSON control route decoded a browser command and returned its real
machine-readable result:

\`\`\`json
$browser_json
\`\`\`

The typed browser boundary rejected an extra token:

\`\`\`text
$browser_error
\`\`\`

The typed browser option parser rejected two competing URL forms:

\`\`\`text
$ambiguous_error
\`\`\`

The typed inject boundary rejected the wrong arity without changing the
daemon's input-event counter:

\`\`\`text
$inject_error
input_events before=$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["input_events"])' "$status_before") after=$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["input_events"])' "$status_after")
\`\`\`

The valid typed \`plug\` command registered a real Component Model actor. A
mailbox command then reached that actor and changed its observable admission
counter:

\`\`\`text
$plug_response
$actor_response
plugin_actor_last_status=$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["plugin_actor_last_status"])' "$status_plugin")
\`\`\`
EOF

  echo "typed_control_parser_controller=ok proof=$proof"
}

if [[ "${1:-}" == "--controller" ]]; then
  [[ $# -eq 3 ]]
  run_controller "$2" "$3"
  exit
fi

cd "$ROOT"
nix develop --command cargo build --quiet -p manifoldd --bins
nix develop --command cargo build --quiet --release --locked \
  --manifest-path examples/wasm-component-actor/Cargo.toml \
  --target wasm32-unknown-unknown
CORE="$ROOT/examples/wasm-component-actor/target/wasm32-unknown-unknown/release/manifold_component_actor_example.wasm"
COMPONENT="$ROOT/examples/wasm-component-actor/target/wasm32-unknown-unknown/release/manifold_component_actor_example.component.wasm"
nix develop --command cargo run --quiet -p manifold-wasm --example componentize -- \
  "$CORE" "$COMPONENT"

OUT="${MANIFOLD_TYPED_CONTROL_OUT:-$(mktemp -d /tmp/manifold-typed-control.XXXXXX)}"
WORKSPACE="$OUT/workspace"
mkdir -p "$WORKSPACE"
TEST_LOG="$OUT/test.log"
(
  cd "$WORKSPACE"
  RUST_LOG="${RUST_LOG:-manifoldd=info,manifold_wasm=info}" \
  timeout --kill-after=5s 120s nix develop "$ROOT" --command "$BIN" test nested \
    --force-outer \
    --out-dir "$OUT/nested" \
    --inner-command "$ROOT/tools/test-typed-control-parser-nested.sh --controller $WORKSPACE $COMPONENT"
) | tee "$TEST_LOG"

grep -qx 'topology=outer-supervisor' "$TEST_LOG"
grep -qx 'outer_presentation=offscreen' "$TEST_LOG"
grep -q 'typed_control_parser_controller=ok' "$OUT/nested/inner.log"
echo "typed_control_parser_nested=ok proof=$WORKSPACE/.manifold/typed-control-parser-proof.md"
