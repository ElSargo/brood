#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

require_host_presentation_environment

run_step() {
  local name="$1"
  shift
  printf '\n== %s ==\n' "$name"
  "$@"
}

run_step "cargo fmt" cargo fmt --check
run_step "cargo check" cargo check -p manifoldd --offline
run_step "cargo test" cargo test -p manifoldd --offline
run_step "dmabuf status validator fixtures" ./tools/test-dmabuf-status-validator.sh
run_step "dmabuf backend classifier fixtures" ./tools/test-dmabuf-backend-classifier.sh

run_step "headless Wayland globals smoke" ./tools/smoke-headless-wayland-info.sh
run_step "headless native-client smoke" ./tools/smoke-headless-native-client.sh
run_step "headless input forwarding smoke" ./tools/smoke-headless-input-forwarding.sh
run_step "headless control error smoke" ./tools/smoke-headless-control-errors.sh
run_step "headless status-json smoke" ./tools/smoke-headless-status-json.sh
run_step "headless status parity smoke" ./tools/smoke-headless-status-parity.sh
run_step "headless client-exit smoke" ./tools/smoke-headless-client-exit.sh
run_step "headless multi-client smoke" ./tools/smoke-headless-multi-client.sh
run_step "headless multi-client input focus smoke" ./tools/smoke-headless-multi-client-input-focus.sh
run_step "headless attach diagnostics smoke" ./tools/smoke-headless-attach-diagnostics.sh
run_step "headless frontend-close idempotency smoke" ./tools/smoke-headless-frontend-close-idempotent.sh
run_step "headless repeated attach-failure smoke" ./tools/smoke-headless-repeated-attach-failure.sh
run_step "startup attach fallback smoke" ./tools/smoke-startup-attach-fallback.sh
run_step "nest/detach/reattach smoke" ./tools/smoke-manifoldd.sh
run_step "input forwarding smoke" ./tools/smoke-input-forwarding.sh
run_step "status-json smoke" ./tools/smoke-status-json.sh
run_step "dmabuf diagnostics smoke" ./tools/smoke-dmabuf-diagnostics.sh
run_step "explicit sync diagnostics smoke" ./tools/smoke-explicit-sync-diagnostics.sh
run_step "latency/cadence smoke" ./tools/smoke-latency-cadence.sh

printf '\nOK v1 full smoke suite passed\n'
