#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug/manifold"
WORKSPACE_WASM="$ROOT/plugins/default-workspace/target/wasm32-unknown-unknown/release/manifold_default_workspace_plugin.wasm"

ctl() {
  RUST_LOG=off timeout 5s "$BIN" ctl "$@"
}

run_terminal_controller() {
  local out="$MANIFOLD_GEOMETRY_OUT"
  local event_wrapper="$out/eventdemo-wrapper.sh"
  local client_log="$out/weston-eventdemo.log"
  local clean_client_log="$out/weston-eventdemo.txt"
  local before_json="$out/status-before-input.json"
  local after_json="$out/status-after-input.json"
  local dump="$out/inner-geometry-dump"
  local proof="$out/geometry-proof.md"
  local plug_out spawn_out event_pane status center_x center_y rect_text

  printf '\033[2J\033[H'
  echo 'Manifold checked geometry proof'
  echo
  echo 'The real workspace actor owns this two-pane layout.'
  echo 'Pointer motion, click, and wheel input target Weston eventdemo on the right.'
  echo

  plug_out="$("$BIN" plug "$WORKSPACE_WASM")"
  echo "$plug_out"
  [[ "$plug_out" == OK\ plug\ kind=plugin\ path=* ]]

  spawn_out="$(ctl spawn "$event_wrapper")"
  event_pane="$(sed -n 's/.*pane_id=\([0-9][0-9]*\).*/\1/p' <<<"$spawn_out")"
  test -n "$event_pane"

  for _ in $(seq 1 200); do
    status="$(ctl status-json)"
    if python3 -c 'import json,sys; p=json.loads(sys.argv[1]); raise SystemExit(0 if p["windows"] >= 2 and p["frames"] >= 2 else 1)' "$status"; then
      break
    fi
    sleep 0.05
  done
  python3 -c 'import json,sys; p=json.loads(sys.argv[1]); assert p["windows"] >= 2 and p["frames"] >= 2, p' "$status"
  [[ "$event_pane" == 2 ]]

  for _ in $(seq 1 100); do
    status="$(ctl status-json)"
    if python3 -c 'import json,sys; p=json.loads(sys.argv[1]); raise SystemExit(0 if p["resolved_layout_source"] == "actor" and p["resolved_layout_panes"] == 2 and p["resolved_layout_placeholders"] == 0 else 1)' "$status"; then
      break
    fi
    sleep 0.05
  done
  python3 -c 'import json,sys; p=json.loads(sys.argv[1]); assert p["resolved_layout_source"] == "actor" and p["resolved_layout_panes"] == 2 and p["resolved_layout_placeholders"] == 0, p' "$status"
  printf '%s\n' "$status" >"$before_json"

  ctl debug dump "$dump" >/dev/null
  read -r center_x center_y rect_text < <(python3 - "$dump/state.json" "$event_pane" <<'PY'
import json
import pathlib
import sys

state = json.loads(pathlib.Path(sys.argv[1]).read_text())
pane = int(sys.argv[2])
items = [
    item for item in state["layout"]["items"]
    if item["kind"].get("type") == "pane" and item["kind"].get("pane") == pane
]
assert len(items) == 1, (pane, state["layout"])
rect = items[0]["rect"]
assert rect["w"] > 0 and rect["h"] > 0, rect
print(
    rect["x"] + rect["w"] / 2,
    rect["y"] + rect["h"] / 2,
    f'{rect["x"]},{rect["y"]},{rect["w"]},{rect["h"]}',
)
PY
  )

  ctl inject motion "$center_x" "$center_y" >/dev/null
  ctl inject motion "$center_x" "$center_y" >/dev/null
  ctl inject button 272 pressed >/dev/null
  ctl inject button 272 released >/dev/null
  ctl inject wheel 0 120 >/dev/null
  ctl inject axis 0 7.5 >/dev/null
  sleep 0.8

  status="$(ctl status-json)"
  printf '%s\n' "$status" >"$after_json"
  python3 - "$before_json" "$after_json" <<'PY'
import json
import pathlib
import sys

before = json.loads(pathlib.Path(sys.argv[1]).read_text())
after = json.loads(pathlib.Path(sys.argv[2]).read_text())
for field, minimum in {
    "pointer_motion_events": 2,
    "pointer_button_events": 2,
    "pointer_axis_events": 2,
    "pointer_target_hits": 6,
}.items():
    delta = after[field] - before[field]
    assert delta >= minimum, (field, before[field], after[field], delta)
assert after["pointer_target_misses"] == before["pointer_target_misses"], (before, after)
PY

  tr -d '\000' <"$client_log" >"$clean_client_log"
  grep -q 'focus x:' "$clean_client_log"
  grep -q 'motion time:' "$clean_client_log"
  grep -q 'button time: .*button: 272, state: pressed' "$clean_client_log"
  grep -q 'button time: .*button: 272, state: released' "$clean_client_log"
  grep -q 'axis time: .*axis: vertical' "$clean_client_log"

  ctl debug dump "$dump" >/dev/null
  test -s "$dump/composed.png"
  test -s "$dump/state.json"
  python3 - "$dump/state.json" "$dump/composed.png" "$event_pane" <<'PY'
import json
import pathlib
import sys

state = json.loads(pathlib.Path(sys.argv[1]).read_text())
png = pathlib.Path(sys.argv[2])
pane = int(sys.argv[3])
status = state["status"]
assert state["layout"]["source"] == "actor", state["layout"]
assert status["resolved_layout_panes"] >= 2, status
assert status["resolved_layout_placeholders"] == 0, status
assert any(
    item["kind"].get("type") == "pane" and item["kind"].get("pane") == pane
    for item in state["layout"]["items"]
), state["layout"]
data = png.read_bytes()
assert data.startswith(b"\x89PNG\r\n\x1a\n") and len(data) > 4096, (png, len(data))
PY

  {
    echo '# Checked geometry nested proof'
    echo
    echo 'The `workspace` actor visibly split a real Weston terminal and Weston eventdemo into signed logical rectangles. Manifold then targeted the eventdemo pane by its resolved global rectangle and forwarded motion, click, wheel-v120, and fractional continuous scroll input.'
    echo
    echo '```text'
    echo "$plug_out"
    echo "$spawn_out"
    echo "eventdemo_pane=$event_pane"
    echo "eventdemo_global_rect=$rect_text"
    echo "injected_global_point=$center_x,$center_y"
    echo '```'
    echo
    echo 'The compositor metrics before and after input show six target hits and no new misses:'
    echo
    echo '```json'
    cat "$before_json"
    cat "$after_json"
    echo '```'
    echo
    echo 'The client itself observed the corresponding local input events:'
    echo
    echo '```text'
    grep -aE 'focus x:|motion time:|button time:|axis time:' "$clean_client_log" | tail -n 24
    echo '```'
    echo
    echo "The rendered two-pane proof is captured in \`$dump/composed.png\`; \`state.json\` records the plugin-owned rectangles."
  } >"$proof"

  echo
  echo "Plugin layout: eventdemo global rect $rect_text"
  echo "Input target: $center_x,$center_y"
  echo 'Observed: motion + click + wheel in eventdemo, zero new target misses'
  echo "Screenshot: $dump/composed.png"
  sleep 0.5
}

