#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

TMPDIR="$(mktemp -d /tmp/manifold-v1-audit.XXXXXX)"
FAILURES=0
SKIPS=0
REQUIRE_NO_SKIPS="${MANIFOLD_AUDIT_REQUIRE_NO_SKIPS:-0}"
declare -A CHECK_RESULT
declare -A CHECK_RC
declare -A CHECK_LOG

case "$REQUIRE_NO_SKIPS" in
  0 | 1) ;;
  *)
    echo "ERR MANIFOLD_AUDIT_REQUIRE_NO_SKIPS must be 0 or 1" >&2
    exit 2
    ;;
esac

run_check() {
  local name="$1"
  shift
  local log="$TMPDIR/$name.log"
  local rc

  set +e
  "$@" > "$log" 2>&1
  rc=$?
  set -e

  case "$rc" in
    0)
      CHECK_RESULT["$name"]="pass"
      CHECK_RC["$name"]="$rc"
      CHECK_LOG["$name"]="$log"
      echo "check=$name result=pass rc=0 log=$log"
      ;;
    77)
      SKIPS=$((SKIPS + 1))
      CHECK_RESULT["$name"]="skip"
      CHECK_RC["$name"]="$rc"
      CHECK_LOG["$name"]="$log"
      echo "check=$name result=skip rc=77 log=$log"
      tail -n 8 "$log" | sed 's/^/  /'
      ;;
    *)
      FAILURES=$((FAILURES + 1))
      CHECK_RESULT["$name"]="fail"
      CHECK_RC["$name"]="$rc"
      CHECK_LOG["$name"]="$log"
      echo "check=$name result=fail rc=$rc log=$log"
      tail -n 40 "$log" | sed 's/^/  /'
      ;;
  esac
}

emit_requirement() {
  local id="$1"
  local result="$2"
  local evidence="$3"
  local note="${4:-none}"

  echo "requirement=$id result=$result evidence=$evidence note=$note"
}

aggregate_requirement_result() {
  local result=pass
  local check
  for check in "$@"; do
    case "${CHECK_RESULT[$check]:-fail}" in
      pass) ;;
      skip)
        if [[ "$result" == "pass" ]]; then
          result=skip
        fi
        ;;
      *)
        result=fail
        ;;
    esac
  done
  printf '%s' "$result"
}

aggregate_requirement_evidence() {
  local evidence=""
  local check
  for check in "$@"; do
    if [[ -n "$evidence" ]]; then
      evidence="${evidence},"
    fi
    evidence="${evidence}${check}:${CHECK_LOG[$check]:-none}"
  done
  printf '%s' "$evidence"
}

emit_aggregate_requirement() {
  local id="$1"
  local note="$2"
  shift 2
  local result
  local evidence

  result="$(aggregate_requirement_result "$@")"
  evidence="$(aggregate_requirement_evidence "$@")"
  emit_requirement "$id" "$result" "$evidence" "$note"
}

diagnostic_field() {
  local key="$1"
  local log="${CHECK_LOG[dmabuf_backend_diagnostic]:-}"
  if [[ -n "$log" && -f "$log" ]]; then
    sed -n "s/^${key}=//p" "$log" | tail -n 1
  fi
}

emit_capability_backend_summary() {
  local classification
  local dmabuf_formats
  local dmabuf_disabled
  local explicit_sync
  local explicit_sync_disabled
  local render_node
  local next

  classification="$(diagnostic_field classification)"
  dmabuf_formats="$(diagnostic_field dmabuf_formats)"
  dmabuf_disabled="$(diagnostic_field dmabuf_disabled)"
  explicit_sync="$(diagnostic_field explicit_sync)"
  explicit_sync_disabled="$(diagnostic_field explicit_sync_disabled)"
  render_node="$(diagnostic_field render_node)"
  next="$(diagnostic_field next)"

  echo "capability_backend_diagnostic=${CHECK_RESULT[dmabuf_backend_diagnostic]:-not_run} evidence=${CHECK_LOG[dmabuf_backend_diagnostic]:-none}"
  [[ -z "$classification" ]] || echo "capability_backend_classification=$classification"
  [[ -z "$dmabuf_formats" ]] || echo "capability_backend_dmabuf_formats=$dmabuf_formats"
  [[ -z "$dmabuf_disabled" ]] || echo "capability_backend_dmabuf_disabled=$dmabuf_disabled"
  [[ -z "$explicit_sync" ]] || echo "capability_backend_explicit_sync=$explicit_sync"
  [[ -z "$explicit_sync_disabled" ]] || echo "capability_backend_explicit_sync_disabled=$explicit_sync_disabled"
  [[ -z "$render_node" ]] || echo "capability_backend_render_node=$render_node"
  [[ -z "$next" ]] || echo "capability_backend_next=$next"
}

