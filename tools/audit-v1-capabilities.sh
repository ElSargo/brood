#!/usr/bin/env bash
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

OUT="${MANIFOLD_AUDIT_OUT:-$(mktemp -d /tmp/manifold-v1-audit.XXXXXX)}"
mkdir -p "$OUT"
failures=0

run_check() {
  local name="$1"
  shift
  local log="$OUT/$name.log"
  if "$@" >"$log" 2>&1; then
    echo "check=$name result=pass log=$log"
  else
    local status=$?
    echo "check=$name result=fail rc=$status log=$log"
    tail -n 80 "$log" | sed 's/^/  /'
    failures=$((failures + 1))
  fi
}

echo "audit=v1_capabilities topology=external_client out=$OUT"

run_check cargo_fmt cargo fmt --all --check
run_check cargo_check cargo check --workspace --all-targets
run_check cargo_test cargo test --workspace --no-fail-fast
run_check headless_wayland_globals ./tools/smoke-headless-wayland-info.sh
run_check headless_native_client ./tools/smoke-headless-native-client.sh
run_check headless_control_errors ./tools/smoke-headless-control-errors.sh
run_check headless_status_parity ./tools/smoke-headless-status-parity.sh
run_check headless_client_exit ./tools/smoke-headless-client-exit.sh
run_check headless_multi_client ./tools/smoke-headless-multi-client.sh
run_check client_layout_actor ./tools/test-client-layout-actor.sh
run_check recursive_session_switcher ./tools/test-session-switcher-plugin-nested.sh
run_check software_e2e \
  ./tools/software-e2e-capture.sh \
  --out-dir "$OUT/software-e2e" \
  --manifold-bin target/debug/manifold \
  --client-bin target/debug/manifold-client

if ((failures > 0)); then
  echo "overall=fail failures=$failures out=$OUT"
  exit 1
fi

echo "overall=pass failures=0 out=$OUT"
