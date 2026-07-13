#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug/manifold"

ctl() {
  RUST_LOG=off timeout 8s "$BIN" ctl "$@"
}

layout_shape() {
  python3 - "$1/state.json" "$2" <<'PY'
import json
import pathlib
import sys

state = json.loads(pathlib.Path(sys.argv[1]).read_text())
items = [item for item in state["layout"]["items"] if item["kind"].get("type") == "pane"]
assert len(items) == 2, items
rects = sorted((item["rect"] for item in items), key=lambda r: (r["x"], r["y"]))
if sys.argv[2] == "columns":
    assert rects[0]["x"] < rects[1]["x"], rects
    assert rects[0]["y"] == rects[1]["y"], rects
else:
    rects.sort(key=lambda r: (r["y"], r["x"]))
    assert rects[0]["y"] < rects[1]["y"], rects
    assert rects[0]["x"] == rects[1]["x"], rects
print(json.dumps(rects, sort_keys=True))
PY
}

wait_for_shape() {
  local dump="$1" shape="$2" status
  for _ in $(seq 1 120); do
    rm -rf "$dump"
    ctl debug dump "$dump" >/dev/null
    if layout_shape "$dump" "$shape" >"$dump/rects.json" 2>/dev/null; then
      status="$(ctl status-json)"
      python3 -c 'import json,sys; s=json.loads(sys.argv[1]); assert s["windows"] == 2 and s["resolved_layout_source"] == "wasm-policy" and s["plugin_actor_failures"] == 0, s' "$status"
      printf '%s\n' "$status" >"$dump/status.json"
      return
    fi
    sleep 0.05
  done
  echo "ERR timed out waiting for $shape plugin layout" >&2
  return 1
}

run_controller() {
  local out="$MANIFOLD_ACTOR_LIFECYCLE_OUT"
  local v1="$out/v1-target/wasm32-unknown-unknown/release/manifold_wasm_actor_lifecycle_proof.wasm"
  local broken="$out/broken-target/wasm32-unknown-unknown/release/manifold_wasm_actor_lifecycle_proof.wasm"
  local v2="$out/v2-target/wasm32-unknown-unknown/release/manifold_wasm_actor_lifecycle_proof.wasm"
  local first_dump="$out/columns" rejected_dump="$out/rejected-still-columns" second_dump="$out/rows"
  local first_load rejected_load ownership_probe replacement_load spawn_out status
  local probe_payload messages_before messages_after

  printf '\033[2J\033[H'
  echo 'Manifold actor generation and subscription proof'
  echo
  echo 'Generation one puts this terminal beside a live animated EGL client.'
  echo 'The same actor names then hot-reload and visibly stack the two panes.'
  echo 'A real non-subscriber actor would spawn an unwanted third pane if events were broadcast.'
  echo

  first_load="$(ctl plug "$v1")"
  echo "$first_load"
  spawn_out="$(ctl spawn weston-simple-egl -f)"
  echo "$spawn_out"
  wait_for_shape "$first_dump" columns
  echo 'Generation 1: two columns'
  cat "$first_dump/rects.json"

  if rejected_load="$(ctl plug "$broken" 2>&1)"; then
    echo 'ERR invalid second actor was accepted' >&2
    return 1
  fi
  test -n "$rejected_load"
  echo 'Broken candidate rejected; generation 1 remains published'
  echo "$rejected_load"
  status="$(ctl status-json)"
  messages_before="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["plugin_actor_messages"])' "$status")"
  probe_payload="$(python3 - "$first_dump/state.json" <<'PY'
import json
import pathlib
import sys
state = json.loads(pathlib.Path(sys.argv[1]).read_text())
panes = [item["kind"]["pane"] for item in state["layout"]["items"] if item["kind"].get("type") == "pane"]
assert len(panes) == 2, panes
print(json.dumps({"type":"layout-state","visible_panes":panes,"focused_pane":panes[-1],"pane_app_ids":[],"size":{"w":1280,"h":800}}, separators=(",",":")))
PY
  )"
  ownership_probe="$(ctl actors send layout "$probe_payload")"
  grep -q 'actor id=layout queued=true' <<<"$ownership_probe"
  echo "$ownership_probe"
  for _ in $(seq 1 100); do
    status="$(ctl status-json)"
    messages_after="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["plugin_actor_messages"])' "$status")"
    if (( messages_after >= messages_before + 1 )) && grep -q '"plugin_actor_last_status":"wasm-host:message:applied"' <<<"$status"; then
      break
    fi
    sleep 0.05
  done
  (( messages_after >= messages_before + 1 ))
  wait_for_shape "$rejected_dump" columns

  replacement_load="$(ctl plug "$v2")"
  echo "$replacement_load"
  wait_for_shape "$second_dump" rows
  echo 'Generation 2: two rows'
  cat "$second_dump/rects.json"

  status="$(ctl status-json)"
  python3 -c 'import json,sys; s=json.loads(sys.argv[1]); assert s["windows"] == 2, s; assert s["plugin_actor_failures"] == 0, s' "$status"
  printf '%s\n' "$status" >"$out/final-status.json"

  {
    echo '# WASM actor generation nested proof'
    echo
    echo 'The real `layout` actor subscribed to pane/layout events. Its same-name replacement changed two rendered clients from columns to rows. The real `poison` actor had no subscription and therefore did not receive those events or spawn its deliberately visible unwanted third client.'
    echo
    echo '```text'
    echo "$first_load"
    echo "$spawn_out"
    echo "columns=$(cat "$first_dump/rects.json")"
    echo "broken_candidate_rejected=$rejected_load"
    echo "old_generation_probe=$ownership_probe"
    echo "after_rejection=$(cat "$rejected_dump/rects.json")"
    echo "$replacement_load"
    echo "rows=$(cat "$second_dump/rects.json")"
    echo '```'
    echo
    echo 'The final status retains exactly two windows and no plugin actor failures:'
    echo
    echo '```json'
    echo "$status"
    echo '```'
    echo
    printf 'Rendered generation artifacts: `%s/composed.png` and `%s/composed.png`.\n' "$first_dump" "$second_dump"
  } >"$out/proof.md"

  echo
  echo 'Observable result: same two clients changed from columns to rows; no third poison pane appeared.'
  echo "Screenshots: $first_dump/composed.png $second_dump/composed.png"
  sleep 0.5
}

