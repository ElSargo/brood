#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug/manifold"

ctl() {
  RUST_LOG=off timeout 2s "$BIN" ctl "$@"
}

wait_for_report() {
  local report="$1"
  local pattern="$2"
  for _ in $(seq 1 200); do
    if [[ -f "$report" ]] && grep -Fq "$pattern" "$report"; then
      return 0
    fi
    sleep 0.05
  done
  echo "ERR timed out waiting for $pattern in $report" >&2
  [[ -f "$report" ]] && cat "$report" >&2
  ctl status-json >&2 || true
  return 1
}

run_controller() {
  local workspace="$1"
  local component="$2"
  local report="$workspace/.manifold/component-workspace-watch.md"

  wait_for_report "$report" '| rescan | `.` |'
  printf 'created\n' >"$workspace/created.txt"
  printf 'modified\n' >>"$workspace/seed.txt"
  rm "$workspace/remove-me.txt"
  python3 - "$workspace" <<'PY'
import os
import sys

root = os.fsencode(sys.argv[1])
# Both names become `collision-�.txt` under lossy UTF-8 conversion. The guest
# must receive distinct native bytes and render a reversible representation.
for name in (b"collision-\xff.txt", b"collision-\xfe.txt"):
    descriptor = os.open(os.path.join(root, name), os.O_WRONLY | os.O_CREAT, 0o600)
    os.write(descriptor, b"distinct native path\n")
    os.close(descriptor)
PY
  wait_for_report "$report" '| created | `created.txt` |'
  wait_for_report "$report" '| modified | `seed.txt` |'
  wait_for_report "$report" '| removed | `remove-me.txt` |'
  wait_for_report "$report" '| created | `collision-\xFF.txt` |'
  wait_for_report "$report" '| created | `collision-\xFE.txt` |'

  # The artifact is the proof: a human-readable history produced by the real
  # Component guest from real workspace mutations, not a success sentinel.
  grep -Fq '# Component workspace watch' "$report"
  grep -Fq '| generation | change | relative path |' "$report"

  status="$(ctl status-json)"
  python3 - "$status" "$workspace/.manifold/component-workspace-watch.status.json" <<'PY'
import json
import sys
status = json.loads(sys.argv[1])
assert status["filesystem_watcher_registrations"] == 1, status
assert status["filesystem_watcher_subscriptions"] == 1, status
proof = {
    "backend": "component",
    "filesystem_watcher_registrations": status["filesystem_watcher_registrations"],
    "filesystem_watcher_subscriptions": status["filesystem_watcher_subscriptions"],
}
with open(sys.argv[2], "w", encoding="utf-8") as output:
    json.dump(proof, output, indent=2, sort_keys=True)
    output.write("\n")
PY
  actors="$(ctl actors list)"
  [[ "$actors" == *"id=component-workspace-watch kind=wasm-component"* ]]

  # Replacement returns only after the old generation drops its WIT resource,
  # notification, pollable wait, and native subscription.
  ctl plug --id component-workspace-watch "$component" >/dev/null
  status="$(ctl status-json)"
  python3 - "$status" <<'PY'
import json
import sys
status = json.loads(sys.argv[1])
assert status["filesystem_watcher_registrations"] == 0, status
assert status["filesystem_watcher_subscriptions"] == 0, status
PY
  echo "component_workspace_watch_controller=ok report=$report"
}

if [[ "${1:-}" == "--controller" ]]; then
  [[ $# -eq 3 ]]
  run_controller "$2" "$3"
  exit
fi

cd "$ROOT"
nix develop --command cargo build --quiet -p manifoldd --bins
nix develop --command cargo build --quiet --release --locked \
  --manifest-path plugins/component-workspace-watch/Cargo.toml \
  --target wasm32-unknown-unknown
core="$ROOT/plugins/component-workspace-watch/target/wasm32-unknown-unknown/release/manifold_component_workspace_watch_plugin.wasm"
component="$ROOT/plugins/component-workspace-watch/target/wasm32-unknown-unknown/release/manifold_component_workspace_watch_plugin.component.wasm"
nix develop --command cargo run --quiet -p manifold-wasm --example componentize -- \
  "$core" "$component"

OUT="${MANIFOLD_COMPONENT_WATCH_OUT:-$(mktemp -d /tmp/manifold-component-watch.XXXXXX)}"
WORKSPACE="$OUT/workspace"
mkdir -p "$WORKSPACE/.manifold"
printf 'seed\n' >"$WORKSPACE/seed.txt"
printf 'remove\n' >"$WORKSPACE/remove-me.txt"
CONFIG="$OUT/manifold.toml"
printf '%s\n' \
  '[module.component-workspace-watch]' \
  'enabled = true' \
  'kind = "plugin"' \
  "path = \"$component\"" \
  'capabilities = ["filesystem:workspace-write"]' \
  'startup-message = "start"' >"$CONFIG"

TEST_LOG="$OUT/test.log"
(
  cd "$WORKSPACE"
  RUST_LOG="${RUST_LOG:-manifoldd=info,manifold_wasm=info}" \
  timeout --kill-after=5s 120s nix develop "$ROOT" --command "$BIN" test nested \
    --force-outer \
    --out-dir "$OUT/nested" \
    --inner-config "$CONFIG" \
    --inner-command "$ROOT/tools/test-component-workspace-watch-plugin-nested.sh --controller $WORKSPACE $component"
) | tee "$TEST_LOG"

grep -qx 'topology=outer-supervisor' "$TEST_LOG"
grep -qx 'outer_presentation=offscreen' "$TEST_LOG"
grep -q 'component_workspace_watch_controller=ok' "$OUT/nested/inner.log"
echo "component_workspace_watch_nested=ok workspace=$WORKSPACE report=$WORKSPACE/.manifold/component-workspace-watch.md"