echo "audit=v1_capabilities require_no_skips=$REQUIRE_NO_SKIPS tmpdir=$TMPDIR"

run_check cargo_fmt cargo fmt --check
run_check cargo_check cargo check -p manifoldd --offline
run_check cargo_test cargo test -p manifoldd --offline
run_check headless_wayland_globals ./tools/smoke-headless-wayland-info.sh
run_check headless_native_client ./tools/smoke-headless-native-client.sh
run_check headless_input_forwarding ./tools/smoke-headless-input-forwarding.sh
run_check headless_control_errors ./tools/smoke-headless-control-errors.sh
run_check headless_status_json ./tools/smoke-headless-status-json.sh
run_check headless_status_parity ./tools/smoke-headless-status-parity.sh
run_check headless_client_exit ./tools/smoke-headless-client-exit.sh
run_check headless_multi_client ./tools/smoke-headless-multi-client.sh
run_check headless_multi_client_input_focus ./tools/smoke-headless-multi-client-input-focus.sh
run_check headless_attach_diagnostics ./tools/smoke-headless-attach-diagnostics.sh
run_check headless_frontend_close_idempotent ./tools/smoke-headless-frontend-close-idempotent.sh
run_check headless_repeated_attach_failure ./tools/smoke-headless-repeated-attach-failure.sh
run_check startup_attach_fallback ./tools/smoke-startup-attach-fallback.sh
run_check dmabuf_status_validator ./tools/test-dmabuf-status-validator.sh
run_check dmabuf_backend_classifier ./tools/test-dmabuf-backend-classifier.sh

DMABUF_VALIDATOR_RESULT="$(aggregate_requirement_result dmabuf_status_validator dmabuf_backend_classifier)"

HOST_PREFLIGHT_LOG="$TMPDIR/host_presentation_preflight.log"
if ! require_host_presentation_environment > "$HOST_PREFLIGHT_LOG" 2>&1; then
  echo "check=host_presentation_preflight result=blocked rc=1 log=$HOST_PREFLIGHT_LOG"
  sed 's/^/  /' "$HOST_PREFLIGHT_LOG"
  emit_aggregate_requirement rust_baseline "cargo fmt/check/test" \
    cargo_fmt cargo_check cargo_test
  emit_aggregate_requirement nested_wayland_globals "headless Wayland globals smoke" \
    headless_wayland_globals
  emit_aggregate_requirement native_wayland_client_nesting "headless native/status/exit/multi/attach diagnostics/startup fallback smokes" \
    headless_native_client headless_control_errors headless_status_json headless_status_parity headless_client_exit headless_multi_client headless_multi_client_input_focus headless_attach_diagnostics headless_frontend_close_idempotent headless_repeated_attach_failure startup_attach_fallback
  emit_aggregate_requirement input_forwarding "headless control-injected pointer/key/axis forwarding smoke" \
    headless_input_forwarding headless_multi_client_input_focus
  emit_requirement frontend_window_rendering blocked "$HOST_PREFLIGHT_LOG" "host display unavailable before full audit"
  emit_aggregate_requirement close_frontend_keep_client_alive "headless frontend-close idempotency and startup attach fallback; host close/resume unavailable" \
    headless_frontend_close_idempotent headless_repeated_attach_failure startup_attach_fallback
  emit_requirement reopen_frontend_resume_client blocked "$HOST_PREFLIGHT_LOG" "host display unavailable before full audit"
  emit_requirement low_latency_cadence blocked "$HOST_PREFLIGHT_LOG" "host display unavailable before full audit"
  emit_aggregate_requirement dmabuf_validator_logic "dmabuf status validator and backend classifier fixture tests" \
    dmabuf_status_validator dmabuf_backend_classifier
  emit_requirement dmabuf_advertisement_correctness blocked "$HOST_PREFLIGHT_LOG" "host display unavailable before full audit"
  emit_requirement positive_dmabuf_import blocked "$HOST_PREFLIGHT_LOG" "host display unavailable before full audit"
  emit_requirement positive_explicit_sync_fences blocked "$HOST_PREFLIGHT_LOG" "host display unavailable before full audit"
  if ((FAILURES > 0)); then
    echo "overall=fail failures=$FAILURES skips=$SKIPS host_display=unavailable tmpdir=$TMPDIR"
    exit 1
  fi
  echo "overall=blocked_host_display_unavailable failures=$FAILURES skips=$SKIPS tmpdir=$TMPDIR"
  exit 1
