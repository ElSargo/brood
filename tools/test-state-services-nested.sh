#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug/manifold"

ctl() {
  RUST_LOG=off timeout 5s "$BIN" ctl "$@"
}

start_session_client() {
  local unit="$1"
  local session="$2"
  local workdir="$3"
  shift 3
  systemd-run --user --quiet --collect \
    --unit="$unit" \
    --working-directory="$workdir" \
    --setenv="WAYLAND_DISPLAY=$WAYLAND_DISPLAY" \
    --setenv="MANIFOLD_SURFACE_SESSION_ID=$session" \
    --setenv="MANIFOLDD_DBUS_ADDRESS=$MANIFOLDD_DBUS_ADDRESS" \
    --setenv="MANIFOLDD_DBUS_DESTINATION=$MANIFOLDD_DBUS_DESTINATION" \
    --setenv="PATH=$PATH" \
    --setenv="LD_LIBRARY_PATH=${LD_LIBRARY_PATH:-}" \
    --setenv="GI_TYPELIB_PATH=${GI_TYPELIB_PATH:-}" \
    --setenv="XDG_DATA_DIRS=${XDG_DATA_DIRS:-}" \
    --setenv="GDK_PIXBUF_MODULE_FILE=${GDK_PIXBUF_MODULE_FILE:-}" \
    "$@"
}

cleanup_state_service_clients() {
  systemctl --user stop "${STATE_SERVICE_CLIENT_UNITS[@]}" 2>/dev/null || true
}

