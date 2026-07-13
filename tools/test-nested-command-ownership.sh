#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug/manifold"

ctl() {
  RUST_LOG=off timeout 5s "$BIN" ctl "$@"
}

if [[ "${MANIFOLD_ORCHESTRATION_TERMINAL:-0}" == "1" ]]; then
  printf '\033[2J\033[H'
  echo 'Manifold nested command ownership'
  echo
  echo 'This real Weston terminal belongs to the tracked controller.'
  echo 'An unrelated hosted child may exit without completing that controller.'
  echo
  echo 'Capturing the rendered surface before a clean, owned exit...'
  printf 'terminal_rendered=true\n' >>"$MANIFOLD_ORCHESTRATION_REPORT"
  sleep 4
  exit 0
fi

run_controller() {
  local out="$1"
  local report="$out/command-ownership.md"
  local dump="$out/inner-visible"
  local terminal_pid before_windows status spawn_response unrelated_pane unrelated_status

  mkdir -p "$out"
  status="$(ctl status-json)"
  before_windows="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["windows"])' "$status")"
  {
    echo '# Nested command ownership proof'
    echo
    echo 'The tracked controller remained alive while its real Weston terminal mapped and while a different hosted child exited.'
    echo
    echo '```text'
    echo "controller_started=true"
    echo "windows_before=$before_windows"
  } >"$report"

  MANIFOLD_ORCHESTRATION_TERMINAL=1 \
    MANIFOLD_ORCHESTRATION_REPORT="$report" \
    weston-terminal --shell="$ROOT/tools/test-nested-command-ownership.sh" &
  terminal_pid=$!

  for _ in $(seq 1 100); do
    status="$(ctl status-json)"
    python3 -c 'import json,sys; p=json.loads(sys.argv[1]); raise SystemExit(0 if p["windows"] > int(sys.argv[2]) and p["frames"] >= 2 else 1)' \
      "$status" "$before_windows" && break
    sleep 0.05
  done
  python3 -c 'import json,sys; p=json.loads(sys.argv[1]); assert p["windows"] > int(sys.argv[2]) and p["frames"] >= 2, p' \
    "$status" "$before_windows"
  echo 'tracked_window_visible=true' >>"$report"

  spawn_response="$(ctl spawn bash -lc true)"
  unrelated_pane="$(sed -n 's/.*pane_id=\([0-9][0-9]*\).*/\1/p' <<<"$spawn_response")"
  test -n "$unrelated_pane"
  for _ in $(seq 1 100); do
    unrelated_status="$(ctl hosted-child-status "$unrelated_pane")"
    python3 -c 'import json,sys; p=json.loads(sys.argv[1]); raise SystemExit(0 if p["state"] == "exited" and p["success"] else 1)' \
      "$unrelated_status" && break
    sleep 0.05
  done
  python3 -c 'import json,sys; p=json.loads(sys.argv[1]); assert p["state"] == "exited" and p["success"], p' \
    "$unrelated_status"
  {
    echo "unrelated_spawn=$spawn_response"
    echo "unrelated_status=$unrelated_status"
    echo 'controller_alive_after_unrelated_exit=true'
  } >>"$report"

  ctl debug dump "$dump" >/dev/null
  test -s "$dump/composed.png"
  test -s "$dump/state.json"
  wait "$terminal_pid"
  {
    echo 'terminal_exit=0'
    echo 'controller_completed=true'
    echo '```'
    echo
    echo "The rendered terminal is captured in \`$dump/composed.png\`."
  } >>"$report"
  echo "nested_command_controller=complete report=$report screenshot=$dump/composed.png"
}

if [[ "${1:-}" == "--controller" ]]; then
  run_controller "$2"
  exit 0
fi

OUT="${MANIFOLD_NESTED_COMMAND_OUT:-$(mktemp -d /tmp/manifold-nested-command.XXXXXX)}"
rm -rf "$OUT"
mkdir -p "$OUT"

nix develop --command cargo build -p manifoldd
timeout --kill-after=5s 120s nix develop --command "$BIN" test nested \
  --force-outer \
  --out-dir "$OUT/nested" \
  --wait-for-inner-command-exit \
  --inner-command "$ROOT/tools/test-nested-command-ownership.sh --controller $OUT/proof" \
  | tee "$OUT/nested-test.stdout"

grep -q '^inner_command_visible=true$' "$OUT/nested-test.stdout"
grep -q '^inner_command_completed=true$' "$OUT/nested-test.stdout"
grep -q '^controller_alive_after_unrelated_exit=true$' "$OUT/proof/command-ownership.md"
grep -q '^controller_completed=true$' "$OUT/proof/command-ownership.md"
test -s "$OUT/proof/inner-visible/composed.png"
test -s "$OUT/nested/outer-dump/composed.png"

echo "nested_command_ownership=verified report=$OUT/proof/command-ownership.md screenshot=$OUT/proof/inner-visible/composed.png"
