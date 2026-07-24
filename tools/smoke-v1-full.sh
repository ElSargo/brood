#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
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
run_step "headless control error smoke" ./tools/smoke-headless-control-errors.sh
run_step "headless status parity smoke" ./tools/smoke-headless-status-parity.sh
run_step "headless client-exit smoke" ./tools/smoke-headless-client-exit.sh
run_step "headless multi-client smoke" ./tools/smoke-headless-multi-client.sh
run_step "external client layout actor proof" ./tools/test-client-layout-actor.sh
run_step "recursive session-switcher proof" ./tools/test-session-switcher-plugin-nested.sh

SOFTWARE_E2E_OUT="$(mktemp -d /tmp/manifold-software-e2e.XXXXXX)"
run_step "external client software-rendering proof" \
  ./tools/software-e2e-capture.sh \
  --out-dir "$SOFTWARE_E2E_OUT" \
  --manifold-bin target/debug/manifold \
  --client-bin target/debug/manifold-client

printf '\nOK v1 full smoke suite passed\n'
printf 'software_e2e_out=%s\n' "$SOFTWARE_E2E_OUT"