run_controller() {
  if [[ "${MANIFOLD_STATE_SERVICES_TRACE:-0}" == "1" ]]; then
    set -x
  fi
  local workspace="$1"
  local ready="$workspace/.manifold/session-switcher-ready.log"
  local events="$workspace/.manifold/session-switcher-events.log"
  local dump="$workspace/.manifold/state-services-dump"
  local proof="$workspace/.manifold/state-services-proof.md"
  local alpha_json alpha_id beta_json beta_id rename_result
  local switch_alpha switch_beta switch_alpha_final close_beta alpha_default beta_default final_json
  local register agent_id notify agents status alpha_unit beta_unit switcher_unit
  local terminal_bin python_bin env_bin outer_destination

  alpha_unit="manifold-state-alpha-$$"
  beta_unit="manifold-state-beta-$$"
  switcher_unit="manifold-state-switcher-$$"
  terminal_bin="$(command -v weston-terminal)"
  python_bin="$(command -v python3)"
  env_bin="$(command -v env)"
  outer_destination="${MANIFOLDD_DBUS_DESTINATION%.inner}.outer"
  STATE_SERVICE_CLIENT_UNITS=("$switcher_unit" "$beta_unit" "$alpha_unit")
  trap cleanup_state_service_clients EXIT

  mkdir -p "$workspace/.manifold" "$workspace/alpha" "$workspace/beta"
  ctl surface-session dir-new "$workspace/alpha" default "Alpha Workspace" >/dev/null
  alpha_json="$(ctl --json '["surface-session","list-json"]')"
  alpha_id="$(python3 -c 'import json,sys; payload=json.loads(sys.argv[1]); matches=[s for s in payload["sessions"] if s["name"]=="Alpha Workspace"]; assert len(matches)==1, payload; print(matches[0]["id"])' "$alpha_json")"

  # Start clients as transient user services. This breaks ancestry with the
  # hosted controller (which owns its own pending pane) and keeps client exits
  # outside the nested supervisor's hosted-child completion accounting.
  start_session_client "$alpha_unit" "$alpha_id" "$workspace/alpha" \
    "$terminal_bin"
  for _ in $(seq 1 100); do
    alpha_json="$(ctl --json '["surface-session","list-json"]')"
    python3 -c 'import json,sys; p=json.loads(sys.argv[1]); target=int(sys.argv[2]); raise SystemExit(0 if any(s["id"]==target and s["windows"] >= 1 for s in p["sessions"]) else 1)' "$alpha_json" "$alpha_id" && break
    sleep 0.05
  done
  python3 -c 'import json,sys; p=json.loads(sys.argv[1]); target=int(sys.argv[2]); assert any(s["id"]==target and s["windows"] >= 1 for s in p["sessions"]), p' "$alpha_json" "$alpha_id"

  rename_result="$(busctl --address="$MANIFOLDD_DBUS_ADDRESS" call \
    "$MANIFOLDD_DBUS_DESTINATION" /org/manifold/Session \
    org.manifold.Session RenameSurfaceSession ts "$alpha_id" "Alpha Renamed")"
  [[ "$rename_result" == 'b true' ]]

  ctl surface-session dir-new "$workspace/beta" default "Beta Workspace" >/dev/null
  beta_json="$(ctl --json '["surface-session","list-json"]')"
  beta_id="$(python3 -c 'import json,sys; payload=json.loads(sys.argv[1]); matches=[s for s in payload["sessions"] if s["name"]=="Beta Workspace"]; assert len(matches)==1, payload; print(matches[0]["id"])' "$beta_json")"
  start_session_client "$beta_unit" "$beta_id" "$workspace/beta" \
    "$terminal_bin"
  for _ in $(seq 1 100); do
    beta_json="$(ctl --json '["surface-session","list-json"]')"
    python3 -c 'import json,sys; p=json.loads(sys.argv[1]); target=int(sys.argv[2]); raise SystemExit(0 if any(s["id"]==target and s["windows"] >= 1 for s in p["sessions"]) else 1)' "$beta_json" "$beta_id" && break
    sleep 0.05
  done
  python3 -c 'import json,sys; p=json.loads(sys.argv[1]); target=int(sys.argv[2]); assert any(s["id"]==target and s["windows"] >= 1 for s in p["sessions"]), p' "$beta_json" "$beta_id"

  switch_alpha="$(ctl surface-session switch "$alpha_id")"
  switch_beta="$(ctl surface-session switch "$beta_id")"
  switch_alpha_final="$(ctl surface-session switch "$alpha_id")"
  alpha_default="$(ctl surface-session dir-get "$workspace/alpha")"
  beta_default="$(ctl surface-session dir-get "$workspace/beta")"
  [[ "$alpha_default" == "OK surface-session session=$alpha_id" ]]
  [[ "$beta_default" == "OK surface-session session=$beta_id" ]]

  close_beta="$(ctl surface-session close "$beta_id")"
  final_json="$(ctl --json '["surface-session","list-json"]')"
  python3 -c 'import json,sys; p=json.loads(sys.argv[1]); closed=int(sys.argv[2]); active=int(sys.argv[3]); assert p["active"]==active, p; assert all(s["id"] != closed for s in p["sessions"]), p' "$final_json" "$beta_id" "$alpha_id"
  beta_default="$(ctl surface-session dir-get "$workspace/beta")"
  [[ "$beta_default" == 'OK surface-session session=none' ]]

  # Launch the real user-facing extension after the routing transitions so its
  # rendered proof observes the final stable Alpha state.
  start_session_client "$switcher_unit" "$alpha_id" "$workspace/alpha" \
    "$env_bin" GSK_RENDERER=cairo GTK_A11Y=none "$python_bin" \
    "$ROOT/apps/manifold-session-switcher.py" \
    --ready-file "$ready" \
    --event-file "$events" \
    --refresh-ms 100
  for _ in $(seq 1 200); do
    grep -q '^ready=true$' "$ready" 2>/dev/null && break
    sleep 0.05
  done
  grep -q '^ready=true$' "$ready"
  for _ in $(seq 1 200); do
    final_json="$(ctl --json '["surface-session","list-json"]')"
    python3 -c 'import json,sys; p=json.loads(sys.argv[1]); target=int(sys.argv[2]); raise SystemExit(0 if any(s["id"]==target and s["windows"] >= 2 for s in p["sessions"]) else 1)' "$final_json" "$alpha_id" && break
    sleep 0.05
  done
  python3 -c 'import json,sys; p=json.loads(sys.argv[1]); target=int(sys.argv[2]); assert any(s["id"]==target and s["windows"] >= 2 for s in p["sessions"]), p' "$final_json" "$alpha_id"

  register="$(ctl agents register build-agent surface_session="$alpha_id" pane_id=state-services-proof status=running)"
  agent_id="${register#agent id=}"
  notify="$(ctl agents notify "$agent_id" "Build ready" "The state-service proof artifact is ready for review")"
  agents="$(ctl agents list)"
  [[ "$agents" == *"agent id=$agent_id kind=build-agent status=waiting"* ]]
  [[ "$agents" == *"session=$alpha_id attention=true unread=1 notifications=1"* ]]

  sleep 0.5
  ctl debug dump "$dump" >/dev/null
  test -s "$dump/composed.png"
  test -s "$dump/state.json"
  status="$(ctl status-json)"

  {
    echo '# State service nested proof'
    echo
    echo 'A real session-switcher extension registered, mapped a GTK window, and refreshed from the extracted session and registry services.'
    echo
    echo '```json'
    printf '%s\n' "$alpha_json"
    printf '%s\n' "$beta_json"
    printf '%s\n' "$final_json"
    echo '```'
    echo
    echo 'The same service accepted a typed rename, switched away and back, removed Beta, and retired its directory default while preserving Alpha:'
    echo
    echo '```text'
    printf '%s\n%s\n%s\n%s\n%s\n' "$rename_result" "$switch_alpha" "$switch_beta" "$switch_alpha_final" "$close_beta"
    printf '%s\n%s\n' "$alpha_default" "$beta_default"
    echo '```'
    echo
    echo 'A real agent record then transitioned from running to waiting with a substantive notification:'
    echo
    echo '```text'
    printf '%s\n%s\n%s\n' "$register" "$notify" "$agents"
    echo '```'
    echo
    echo 'The switcher emitted these observable lifecycle/refresh events:'
    echo
    echo '```text'
    tail -n 40 "$ready" "$events"
    echo '```'
    echo
    echo "The rendered proof is in \`$dump/composed.png\`; \`state.json\` records its mapped window and active session."
    echo
    echo '```json'
    printf '%s\n' "$status"
    echo '```'
  } >"$proof"

  echo "state_services_controller=ok proof=$proof screenshot=$dump/composed.png"
  # `--keep-running` prevents the nested supervisor from treating the first
  # mapped proof window as command completion. Stop the outer supervisor only
  # after every assertion and artifact write has completed.
  RUST_LOG=off "$BIN" ctl \
    --dbus-address "$MANIFOLDD_DBUS_ADDRESS" \
    --dbus-destination "$outer_destination" \
    quit >/dev/null
  ctl quit >/dev/null
}