if [[ "${MANIFOLD_ACTOR_LIFECYCLE_TERMINAL:-0}" == 1 ]]; then
  run_controller 2>&1 | tee "$MANIFOLD_ACTOR_LIFECYCLE_OUT/controller.log"
  exit 0
fi

OUT="${MANIFOLD_ACTOR_LIFECYCLE_OUT:-$(mktemp -d /tmp/manifold-actor-lifecycle.XXXXXX)}"
rm -rf "$OUT"
mkdir -p "$OUT"

nix develop --command cargo build -p manifoldd
nix develop --command cargo build \
  --manifest-path examples/wasm-actor-lifecycle-proof/Cargo.toml \
  --target-dir "$OUT/broken-target" \
  --target wasm32-unknown-unknown \
  --features broken \
  --release
nix develop --command cargo build \
  --manifest-path examples/wasm-actor-lifecycle-proof/Cargo.toml \
  --target-dir "$OUT/v1-target" \
  --target wasm32-unknown-unknown \
  --release
nix develop --command cargo build \
  --manifest-path examples/wasm-actor-lifecycle-proof/Cargo.toml \
  --target-dir "$OUT/v2-target" \
  --target wasm32-unknown-unknown \
  --features replacement \
  --release

export MANIFOLD_ACTOR_LIFECYCLE_OUT="$OUT"
export MANIFOLD_ACTOR_LIFECYCLE_TERMINAL=1
RUST_LOG=manifoldd=info timeout --kill-after=5s 150s nix develop --command "$BIN" test nested \
  --force-outer \
  --out-dir "$OUT/nested" \
  --wait-for-inner-command-exit \
  --inner-command "weston-terminal --shell=$ROOT/tools/test-wasm-actor-lifecycle-nested.sh" \
  | tee "$OUT/nested-test.stdout"

grep -q '^inner_command_visible=true$' "$OUT/nested-test.stdout"
grep -q '^inner_command_completed=true$' "$OUT/nested-test.stdout"
grep -q 'changed two rendered clients from columns to rows' "$OUT/proof.md"
test -s "$OUT/columns/composed.png"
test -s "$OUT/rejected-still-columns/composed.png"
test -s "$OUT/rows/composed.png"
test "$(grep -c 'cancelled WASM plugin actor generation' "$OUT/nested/inner.log")" -eq 4
if cmp -s "$OUT/columns/composed.png" "$OUT/rows/composed.png"; then
  echo 'ERR column and row screenshots are identical' >&2
  exit 1
fi
python3 - "$OUT/final-status.json" <<'PY'
import json
import pathlib
import sys
status = json.loads(pathlib.Path(sys.argv[1]).read_text())
assert status["windows"] == 2, status
assert status["plugin_actor_failures"] == 0, status
PY

echo "wasm_actor_lifecycle_nested=verified proof=$OUT/proof.md columns=$OUT/columns/composed.png rows=$OUT/rows/composed.png"
