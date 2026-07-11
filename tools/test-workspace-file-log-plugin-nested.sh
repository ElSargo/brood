#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug/manifold"

ctl() {
  RUST_LOG=off timeout 2s "$BIN" ctl "$@"
}

wait_for_log() {
  local log="$1"
  local pattern="$2"
  for _ in $(seq 1 200); do
    if [[ -f "$log" ]] && grep -q $'^'"$pattern"$'\t' "$log"; then
      return 0
    fi
    sleep 0.05
  done
  echo "ERR timed out waiting for $pattern in $log" >&2
  [[ -f "$log" ]] && tail -50 "$log" >&2
  ctl status-json >&2 || true
  return 1
}

run_controller() {
  local workspace="$1"
  local log="$workspace/.manifold/workspace-file-log.log"

  wait_for_log "$log" "initial"
  # The initial assertion is path-specific so it cannot pass on an empty log.
  for _ in $(seq 1 200); do
    [[ -f "$log" ]] && grep -q $'^initial\talpha.txt$' "$log" && break
    sleep 0.05
  done
  grep -q $'^initial\talpha.txt$' "$log"

  printf 'created\n' > "$workspace/new-file.txt"
  printf 'changed\n' >> "$workspace/alpha.txt"
  rm "$workspace/src/removed.txt"

  wait_for_log "$log" "created"
  wait_for_log "$log" "modified"
  wait_for_log "$log" "removed"
  grep -q $'^created\tnew-file.txt$' "$log"
  grep -q $'^modified\talpha.txt$' "$log"
  grep -q $'^removed\tsrc/removed.txt$' "$log"

  # The worker remains active while its host actor services another mailbox turn.
  ctl actors send workspace-file-log probe >/dev/null
  actors="$(ctl actors list)"
  [[ "$actors" == *"id=workspace-file-log kind=wasm-preview1-compat"* ]]
  status="$(ctl status-json)"
  python3 - "$status" <<'PY'
import json
import sys

status = json.loads(sys.argv[1])
assert status["preview1_compat_workers_active"] >= 1, status
assert status["preview1_compat_mailbox_dispatched"] >= 2, status
assert status["plugin_actor_failures"] == 0, status
PY
  echo "workspace_file_log_controller=ok log=$log"
}

if [[ "${1:-}" == "--controller" ]]; then
  [[ $# -eq 2 ]]
  run_controller "$2"
  exit
fi

cd "$ROOT"
nix develop --command cargo build --quiet -p manifoldd --bins
(
  cd plugins/workspace-file-log
  # Cargo reads the target-specific stack/TLS export flags from this crate's
  # own .cargo/config.toml when it is the invocation directory.
  nix develop "$ROOT" --command cargo build --quiet --release --locked
)

OUT="${MANIFOLD_WORKSPACE_LOG_OUT:-$(mktemp -d /tmp/manifold-workspace-log.XXXXXX)}"
WORKSPACE="$OUT/workspace"
mkdir -p "$WORKSPACE/src"
printf 'alpha\n' > "$WORKSPACE/alpha.txt"
printf 'remove me\n' > "$WORKSPACE/src/removed.txt"

PLUGIN="$ROOT/plugins/workspace-file-log/target/wasm32-wasip1-threads/release/manifold_workspace_file_log_plugin.wasm"
CONFIG="$OUT/manifold.toml"
printf '%s\n' \
  '[module.workspace-file-log]' \
  'enabled = true' \
  'kind = "plugin"' \
  "path = \"$PLUGIN\"" \
  'capabilities = ["filesystem:workspace-write"]' \
  'startup-message = "start"' > "$CONFIG"

TEST_LOG="$OUT/test.log"
(
  cd "$WORKSPACE"
  RUST_LOG="${RUST_LOG:-manifoldd=info,manifold_wasm=info}" \
  timeout --kill-after=5s 120s nix develop "$ROOT" --command "$BIN" test nested \
    --force-outer \
    --out-dir "$OUT/nested" \
    --inner-config "$CONFIG" \
    --inner-command "$ROOT/tools/test-workspace-file-log-plugin-nested.sh --controller $WORKSPACE"
) | tee "$TEST_LOG"

grep -qx 'topology=outer-supervisor' "$TEST_LOG"
grep -qx 'outer_presentation=offscreen' "$TEST_LOG"
grep -q 'workspace_file_log_controller=ok' "$OUT/nested/inner.log"

echo "workspace_file_log_nested=ok workspace=$WORKSPACE log=$WORKSPACE/.manifold/workspace-file-log.log"
