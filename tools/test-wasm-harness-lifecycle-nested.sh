#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug/manifold"
HARNESS="$ROOT/examples/wasm-harness-lifecycle-proof/target/wasm32-unknown-unknown/release/manifold_wasm_harness_lifecycle_proof.wasm"
OUT="${MANIFOLD_HARNESS_LIFECYCLE_OUT:-$(mktemp -d /tmp/manifold-harness-lifecycle.XXXXXX)}"
ACTIVE_PID=
ACTIVE_DESTINATION=
ACTIVE_INNER_DESTINATION=

cleanup() {
  if [[ -n "$ACTIVE_INNER_DESTINATION" ]]; then
    timeout 5s "$BIN" ctl --dbus-destination "$ACTIVE_INNER_DESTINATION" quit >/dev/null 2>&1 || true
  fi
  if [[ -n "$ACTIVE_DESTINATION" ]]; then
    timeout 5s "$BIN" ctl --dbus-destination "$ACTIVE_DESTINATION" quit >/dev/null 2>&1 || true
  fi
  if [[ -n "$ACTIVE_PID" ]] && kill -0 "$ACTIVE_PID" 2>/dev/null; then
    for _ in $(seq 1 100); do
      kill -0 "$ACTIVE_PID" 2>/dev/null || break
      sleep 0.05
    done
    if kill -0 "$ACTIVE_PID" 2>/dev/null; then
      kill "$ACTIVE_PID" 2>/dev/null || true
    fi
  fi
  if [[ -n "$ACTIVE_PID" ]]; then
    wait "$ACTIVE_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

rm -rf "$OUT"
mkdir -p "$OUT"

nix develop --command cargo build --quiet -p manifoldd --bins
nix develop --command cargo build --quiet --release \
  --manifest-path examples/wasm-harness-lifecycle-proof/Cargo.toml \
  --target wasm32-unknown-unknown

ctl() {
  timeout 5s "$BIN" ctl --dbus-destination "$ACTIVE_DESTINATION" "$@"
}

wait_for_harness_ready() {
  local log=$1
  local supervisor=$2
  local status
  for _ in $(seq 1 300); do
    if [[ -z "$ACTIVE_DESTINATION" && -f "$log" ]]; then
      ACTIVE_DESTINATION="$(sed -n 's/^MANIFOLDD_DBUS_DESTINATION=//p' "$log" | head -n1)"
      if [[ -n "$ACTIVE_DESTINATION" ]]; then
        ACTIVE_INNER_DESTINATION="${ACTIVE_DESTINATION%.outer}.inner"
      fi
    fi
    if [[ -n "$ACTIVE_DESTINATION" ]] && grep -q '^OK nested test ready$' "$supervisor" 2>/dev/null && \
      status="$(ctl status-json 2>/dev/null)" && \
      python3 -c 'import json,sys; raise SystemExit(json.loads(sys.argv[1])["windows"] != 3)' "$status"; then
      return 0
    fi
    if ! kill -0 "$ACTIVE_PID" 2>/dev/null; then
      echo "nested harness process exited before readiness" >&2
      tail -120 "$log" >&2 || true
      return 1
    fi
    sleep 0.05
  done
  echo "timed out waiting for harness graphical readiness" >&2
  tail -120 "$log" >&2 || true
  return 1
}

wait_for_hosted_cleanup() {
  local status
  for _ in $(seq 1 200); do
    status="$(ctl status-json)"
    if python3 -c 'import json,sys; raise SystemExit(json.loads(sys.argv[1])["hosted_children"] != 0)' "$status"; then
      return 0
    fi
    sleep 0.05
  done
  echo 'timed out waiting for nested graphical children to exit' >&2
  return 1
}

run_case() {
  local name=$1
  local action=$2
  local case_dir="$OUT/$name"
  local harness_panes
  mkdir -p "$case_dir"

  nix develop --command env RUST_LOG=manifoldd::test_harness=info "$BIN" test nested \
    --force-outer \
    --out-dir "$case_dir/nested" \
    --harness "$HARNESS" \
    >"$case_dir/supervisor.stdout" 2>&1 &
  ACTIVE_PID=$!
  wait_for_harness_ready "$case_dir/nested/outer.log" "$case_dir/supervisor.stdout"
  harness_panes="$(sed -n 's/.*child handle .*pane_id=\([0-9][0-9]*\).*/\1/p' "$case_dir/nested/outer.log")"
  [[ "$(wc -w <<<"$harness_panes")" == 2 ]]

  ctl debug dump "$case_dir/controller-dump" | tee "$case_dir/dump-response.txt"
  while read -r harness_pane; do
    ctl client close "$harness_pane" | tee -a "$case_dir/client-close-response.txt"
  done <<<"$harness_panes"
  ctl nested-inner quit | tee "$case_dir/inner-quit-response.txt"
  wait_for_hosted_cleanup

  if [[ "$action" == complete ]]; then
    ctl harness signal capture-and-finish | tee "$case_dir/signal-response.txt"
  else
    ctl quit | tee "$case_dir/quit-response.txt"
  fi

  wait "$ACTIVE_PID"
  ACTIVE_PID=
  ACTIVE_DESTINATION=
  ACTIVE_INNER_DESTINATION=

  if [[ "$action" == complete ]]; then
    grep -q 'TEST_HARNESS_RESULT kind=Passed exit_code=0' "$case_dir/nested/outer.log"
    grep -Eq 'TEST_HARNESS_CLEANUP Joined .*cancellation_requested: false' "$case_dir/nested/outer.log"
    grep -q 'observed release signal and returning pass' "$case_dir/nested/outer.log"
  else
    grep -Eq 'TEST_HARNESS_CLEANUP Joined .*cancellation_requested: true' "$case_dir/nested/outer.log"
    if grep -q 'TEST_HARNESS_RESULT' "$case_dir/nested/outer.log"; then
      echo 'cancelled harness incorrectly reported guest completion' >&2
      return 1
    fi
  fi
}

run_case completed complete
run_case cancelled cancel

python3 - "$OUT" <<'PY'
import json
import pathlib
import sys

out = pathlib.Path(sys.argv[1])
for name in ("completed", "cancelled"):
    dump = out / name / "controller-dump"
    state = json.loads((dump / "state.json").read_text())
    png = (dump / "composed.png").read_bytes()
    assert state["status"]["windows"] == 3, (name, state["status"])
    panes = [item for item in state["layout"]["items"] if item["kind"].get("type") == "pane"]
    assert len(panes) == 3, (name, panes)
    assert png.startswith(b"\x89PNG\r\n\x1a\n") and len(png) > 4096, (name, len(png))
PY

for screenshot in \
  "$OUT/completed/controller-dump/composed.png" \
  "$OUT/cancelled/controller-dump/composed.png"; do
  test "$(magick "$screenshot" -format '%k' info:)" -gt 50
done

{
  echo '# Typed WASM harness lifecycle proof'
  echo
  echo 'The same core-WASM harness controlled two real offscreen nested Manifold runs.'
  echo 'In both runs it spawned an animated Weston EGL client and interactive terminal beside the nested presentation, then blocked on a named host signal.'
  echo
  echo '## Owned completion'
  echo
  echo '- The host delivered `capture-and-finish` through the bounded signal lane.'
  echo '- The controller captured the live three-pane frame, then explicitly closed every graphical child domain before releasing the harness.'
  echo '- The guest observed one final real status request and returned pass.'
  grep -E 'TEST_HARNESS_(RESULT|CLEANUP)' "$OUT/completed/nested/outer.log" | sed 's/^/- `/' | sed 's/$/`/'
  echo
  echo '## Cancellation while blocked'
  echo
  echo '- The controller captured the still-live three-pane frame, then quit the supervisor without releasing the guest signal wait.'
  echo '- Teardown woke the blocked import, interrupted guest execution, joined the exact child, and emitted no fabricated guest completion.'
  grep -E 'TEST_HARNESS_CLEANUP' "$OUT/cancelled/nested/outer.log" | sed 's/^/- `/' | sed 's/$/`/'
  echo
  printf 'Completed screenshot: `%s`\n' "$OUT/completed/controller-dump/composed.png"
  printf 'Cancelled-while-blocked screenshot: `%s`\n' "$OUT/cancelled/controller-dump/composed.png"
} >"$OUT/proof.md"

cat "$OUT/proof.md"