fi
echo "check=host_presentation_preflight result=pass rc=0 log=$HOST_PREFLIGHT_LOG"

run_check dmabuf_backend_diagnostic ./tools/diagnose-dmabuf-backend.sh
run_check host_nest_resume ./tools/smoke-manifoldd.sh
run_check host_input_forwarding ./tools/smoke-input-forwarding.sh
run_check host_status_json ./tools/smoke-status-json.sh
run_check dmabuf_diagnostics ./tools/smoke-dmabuf-diagnostics.sh
run_check explicit_sync_diagnostics ./tools/smoke-explicit-sync-diagnostics.sh
run_check latency_cadence ./tools/smoke-latency-cadence.sh
run_check dmabuf_positive ./tools/validate-dmabuf-positive.sh
run_check explicit_sync_positive env MANIFOLD_DMABUF_REQUIRE_EXPLICIT_SYNC=1 ./tools/validate-dmabuf-positive.sh

emit_aggregate_requirement rust_baseline "cargo fmt/check/test" \
  cargo_fmt cargo_check cargo_test
emit_aggregate_requirement nested_wayland_globals "headless Wayland globals smoke" \
  headless_wayland_globals
emit_aggregate_requirement native_wayland_client_nesting "headless native/status/exit/multi/attach/startup smokes; host nest/detach/reattach" \
  headless_native_client headless_control_errors headless_status_json headless_status_parity headless_client_exit headless_multi_client headless_multi_client_input_focus headless_attach_diagnostics headless_frontend_close_idempotent headless_repeated_attach_failure startup_attach_fallback host_nest_resume
emit_aggregate_requirement frontend_window_rendering "host nest/detach/reattach; status-json" \
  host_nest_resume host_status_json
emit_aggregate_requirement input_forwarding "headless control injection; host control injection" \
  headless_input_forwarding headless_multi_client_input_focus host_input_forwarding
emit_aggregate_requirement close_frontend_keep_client_alive "headless frontend-close idempotency and startup attach fallback" \
  headless_frontend_close_idempotent headless_repeated_attach_failure startup_attach_fallback
emit_aggregate_requirement reopen_frontend_resume_client "host nest/detach/reattach smoke" \
  host_nest_resume
emit_aggregate_requirement low_latency_cadence "latency/cadence smoke lifecycle budgets" \
  latency_cadence
emit_aggregate_requirement dmabuf_validator_logic "dmabuf status validator and backend classifier fixture tests" \
  dmabuf_status_validator dmabuf_backend_classifier
emit_aggregate_requirement dmabuf_advertisement_correctness "dmabuf diagnostics smoke; explicit sync diagnostics smoke" \
  dmabuf_diagnostics explicit_sync_diagnostics
emit_requirement positive_dmabuf_import "${CHECK_RESULT[dmabuf_positive]:-fail}" "${CHECK_LOG[dmabuf_positive]:-none}" "validate-dmabuf-positive"
emit_requirement positive_explicit_sync_fences "${CHECK_RESULT[explicit_sync_positive]:-fail}" "${CHECK_LOG[explicit_sync_positive]:-none}" "MANIFOLD_DMABUF_REQUIRE_EXPLICIT_SYNC=1 validate-dmabuf-positive"

if ((FAILURES > 0)); then
  echo "overall=fail failures=$FAILURES skips=$SKIPS tmpdir=$TMPDIR"
  exit 1
fi

if ((SKIPS > 0)) && [[ "$REQUIRE_NO_SKIPS" == "1" ]]; then
  echo "capability_completion=incomplete_backend_capability_skips skips=$SKIPS"
  emit_capability_backend_summary
  echo "overall=fail_capability_skips_disallowed failures=0 skips=$SKIPS tmpdir=$TMPDIR"
  exit 1
fi

if ((SKIPS > 0)); then
  echo "capability_completion=incomplete_backend_capability_skips skips=$SKIPS"
  emit_capability_backend_summary
  echo "overall=pass_with_capability_skips failures=0 skips=$SKIPS tmpdir=$TMPDIR"
else
  echo "capability_completion=complete skips=0"
  emit_capability_backend_summary
  echo "overall=pass failures=0 skips=0 tmpdir=$TMPDIR"
fi
