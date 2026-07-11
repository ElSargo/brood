#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug/manifold"

ctl() {
  RUST_LOG=off timeout 20s "$BIN" ctl "$@"
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

actor_counter() {
  local actors="$1"
  local actor_id="$2"
  local line value
  line="$(awk -v id="id=$actor_id" '$1 == "actor" && $2 == id { print; exit }' <<<"$actors")"
  [[ -n "$line" ]] || return 1
  value="${line#* enqueued_messages=}"
  printf '%s\n' "${value%% *}"
}

wait_actor_counter() {
  local actor_id="$1"
  local expected="$2"
  local label="$3"
  local actors value
  for _ in $(seq 1 200); do
    actors="$(ctl actors list)"
    value="$(actor_counter "$actors" "$actor_id" || true)"
    if [[ "$value" =~ ^[0-9]+$ ]] && (( value >= expected )); then
      printf '%s' "$actors"
      return 0
    fi
    sleep 0.05
  done
  echo "ERR timed out waiting for $label ($actor_id enqueued_messages >= $expected)" >&2
  ctl actors list >&2 || true
  return 1
}

wait_field() {
  local field="$1"
  local expected="$2"
  local comparison="$3"
  local label="$4"
  local status value
  for _ in $(seq 1 200); do
    status="$(ctl status-json)"
    value="$(json_field "$status" "$field")"
    if [[ "$comparison" == "at-least" ]] && (( value >= expected )); then
      printf '%s' "$status"
      return 0
    fi
    if [[ "$comparison" == "equals" && "$value" == "$expected" ]]; then
      printf '%s' "$status"
      return 0
    fi
    sleep 0.05
  done
  echo "ERR timed out waiting for $label ($field $comparison $expected)" >&2
  ctl status-json >&2 || true
  return 1
}

assert_registry() {
  local actors="$1"
  local component_count preview_count actor_id kind count
  local -a component_ids=(registry-component-0 registry-component-1 registry-component-2 registry-component-3)
  local -a preview_ids=(registry-preview-0 registry-preview-1 registry-preview-2 registry-preview-3)

  for actor_id in "${component_ids[@]}"; do
    kind="wasm-component"
    count="$(awk -v id="id=$actor_id" -v kind="kind=$kind" \
      '$1 == "actor" && $2 == id && $3 == kind { count += 1 } END { print count + 0 }' \
      <<<"$actors")"
    [[ "$count" == 1 ]] || {
      echo "ERR registry expected exactly one $kind actor id=$actor_id" >&2
      echo "$actors" >&2
      return 1
    }
  done
  for actor_id in "${preview_ids[@]}"; do
    kind="wasm-preview1-compat"
    count="$(awk -v id="id=$actor_id" -v kind="kind=$kind" \
      '$1 == "actor" && $2 == id && $3 == kind { count += 1 } END { print count + 0 }' \
      <<<"$actors")"
    [[ "$count" == 1 ]] || {
      echo "ERR registry expected exactly one $kind actor id=$actor_id" >&2
      echo "$actors" >&2
      return 1
    }
  done

  component_count="$(awk '$1 == "actor" && $3 == "kind=wasm-component" { count += 1 } END { print count + 0 }' <<<"$actors")"
  preview_count="$(awk '$1 == "actor" && $3 == "kind=wasm-preview1-compat" { count += 1 } END { print count + 0 }' <<<"$actors")"
  [[ "$component_count" == 4 && "$preview_count" == 4 ]] || {
    echo "ERR registry expected exactly four actors of each hosted I/O kind" >&2
    echo "$actors" >&2
    return 1
  }
}

run_controller() {
  local component="$1"
  local preview="$2"
  local legacy="$3"
  local actors actor_id status value expected invalid replacement_before
  local replaced="registry-preview-0"
  local -a component_ids=(registry-component-0 registry-component-1 registry-component-2 registry-component-3)
  local -a preview_ids=(registry-preview-0 registry-preview-1 registry-preview-2 registry-preview-3)
  local -a all_ids=("${component_ids[@]}" "${preview_ids[@]}")
  declare -A before_replace=()

  for actor_id in "${component_ids[@]}"; do
    ctl plug --id "$actor_id" "$component" >/dev/null
  done
  for actor_id in "${preview_ids[@]}"; do
    ctl plug --id "$actor_id" "$preview" >/dev/null
  done

  actors="$(ctl actors list)"
  assert_registry "$actors"

  # Every actor receives an independently accounted mailbox admission.
  for actor_id in "${all_ids[@]}"; do
    ctl actors send "$actor_id" registry-probe >/dev/null
    wait_actor_counter "$actor_id" 1 "initial registry probe" >/dev/null
  done

  # Suspend real workers in one Preview generation. While it is waiting, all
  # seven peers must continue accepting their own actor mailbox traffic.
  ctl actors send "$replaced" start-notify >/dev/null
  wait_field preview1_compat_wait_registrations 4 at-least \
    "old Preview generation worker waits" >/dev/null
  for actor_id in "${all_ids[@]}"; do
    [[ "$actor_id" == "$replaced" ]] && continue
    ctl actors send "$actor_id" pre-replace-progress >/dev/null
    wait_actor_counter "$actor_id" 2 "peer progress before replacement" >/dev/null
  done

  actors="$(ctl actors list)"
  for actor_id in "${all_ids[@]}"; do
    [[ "$actor_id" == "$replaced" ]] && continue
    before_replace["$actor_id"]="$(actor_counter "$actors" "$actor_id")"
  done

  # A malformed candidate must fail before the healthy generation is removed.
  invalid="$(mktemp /tmp/manifold-invalid-replacement.XXXXXX.wasm)"
  printf 'not-a-wasm-module\n' >"$invalid"
  replacement_before="$(actor_counter "$actors" "$replaced")"
  if ctl plug --id "$replaced" "$invalid" >"$invalid.out" 2>"$invalid.err"; then
    echo "ERR malformed replacement unexpectedly loaded" >&2
    rm -f "$invalid" "$invalid.out" "$invalid.err"
    return 1
  fi
  rm -f "$invalid" "$invalid.out" "$invalid.err"
  ctl actors send "$replaced" failed-replacement-progress >/dev/null
  wait_actor_counter "$replaced" "$((replacement_before + 1))" \
    "healthy generation after rejected replacement" >/dev/null

  # This command returns only after the old actor's workers, wait sets, and
  # Store have joined. It replaces one map entry, not the other seven.
  ctl plug --id "$replaced" "$preview" >/dev/null
  actors="$(ctl actors list)"
  assert_registry "$actors"
  wait_field preview1_compat_wait_registrations 0 equals \
    "old Preview wait teardown" >/dev/null
  wait_field preview1_compat_notifications 0 equals \
    "old Preview notification teardown" >/dev/null
  wait_field preview1_compat_workers_active 0 equals \
    "old Preview worker teardown" >/dev/null

  value="$(actor_counter "$actors" "$replaced")"
  [[ "$value" == 0 ]] || {
    echo "ERR replacement actor inherited mailbox count $value" >&2
    return 1
  }

  for actor_id in "${all_ids[@]}"; do
    [[ "$actor_id" == "$replaced" ]] && continue
    value="$(actor_counter "$actors" "$actor_id")"
    expected="${before_replace[$actor_id]}"
    (( value >= expected )) || {
      echo "ERR peer $actor_id regressed from $expected to $value during replacement" >&2
      return 1
    }
    ctl actors send "$actor_id" post-replace-progress >/dev/null
    wait_actor_counter "$actor_id" "$((expected + 1))" \
      "peer progress after replacement" >/dev/null
  done
  ctl actors send "$replaced" replacement-probe >/dev/null
  wait_actor_counter "$replaced" 1 "replacement generation progress" >/dev/null

  # Legacy core artifacts keep their declared singular namespace. Test this
  # after the replacement counter proof because the workspace policy emits
  # ordinary host events which are legitimately broadcast to hosted actors.
  ctl plug "$legacy" >/dev/null
  if ctl plug --id legacy-false-id "$legacy" >/dev/null 2>&1; then
    echo "ERR legacy core plugin accepted an unsupported custom actor id" >&2
    return 1
  fi

  status="$(ctl status-json)"
  [[ "$(json_field "$status" preview1_compat_active)" == true ]] || {
    echo "ERR aggregate status did not report Preview actors" >&2
    return 1
  }
  [[ "$(json_field "$status" preview1_compat_generation)" == 0 ]] || {
    echo "ERR aggregate status claimed one generation for four Preview actors" >&2
    return 1
  }
  [[ "$(json_field "$status" plugin_actor_failures)" == 0 ]] || {
    echo "ERR daemon reported plugin actor failures" >&2
    echo "$status" >&2
    return 1
  }

  actors="$(ctl actors list)"
  assert_registry "$actors"
  echo "wasm_io_registry_controller=ok components=4 preview1=4 legacy_id_rejection=true replacement=$replaced rejected_candidate_preserved_old=true peer_progress=7"
}

if [[ "${1:-}" == "--controller" ]]; then
  [[ $# -eq 4 ]] || {
    echo "usage: $0 --controller <component.wasm> <preview1.wasm> <legacy.wasm>" >&2
    exit 2
  }
  run_controller "$2" "$3" "$4"
  exit
fi

cd "$ROOT"
nix develop --command cargo build --quiet -p manifoldd --bins
WASM_MODULES="$(nix build --no-link --print-out-paths .#wasm-examples | tail -n1)"
COMPONENT="$WASM_MODULES/examples/wasm-component-actor/target/wasm32-unknown-unknown/release/manifold_component_actor_example.component.wasm"
PREVIEW1="$WASM_MODULES/examples/wasm-preview1-worker/target/wasm32-wasip1-threads/release/manifold_preview1_worker_example.wasm"
LEGACY="$WASM_MODULES/plugins/default-workspace/target/wasm32-unknown-unknown/release/manifold_default_workspace_plugin.wasm"

[[ -s "$COMPONENT" ]] || {
  echo "ERR packaged Component guest missing at $COMPONENT" >&2
  exit 1
}
[[ -s "$PREVIEW1" ]] || {
  echo "ERR packaged Preview 1 guest missing at $PREVIEW1" >&2
  exit 1
}
[[ -s "$LEGACY" ]] || {
  echo "ERR packaged legacy workspace plugin missing at $LEGACY" >&2
  exit 1
}

OUT="${MANIFOLD_WASM_IO_REGISTRY_OUT:-$(mktemp -d /tmp/manifold-wasm-io-registry.XXXXXX)}"
mkdir -p "$OUT"
OUT="$(realpath "$OUT")"
cleanup() {
  local status="$1"
  if (( status == 0 )) && [[ -z "${MANIFOLD_WASM_IO_REGISTRY_KEEP:-}" ]]; then
    rm -rf "$OUT"
  else
    echo "wasm_io_registry_out=$OUT" >&2
  fi
}
trap 'cleanup "$?"' EXIT

TEST_LOG="$OUT/test.log"
RUST_LOG="${RUST_LOG:-manifoldd=info,manifold_wasm=warn}" \
timeout --kill-after=10s 240s nix develop --command "$BIN" test nested \
  --force-outer \
  --inner-headless \
  --out-dir "$OUT/nested" \
  --inner-command "$ROOT/tools/test-wasm-io-registry-nested.sh --controller $COMPONENT $PREVIEW1 $LEGACY" \
  | tee "$TEST_LOG"

grep -qx 'topology=outer-supervisor' "$TEST_LOG"
grep -qx 'outer_presentation=offscreen' "$TEST_LOG"
grep 'wasm_io_registry_controller=ok' "$OUT/nested/inner.log"

echo "wasm_io_registry_nested=ok actors=8 component=4 preview1=4 legacy_id_rejection=true mailbox_probes=true failed_replacement_preserved_old=true isolated_replacement=true structured_shutdown=true"
