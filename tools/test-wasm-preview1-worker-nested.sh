#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug/manifold"

ctl() {
  RUST_LOG=off timeout 2s "$BIN" ctl "$@"
}

json_field() {
  local status="$1"
  local field="$2"
  python3 - "$status" "$field" <<'PY'
import json
import sys

value = json.loads(sys.argv[1])[sys.argv[2]]
if isinstance(value, bool):
    print("true" if value else "false")
else:
    print(value)
PY
}

wait_field_at_least() {
  local field="$1"
  local expected="$2"
  local label="$3"
  local status value
  for _ in $(seq 1 200); do
    status="$(ctl status-json)"
    value="$(json_field "$status" "$field")"
    if (( value >= expected )); then
      printf '%s' "$status"
      return 0
    fi
    sleep 0.05
  done
  echo "ERR timed out waiting for $label ($field >= $expected)" >&2
  ctl status-json >&2 || true
  return 1
}

wait_field_equals() {
  local field="$1"
  local expected="$2"
  local label="$3"
  local status value
  for _ in $(seq 1 200); do
    status="$(ctl status-json)"
    value="$(json_field "$status" "$field")"
    if [[ "$value" == "$expected" ]]; then
      printf '%s' "$status"
      return 0
    fi
    sleep 0.05
  done
  echo "ERR timed out waiting for $label ($field == $expected)" >&2
  ctl status-json >&2 || true
  return 1
}

run_generation() {
  local actor="$1"
  local input="$2"
  local expected_value="$3"
  local status

  echo "preview1-controller: start shared-memory workers input=$input" >&2
  ctl actors send "$actor" start-notify >/dev/null
  status="$(wait_field_at_least preview1_compat_wait_registrations 4 "two worker wait sets")"
  [[ "$(json_field "$status" preview1_compat_workers_started)" -ge 2 ]] || {
    echo "ERR notification workers waited before their started lifecycle events" >&2
    return 1
  }
  [[ "$(json_field "$status" preview1_compat_notifications)" == 3 ]] || {
    echo "ERR actor did not own three notification resources" >&2
    return 1
  }
  [[ "$(json_field "$status" preview1_compat_pollables)" == 4 ]] || {
    echo "ERR workers did not own four notification pollables" >&2
    return 1
  }

  ctl actors send "$actor" probe >/dev/null
  status="$(wait_field_at_least preview1_compat_mailbox_dispatched 2 "actor probe")"
  [[ "$(json_field "$status" preview1_compat_workers_finished)" == 0 ]] || {
    echo "ERR notification worker finished before the actor progress probe" >&2
    return 1
  }

  ctl actors send "$actor" alternate:B >/dev/null
  status="$(wait_field_at_least preview1_compat_workers_finished 1 "alternate select")"
  status="$(wait_field_equals preview1_compat_wait_registrations 2 "losing worker re-arm")"

  ctl actors send "$actor" "queue:$input" >/dev/null
  wait_field_at_least preview1_compat_workers_finished 2 "shared queue finish" >/dev/null
  wait_field_at_least preview1_compat_notifications_delivered 1 "coalesced actor notification" >/dev/null
  wait_field_equals preview1_compat_wait_registrations 0 "worker wait teardown" >/dev/null
  wait_field_equals preview1_compat_pollables 0 "pollable teardown" >/dev/null

  # Keep one ordinary blocking-looking WASI read as a compatibility adapter.
  ctl actors send "$actor" start-stdin >/dev/null
  status="$(wait_field_at_least preview1_compat_worker_reads_started 1 "ordinary worker read")"
  [[ "$(json_field "$status" preview1_compat_workers_started)" -ge 3 ]] || {
    echo "ERR stdin worker reached WASI before its started lifecycle event" >&2
    return 1
  }
  ctl actors send "$actor" probe >/dev/null
  wait_field_at_least preview1_compat_mailbox_dispatched 6 "stdin actor probe" >/dev/null
  ctl actors send "$actor" "feed:$input" >/dev/null
  wait_field_at_least preview1_compat_completion_delivered 1 "actor completion" >/dev/null
  wait_field_at_least preview1_compat_workers_finished 3 "stdin worker finish" >/dev/null
  wait_field_equals preview1_compat_last_worker_value "$expected_value" "worker result" >/dev/null
  echo "preview1-controller: finished shared and WASI paths value=$expected_value" >&2
}

