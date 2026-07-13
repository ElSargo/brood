#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug/manifold"
PLUGIN="$ROOT/plugins/default-workspace/target/wasm32-unknown-unknown/release/manifold_default_workspace_plugin.wasm"

ctl() {
  RUST_LOG=off timeout 5s "$BIN" ctl "$@"
}

windows() {
  python3 -c 'import json,sys; print(json.loads(sys.argv[1])["windows"])' "$1"
}

inject_key() {
  ctl inject key "$1" pressed >/dev/null
  ctl inject key "$1" released >/dev/null
}

inject_ctrl_key() {
  ctl inject key 37 pressed >/dev/null
  inject_key "$1"
  ctl inject key 37 released >/dev/null
}

run_controller() {
  local out="$MANIFOLD_TYPED_KEY_OUT"
  local dump="$out/inner-key-policy-dump"
  local proof="$out/typed-key-policy-proof.md"
  local status_summary="$out/status-summary.jsonl"
  local initial after_control after_action

  printf '\033[2J\033[H'
  echo 'Typed WASM key invocation proof'
  echo
  echo 'One event context carries key code, press/release state, modifiers, and input mode.'
  echo 'Control: bare B plus Ctrl+O must leave this as the only terminal.'
  echo 'Action: Ctrl+B selects workspace mode; Ctrl+O then opens terminal two.'
  echo

  for _ in $(seq 1 100); do
    initial="$(ctl status-json)"
    [[ "$(windows "$initial")" == 1 ]] && break
    sleep 0.05
  done
  [[ "$(windows "$initial")" == 1 ]]

  # Bare B has the right key code but lacks the modifier bit in the typed
  # context, so it cannot toggle the plugin's workspace mode. Ctrl+O therefore
  # remains forwarded to this terminal and must not create a pane.
  inject_key 56
  inject_ctrl_key 32
  sleep 0.5
  after_control="$(ctl status-json)"
  [[ "$(windows "$after_control")" == 1 ]]

  # The same B key with Ctrl in the invocation context toggles the plugin mode.
  # Ctrl+O now returns a typed SpawnApp message; press is actionable and release
  # is consumed without creating a duplicate.
  inject_ctrl_key 56
  inject_ctrl_key 32
  for _ in $(seq 1 100); do
    after_action="$(ctl status-json)"
    [[ "$(windows "$after_action")" == 2 ]] && break
    sleep 0.05
  done
  [[ "$(windows "$after_action")" == 2 ]]
  sleep 0.4
  after_action="$(ctl status-json)"
  [[ "$(windows "$after_action")" == 2 ]]

  python3 - "$initial" "$after_control" "$after_action" <<'PY'
import json
import sys

initial, control, action = map(json.loads, sys.argv[1:])
assert initial["windows"] == 1, initial
assert control["windows"] == 1, control
assert action["windows"] == 2, action
assert action["keyboard_events"] > control["keyboard_events"] > initial["keyboard_events"], (
    initial,
    control,
    action,
)
assert action["plugin_actor_failures"] == initial["plugin_actor_failures"], (initial, action)
assert action["plugin_actor_messages"] > control["plugin_actor_messages"], (control, action)
PY
  python3 - "$initial" "$after_control" "$after_action" >"$status_summary" <<'PY'
import json
import sys

for stage, raw in zip(("initial", "missing-modifier-control", "typed-context-action"), sys.argv[1:]):
    status = json.loads(raw)
    print(json.dumps({
        "stage": stage,
        "windows": status["windows"],
        "keyboard_events": status["keyboard_events"],
        "plugin_actor_messages": status["plugin_actor_messages"],
        "plugin_actor_failures": status["plugin_actor_failures"],
        "plugin_actor_last_status": status["plugin_actor_last_status"],
    }, separators=(",", ":")))
PY

  ctl debug dump "$dump" >/dev/null
  test -s "$dump/composed.png"
  test -s "$dump/state.json"
  python3 - "$dump/state.json" "$dump/composed.png" <<'PY'
import json
import pathlib
import sys

state = json.loads(pathlib.Path(sys.argv[1]).read_text())
png = pathlib.Path(sys.argv[2]).read_bytes()
assert state["status"]["windows"] == 2, state["status"]
assert len([
    item for item in state["layout"]["items"]
    if item["kind"].get("type") == "pane"
]) == 2, state["layout"]
assert png.startswith(b"\x89PNG\r\n\x1a\n") and len(png) > 4096, len(png)
PY
  # A structurally valid all-black PNG is not an observable UI proof. The two
  # rendered terminals include hundreds of colors from text, borders, and
  # terminal backgrounds.
  test "$(magick "$dump/composed.png" -format '%k' info:)" -gt 50

  {
    echo '# Typed WASM key invocation nested proof'
    echo
    echo 'The checked-in `default-workspace` policy ran inside a real nested Manifold session. A Weston terminal was the visible control surface.'
    echo
    echo '- Bare `B` followed by `Ctrl+O` did not open a pane: the B event lacked the required Ctrl modifier in its typed invocation context.'
    echo '- `Ctrl+B` selected the plugin workspace mode.'
    echo '- The next `Ctrl+O` opened exactly one additional real Foot terminal. Its release event did not open a duplicate.'
    echo
    echo '```json'
    cat "$status_summary"
    echo '```'
    echo
    echo "The rendered two-terminal result is captured in \`$dump/composed.png\`; the matching compositor state is in \`$dump/state.json\`."
  } >"$proof"

  echo
  echo 'Control observed: 1 terminal'
  echo 'Typed Ctrl context observed: exactly 2 terminals'
  echo "Screenshot: $dump/composed.png"
  sleep 0.5
}

if [[ "${MANIFOLD_TYPED_KEY_CONTROLLER:-0}" == 1 ]]; then
  run_controller 2>&1 | tee "$MANIFOLD_TYPED_KEY_OUT/controller.log"
  exit 0
fi

OUT="${MANIFOLD_TYPED_KEY_OUT:-$(mktemp -d /tmp/manifold-typed-key.XXXXXX)}"
rm -rf "$OUT"
mkdir -p "$OUT"

nix develop --command cargo build --quiet -p manifoldd --bins
nix develop --command cargo build --quiet --release --locked \
  --manifest-path plugins/default-workspace/Cargo.toml \
  --target wasm32-unknown-unknown

export MANIFOLD_TYPED_KEY_OUT="$OUT"
export MANIFOLD_TYPED_KEY_CONTROLLER=1
timeout --kill-after=5s 120s nix develop --command "$BIN" test nested \
  --force-outer \
  --out-dir "$OUT/nested" \
  --wait-for-inner-command-exit \
  --inner-wasm-key-policy workspace "$PLUGIN" \
  --inner-command "weston-terminal --shell=$ROOT/tools/test-typed-key-policy-nested.sh" \
  | tee "$OUT/nested-test.stdout"

grep -qx 'topology=outer-supervisor' "$OUT/nested-test.stdout"
grep -qx 'outer_presentation=offscreen' "$OUT/nested-test.stdout"
grep -qx 'inner_command_visible=true' "$OUT/nested-test.stdout"
grep -qx 'inner_command_completed=true' "$OUT/nested-test.stdout"
grep -q 'Control observed: 1 terminal' "$OUT/controller.log"
grep -q 'Typed Ctrl context observed: exactly 2 terminals' "$OUT/controller.log"
test -s "$OUT/inner-key-policy-dump/composed.png"
test -s "$OUT/inner-key-policy-dump/state.json"

echo "typed_key_policy_nested=verified proof=$OUT/typed-key-policy-proof.md screenshot=$OUT/inner-key-policy-dump/composed.png"
