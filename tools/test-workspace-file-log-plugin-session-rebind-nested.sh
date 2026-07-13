#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug/manifold"

ctl() {
  RUST_LOG=off timeout 3s "$BIN" ctl "$@"
}

wait_for_record() {
  local log="$1"
  local kind="$2"
  local path="$3"
  for _ in $(seq 1 200); do
    if [[ -f "$log" ]] && grep -Fqx "$kind"$'\t'"$path" "$log"; then
      return 0
    fi
    sleep 0.05
  done
  echo "ERR timed out waiting for $kind $path in $log" >&2
  [[ -f "$log" ]] && tail -50 "$log" >&2
  ctl status-json >&2 || true
  return 1
}

wait_for_generation_after() {
  local previous="$1"
  for _ in $(seq 1 200); do
    local status generation
    status="$(ctl status-json)"
    generation="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["preview1_compat_generation"])' <<<"$status")"
    if (( generation > previous )); then
      printf '%s\n' "$generation"
      return 0
    fi
    sleep 0.05
  done
  echo "ERR Preview 1 generation did not advance beyond $previous" >&2
  return 1
}

assert_session() {
  local expected_id="$1"
  local expected_cwd="$2"
  local status sessions
  status="$(ctl status-json)"
  sessions="$(ctl surface-session list-json)"
  python3 - "$expected_id" "$expected_cwd" "$status" "$sessions" <<'PY'
import json
import os
import sys

expected_id = int(sys.argv[1])
expected_cwd = os.path.realpath(sys.argv[2])
status = json.loads(sys.argv[3])
sessions = json.loads(sys.argv[4])
assert status["active_surface_session"] == expected_id, status
assert status["plugin_actor_failures"] == 0, status
actors = {actor[0]: actor for actor in status["plugin_actors"]}
assert actors["workspace-file-log"][1] == "wasm-preview1-compat", actors
assert sessions["active"] == expected_id, sessions
session = next(item for item in sessions["sessions"] if item["id"] == expected_id)
assert session["active"] is True, session
assert os.path.realpath(session["working_directory"]) == expected_cwd, session
PY
}

run_controller() {
  local first="$1"
  local second="$2"
  local first_log="$first/.manifold/workspace-file-log.log"
  local second_log="$second/.manifold/workspace-file-log.log"

  wait_for_record "$first_log" initial first-only.txt
  local first_session first_generation
  read -r first_session first_generation < <(
    ctl status-json | python3 -c 'import json,sys; s=json.load(sys.stdin); print(s["active_surface_session"], s["preview1_compat_generation"])'
  )
  assert_session "$first_session" "$first"

  local create second_session
  create="$(ctl surface-session dir-new "$second" default second-workspace)"
  second_session="$(sed -n 's/.*active=\([0-9][0-9]*\).*/\1/p' <<<"$create")"
  [[ -n "$second_session" && "$second_session" != "$first_session" ]]
  assert_session "$second_session" "$second"
  local second_generation
  second_generation="$(wait_for_generation_after "$first_generation")"

  # Keep the second session live after switching away so the test exercises
  # repeated activation of two real sessions rather than recreation.
  ctl spawn-in-session "$second_session" weston-simple-shm >/dev/null
  wait_for_record "$second_log" initial second-only.txt
  local first_hash
  first_hash="$(sha256sum "$first_log")"
  printf 'second active\n' > "$second/second-active.txt"
  wait_for_record "$second_log" created second-active.txt
  [[ "$(sha256sum "$first_log")" == "$first_hash" ]]
  [[ ! -e "$first/second-active.txt" ]]

  local second_hash
  second_hash="$(sha256sum "$second_log")"
  ctl surface-session switch "$first_session" >/dev/null
  assert_session "$first_session" "$first"
  wait_for_generation_after "$second_generation" >/dev/null
  printf 'first active again\n' > "$first/first-active-again.txt"
  wait_for_record "$first_log" created first-active-again.txt
  [[ "$(sha256sum "$second_log")" == "$second_hash" ]]
  [[ ! -e "$second/first-active-again.txt" ]]

  echo "workspace_file_log_session_rebind=ok first=$first second=$second"
}

if [[ "${1:-}" == "--controller" ]]; then
  [[ $# -eq 3 ]]
  run_controller "$2" "$3"
  exit
fi

cd "$ROOT"
nix develop --command cargo build --quiet -p manifoldd --bins
(
  cd plugins/workspace-file-log
  nix develop "$ROOT" --command cargo build --quiet --release --locked
)

OUT="${MANIFOLD_WORKSPACE_REBIND_OUT:-$(mktemp -d /tmp/manifold-workspace-rebind.XXXXXX)}"
FIRST="$OUT/first"
SECOND="$OUT/second"
mkdir -p "$FIRST" "$SECOND"
printf 'first\n' > "$FIRST/first-only.txt"
printf 'second\n' > "$SECOND/second-only.txt"

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
  cd "$FIRST"
  RUST_LOG="${RUST_LOG:-manifoldd=info,manifold_wasm=info}" \
  timeout --kill-after=5s 120s nix develop "$ROOT" --command "$BIN" test nested \
    --force-outer \
    --out-dir "$OUT/nested" \
    --inner-config "$CONFIG" \
    --inner-command "$ROOT/tools/test-workspace-file-log-plugin-session-rebind-nested.sh --controller $FIRST $SECOND"
) | tee "$TEST_LOG"

grep -qx 'topology=outer-supervisor' "$TEST_LOG"
grep -qx 'outer_presentation=offscreen' "$TEST_LOG"
grep -q 'workspace_file_log_session_rebind=ok' "$OUT/nested/inner.log"

echo "workspace_file_log_session_rebind_nested=ok out=$OUT"