run_controller() {
  local plugin="$1"
  local actor
  actor="$(basename "$plugin")"
  actor="${actor%.wasm}"

  local actors status first_generation second_generation
  actors="$(ctl actors list)"
  [[ "$actors" == *"id=$actor kind=wasm-preview1-compat"* ]] || {
    echo "ERR Preview 1 actor was not daemon-reachable: $actors" >&2
    return 1
  }
  [[ "$actors" == *"latest_frame=none"* ]] || {
    echo "ERR Preview 1 actor incorrectly advertised a publication: $actors" >&2
    return 1
  }
  [[ "$(grep -c 'kind=wasm-preview1-compat' <<<"$actors")" == 1 ]] || {
    echo "ERR expected exactly one Preview 1 compatibility actor: $actors" >&2
    return 1
  }

  status="$(ctl status-json)"
  [[ "$(json_field "$status" preview1_compat_active)" == true ]] || {
    echo "ERR status-json did not report an active Preview 1 actor" >&2
    return 1
  }
  first_generation="$(json_field "$status" preview1_compat_generation)"
  echo "preview1-controller: first generation=$first_generation" >&2
  run_generation "$actor" Z 90

  # Replace the actor while two notification wait sets are suspended. The plug
  # command must not return until the old Stores and registrations are gone.
  ctl actors send "$actor" start-notify >/dev/null
  status="$(wait_field_at_least preview1_compat_wait_registrations 4 "old-generation notification waits")"
  [[ "$(json_field "$status" preview1_compat_workers_finished)" == 3 ]] || {
    echo "ERR old-generation notification workers were not pending at hot reload" >&2
    return 1
  }

  echo "preview1-controller: reload with pending old worker" >&2
  ctl plug "$plugin" >/dev/null
  echo "preview1-controller: reload returned" >&2
  for _ in $(seq 1 200); do
    status="$(ctl status-json)"
    second_generation="$(json_field "$status" preview1_compat_generation)"
    if [[ "$second_generation" != "$first_generation" && "$second_generation" != 0 ]]; then
      break
    fi
    sleep 0.05
  done
  [[ "$second_generation" != "$first_generation" && "$second_generation" != 0 ]] || {
    echo "ERR hot reload did not install a new actor generation" >&2
    return 1
  }
  [[ "$(json_field "$status" preview1_compat_mailbox_dispatched)" == 0 ]] || {
    echo "ERR replacement generation inherited mailbox counters" >&2
    return 1
  }
  [[ "$(json_field "$status" preview1_compat_wait_registrations)" == 0 ]] || {
    echo "ERR replacement inherited old wait registrations" >&2
    return 1
  }
  [[ "$(json_field "$status" preview1_compat_notifications)" == 0 ]] || {
    echo "ERR replacement inherited old notification resources" >&2
    return 1
  }
  actors="$(ctl actors list)"
  [[ "$(grep -c 'kind=wasm-preview1-compat' <<<"$actors")" == 1 ]] || {
    echo "ERR replacement did not leave exactly one compatibility actor: $actors" >&2
    return 1
  }
  run_generation "$actor" Q 81

  status="$(ctl status-json)"
  [[ "$(json_field "$status" plugin_actor_failures)" == 0 ]] || {
    echo "ERR daemon reported plugin actor failures: $status" >&2
    return 1
  }

  echo "preview1_nested_controller=ok first_generation=$first_generation second_generation=$second_generation first_worker_value=90 second_worker_value=81"
}

if [[ "${1:-}" == "--controller" ]]; then
  [[ $# -eq 2 ]] || {
    echo "usage: $0 --controller <plugin.wasm>" >&2
    exit 2
  }
  run_controller "$2"
  exit
fi

cd "$ROOT"
nix develop --command cargo build --quiet -p manifoldd --bins
(
  cd examples/wasm-preview1-worker
  # Cargo discovers target-specific stack/TLS export flags from this crate's
  # checked-in .cargo/config.toml only when invoked from the crate directory.
  nix develop "$ROOT" --command cargo build --quiet \
    --manifest-path Cargo.toml \
    --target wasm32-wasip1-threads \
    --release
)

GUEST="$ROOT/examples/wasm-preview1-worker/target/wasm32-wasip1-threads/release/manifold_preview1_worker_example.wasm"
OUT="${MANIFOLD_PREVIEW1_NESTED_OUT:-$(mktemp -d /tmp/manifold-preview1-nested.XXXXXX)}"
mkdir -p "$OUT"
cleanup() {
  local status="$1"
  if (( status == 0 )) && [[ -z "${MANIFOLD_PREVIEW1_NESTED_KEEP:-}" ]]; then
    rm -rf "$OUT"
  else
    echo "preview1_nested_out=$OUT" >&2
  fi
}
trap 'cleanup "$?"' EXIT

PLUGIN_DIR="$OUT/plugin"
mkdir -p "$PLUGIN_DIR"
PLUGIN="$PLUGIN_DIR/preview1-compat.wasm"
cp "$GUEST" "$PLUGIN"
printf 'Q-completion' > "$PLUGIN_DIR/completion.txt"

TEST_LOG="$OUT/test.log"
RUST_LOG="${RUST_LOG:-manifoldd=info,manifold_wasm=warn}" \
timeout --kill-after=5s 120s nix develop --command "$BIN" test nested \
  --force-outer \
  --inner-headless \
  --out-dir "$OUT/nested" \
  --inner-wasm-plugin "$PLUGIN" \
  --inner-command "$ROOT/tools/test-wasm-preview1-worker-nested.sh --controller $PLUGIN" \
  | tee "$TEST_LOG"

grep -qx 'topology=outer-supervisor' "$TEST_LOG"
grep -qx 'outer_presentation=offscreen' "$TEST_LOG"
grep 'preview1_nested_controller=ok' "$OUT/nested/inner.log"

echo "preview1_nested_workload=ok daemon_routing=true actor_progress=true completion_io=true shared_queue=true worker_wait_any=true notification_coalesced=true wait_teardown=true worker_read_compat=true hot_reload=true structured_shutdown=true"