if [[ "${MANIFOLD_GEOMETRY_TERMINAL:-0}" == "1" ]]; then
  run_terminal_controller 2>&1 | tee "$MANIFOLD_GEOMETRY_OUT/controller.log"
  exit 0
fi

OUT="${MANIFOLD_GEOMETRY_OUT:-$(mktemp -d /tmp/manifold-geometry.XXXXXX)}"
rm -rf "$OUT"
mkdir -p "$OUT"

cat >"$OUT/eventdemo-wrapper.sh" <<EOF
#!/usr/bin/env bash
echo \$\$ >"$OUT/weston-eventdemo.pid"
exec stdbuf -oL -eL weston-eventdemo \\
  --log-motion --log-button --log-axis --log-focus --log-redraw --log-resize \\
  >"$OUT/weston-eventdemo.log" 2>&1
EOF
chmod +x "$OUT/eventdemo-wrapper.sh"

nix develop --command cargo build -p manifoldd
nix develop --command cargo build \
  --manifest-path plugins/default-workspace/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release

export MANIFOLD_GEOMETRY_OUT="$OUT"
export MANIFOLD_GEOMETRY_TERMINAL=1
timeout --kill-after=5s 120s nix develop --command "$BIN" test nested \
  --force-outer \
  --out-dir "$OUT/nested" \
  --wait-for-inner-command-exit \
  --inner-command "weston-terminal --shell=$ROOT/tools/test-geometry-conversions-nested.sh" \
  | tee "$OUT/nested-test.stdout"

grep -q '^inner_command_visible=true$' "$OUT/nested-test.stdout"
grep -q '^inner_command_completed=true$' "$OUT/nested-test.stdout"
grep -q 'pointer_target_misses.*' "$OUT/geometry-proof.md"
grep -q 'button time: .*button: 272, state: pressed' "$OUT/geometry-proof.md"
test -s "$OUT/inner-geometry-dump/composed.png"
test -s "$OUT/inner-geometry-dump/state.json"

echo "geometry_conversions_nested=verified proof=$OUT/geometry-proof.md screenshot=$OUT/inner-geometry-dump/composed.png"