if [[ "${1:-}" == "--controller" ]]; then
  [[ $# -eq 2 ]]
  run_controller "$2"
  exit
fi

cd "$ROOT"
nix develop --command cargo build --quiet -p manifoldd --bins

OUT="${MANIFOLD_STATE_SERVICES_OUT:-$(mktemp -d /tmp/manifold-state-services.XXXXXX)}"
WORKSPACE="$OUT/workspace"
mkdir -p "$WORKSPACE"
TEST_LOG="$OUT/test.log"
(
  cd "$WORKSPACE"
  RUST_LOG="${RUST_LOG:-manifoldd=info}" \
  timeout --kill-after=5s 120s nix develop "$ROOT" --command "$BIN" test nested \
    --force-outer \
    --keep-running \
    --out-dir "$OUT/nested" \
    --inner-command "$ROOT/tools/test-state-services-nested.sh --controller $WORKSPACE"
) | tee "$TEST_LOG"

grep -qx 'topology=outer-supervisor' "$TEST_LOG"
grep -qx 'outer_presentation=offscreen' "$TEST_LOG"
grep -q 'state_services_controller=ok' "$OUT/nested/inner.log"
echo "state_services_nested=ok proof=$WORKSPACE/.manifold/state-services-proof.md screenshot=$WORKSPACE/.manifold/state-services-dump/composed.png"
