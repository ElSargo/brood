#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug/manifold"
CLIENT="$ROOT/target/debug/manifold-client"

if [[ "${MANIFOLD_SESSION_SWITCHER_CONTROLLER:-0}" == 1 ]]; then
  if [[ "${MANIFOLD_SESSION_SWITCHER_DBUS_WRAPPED:-0}" != 1 ]]; then
    exec env -u MANIFOLDD_DBUS_ADDRESS -u MANIFOLDD_DBUS_DESTINATION -u manifold_session \
      dbus-run-session -- env MANIFOLD_SESSION_SWITCHER_DBUS_WRAPPED=1 "$0"
  fi

  exec > >(tee "$MANIFOLD_SESSION_SWITCHER_OUT/controller.log") 2>&1
  export XDG_CACHE_HOME="$MANIFOLD_SESSION_SWITCHER_OUT/cache"
  export MANIFOLD_CONFIG="$MANIFOLD_SESSION_SWITCHER_OUT/manifold.toml"
  mkdir -p "$XDG_CACHE_HOME"
  client_log="$MANIFOLD_SESSION_SWITCHER_OUT/client.log"
  server_log="$XDG_CACHE_HOME/manifold/session-server/global.log"

  "$CLIENT" >"$client_log" 2>&1 &
  client_pid=$!
  trap 'kill "$client_pid" 2>/dev/null || true' EXIT

  for _ in $(seq 1 400); do
    if [[ -f "$server_log" ]] \
      && grep -q 'started presentation WASM actor id=session-switcher' "$client_log" \
      && grep -q 'loaded presentation key policy provider=workspace' "$client_log" \
      && grep -q 'accepted plugin draw packet' "$client_log" \
      && grep -q 'presentation session snapshot revision=.*sessions=1' "$client_log"; then
      break
    fi
    if ! kill -0 "$client_pid" 2>/dev/null; then
      echo 'ERR manifold-client exited before the session switcher rendered' >&2
      sed -n '1,260p' "$client_log" >&2
      [[ -f "$server_log" ]] && sed -n '1,260p' "$server_log" >&2
      exit 1
    fi
    sleep 0.05
  done

  "$BIN" ctl surface-session new Beta >/dev/null
  "$BIN" ctl surface-session new Gamma >/dev/null
  for _ in $(seq 1 300); do
    if grep -q 'presentation session snapshot revision=.*sessions=3' "$client_log"; then
      break
    fi
    sleep 0.05
  done
  grep -q 'presentation session snapshot revision=.*sessions=3' "$client_log"

  refresh_backend_state() {
    RUST_LOG=off "$BIN" ctl status >"$MANIFOLD_SESSION_SWITCHER_OUT/backend-status.tmp"
    mv "$MANIFOLD_SESSION_SWITCHER_OUT/backend-status.tmp" \
      "$MANIFOLD_SESSION_SWITCHER_OUT/backend-status"
    RUST_LOG=off "$BIN" ctl surface-session list-json \
      >"$MANIFOLD_SESSION_SWITCHER_OUT/backend-sessions.tmp"
    mv "$MANIFOLD_SESSION_SWITCHER_OUT/backend-sessions.tmp" \
      "$MANIFOLD_SESSION_SWITCHER_OUT/backend-sessions.json"
  }
  refresh_backend_state
  touch "$MANIFOLD_SESSION_SWITCHER_OUT/controller.ready"

  for _ in $(seq 1 600); do
    [[ -f "$MANIFOLD_SESSION_SWITCHER_OUT/controller.stop" ]] && break
    if ! kill -0 "$client_pid" 2>/dev/null; then
      echo 'ERR manifold-client exited during session switcher interaction' >&2
      sed -n '1,320p' "$client_log" >&2
      exit 1
    fi
    refresh_backend_state
    sleep 0.05
  done
  test -f "$MANIFOLD_SESSION_SWITCHER_OUT/controller.stop"

  kill "$client_pid" 2>/dev/null || true
  wait "$client_pid" || true
  "$BIN" ctl quit >/dev/null 2>&1 || true
  trap - EXIT
  touch "$MANIFOLD_SESSION_SWITCHER_OUT/controller.done"
  exit 0
fi

OUT="${MANIFOLD_SESSION_SWITCHER_OUT:-$(mktemp -d /tmp/manifold-session-switcher.XXXXXX)}"
PACKAGE="$OUT/package"
mkdir -p "$PACKAGE"
exec > >(tee "$OUT/driver.log") 2>&1
trap 'status=$?; echo "ERR session switcher proof line=$LINENO status=$status"; exit "$status"' ERR

nix develop --command cargo build -p manifoldd -p manifold-client --bins
nix develop --command cargo build \
  --manifest-path examples/wasm-init-actor/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release
nix develop --command cargo run --quiet -p manifold-wasm --example componentize -- \
  examples/wasm-init-actor/target/wasm32-unknown-unknown/release/manifold_wasm_init_actor_example.wasm \
  "$PACKAGE/session-init.component.wasm"
nix develop --command cargo build \
  --manifest-path plugins/session-switcher/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release
nix develop --command cargo run --quiet -p manifold-wasm --example componentize -- \
  plugins/session-switcher/target/wasm32-unknown-unknown/release/manifold_session_switcher_plugin.wasm \
  "$PACKAGE/manifold_session_switcher_plugin.component.wasm"
nix develop --command cargo build \
  --manifest-path plugins/default-workspace/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release

printf '[init]\nsession = "%s"\nclient = "%s"\n\n[client.key-policy]\nprovider = "workspace"\npath = "%s"\n' \
  "$PACKAGE/session-init.component.wasm" \
  "$PACKAGE/manifold_session_switcher_plugin.component.wasm" \
  "$ROOT/plugins/default-workspace/target/wasm32-unknown-unknown/release/manifold_default_workspace_plugin.wasm" \
  >"$OUT/manifold.toml"

export MANIFOLD_CONFIG="$OUT/manifold.toml"
export MANIFOLD_SESSION_SWITCHER_CONTROLLER=1
export MANIFOLD_SESSION_SWITCHER_OUT="$OUT"
# The source checkout currently lacks the test subcommand carried by the
# Nix-provided Manifold test supervisor, so use that checked tool after the
# source binaries above have been built.
RUST_LOG=manifoldd=info nix develop --command manifold test nested \
  --force-outer \
  --keep-running \
  --out-dir "$OUT/nested" \
  --inner-command "weston-terminal --shell=$ROOT/tools/test-session-switcher-plugin-nested.sh" \
  > >(tee "$OUT/nested-test.stdout") 2>&1 &
test_pid=$!
trap 'kill "$test_pid" 2>/dev/null || true' EXIT

for _ in $(seq 1 600); do
  [[ -f "$OUT/controller.ready" ]] && break
  sleep 0.05
done
test -f "$OUT/controller.ready"

inner_destination="$(sed -n 's/^inner_dbus_destination=//p' "$OUT/nested-test.stdout" | tail -1)"
outer_destination="$(sed -n 's/^outer_dbus_destination=//p' "$OUT/nested-test.stdout" | tail -1)"
test -n "$inner_destination"
test -n "$outer_destination"

ctl() {
  RUST_LOG=off "$BIN" ctl --dbus-destination "$inner_destination" "$@"
}

# The nested host carries its own Manifold shortcuts. Disable their interception
# so this proof exercises the focused inner client's policy, just as a desktop
# compositor with no conflicting global shortcut would.
ctl passthrough-mode set on >/dev/null

inject_key() {
  ctl inject key "$1" pressed >/dev/null
  ctl inject key "$1" released >/dev/null
}

inject_ctrl_key() {
  ctl inject key 37 pressed >/dev/null
  inject_key "$1"
  ctl inject key 37 released >/dev/null
}

inject_ctrl_alt_key() {
  ctl inject key 37 pressed >/dev/null
  ctl inject key 64 pressed >/dev/null
  inject_key "$1"
  ctl inject key 64 released >/dev/null
  ctl inject key 37 released >/dev/null
}

inject_click() {
  ctl inject motion "$1" "$2" >/dev/null
  ctl inject button 272 pressed >/dev/null
  ctl inject button 272 released >/dev/null
}

type_keys() {
  local key
  for key in "$@"; do
    inject_key "$key"
  done
}

ctl debug dump "$OUT/nested/sessions-initial" >/dev/null
read -r client_x client_y client_w client_h < <(
  python3 - "$OUT/nested/sessions-initial/state.json" <<'PY'
import json
import sys

data = json.load(open(sys.argv[1], encoding="utf-8"))
rect = data["layout"]["active_rect"]
print(rect["x"], rect["y"], rect["w"], rect["h"])
PY
)

# The Winit presentation is the active inner window. The switcher wrapper
# reserves at most 45% on the right; pointer coordinates are projected back
# into its pane-local canvas by manifold-client.
row_x=$((client_x + 55 * client_w / 100 + 50))
# Pick the second catalogue row in the switcher's full-height pane.
second_row_y=$((client_y + 173 * client_h / 800))

# Vanilla config must load the workspace policy without client CLI overrides.
# Ctrl+O spawns one real terminal in the active session.
inject_ctrl_key 32
for _ in $(seq 1 200); do
  sessions_after_spawn="$(cat "$OUT/backend-sessions.json")"
  active_windows="$(python3 - "$sessions_after_spawn" <<'PY'
import json
import sys

sessions = json.loads(sys.argv[1])["sessions"]
print(next(session["windows"] for session in sessions if session["active"]))
PY
)"
  [[ "$active_windows" == 2 ]] && break
  sleep 0.05
done
[[ "$active_windows" == 2 ]]

# Return focus to the plugin, then prove Ctrl+J/K cycle between the two real
# managed windows. The client logs the exact backend pane selected each time.
input_y=$((client_y + 70 * client_h / 1600))
inject_click "$row_x" "$input_y"
focus_count="$(grep -c 'focused backend-managed window=' "$OUT/client.log" || true)"
inject_ctrl_key 44
for _ in $(seq 1 100); do
  next_focus_count="$(grep -c 'focused backend-managed window=' "$OUT/client.log" || true)"
  ((next_focus_count > focus_count)) && break
  sleep 0.05
done
first_focus="$(grep 'focused backend-managed window=' "$OUT/client.log" | tail -1)"
focus_count="$next_focus_count"
inject_ctrl_key 44
for _ in $(seq 1 100); do
  next_focus_count="$(grep -c 'focused backend-managed window=' "$OUT/client.log" || true)"
  ((next_focus_count > focus_count)) && break
  sleep 0.05
done
second_focus="$(grep 'focused backend-managed window=' "$OUT/client.log" | tail -1)"
[[ "$first_focus" != "$second_focus" ]]
focus_count="$next_focus_count"
inject_ctrl_key 45
for _ in $(seq 1 100); do
  next_focus_count="$(grep -c 'focused backend-managed window=' "$OUT/client.log" || true)"
  ((next_focus_count > focus_count)) && break
  sleep 0.05
done
previous_focus="$(grep 'focused backend-managed window=' "$OUT/client.log" | tail -1)"
[[ "$previous_focus" == "$first_focus" ]]

# Ctrl+Alt+M focuses the visible switcher, hides it on the next invocation,
# and restores/focuses it on the third. Capture the managed-only middle state.
inject_ctrl_alt_key 58
for _ in $(seq 1 100); do
  grep -q 'focused visible presentation session switcher' "$OUT/client.log" && break
  sleep 0.05
done
grep -q 'focused visible presentation session switcher' "$OUT/client.log"
inject_ctrl_alt_key 58
for _ in $(seq 1 100); do
  grep -q 'hid presentation session switcher' "$OUT/client.log" && break
  sleep 0.05
done
grep -q 'hid presentation session switcher' "$OUT/client.log"
ctl debug dump "$OUT/nested/switcher-hidden" >/dev/null
inject_ctrl_alt_key 58
for _ in $(seq 1 100); do
  grep -q 'showed presentation session switcher' "$OUT/client.log" && break
  sleep 0.05
done
grep -q 'showed presentation session switcher' "$OUT/client.log"

for _ in $(seq 1 200); do
  # Winit may still be processing the configure generated by the debug dump;
  # retrying the idempotent activation also proves focus can return from the
  # managed pane to the plugin pane.
  inject_click "$row_x" "$second_row_y"
  status_after_switch="$(cat "$OUT/backend-status")"
  grep -q 'active_surface_session=2' <<<"$status_after_switch" && break
  sleep 0.05
done
grep -q 'active_surface_session=2' <<<"$status_after_switch"

# Type an unmatched query and let Enter create it. XKB keycodes spell delta.
inject_click "$row_x" "$input_y"
type_keys 40 26 46 28 38
inject_key 36
for _ in $(seq 1 200); do
  sessions_after_create="$(cat "$OUT/backend-sessions.json")"
  grep -q '"name":"delta"' <<<"$sessions_after_create" && break
  sleep 0.05
done
grep -q '"name":"delta"' <<<"$sessions_after_create"

# Rename the selected new session through the plugin's F2 editor.
inject_click "$row_x" "$input_y"
inject_key 68
for _ in $(seq 1 5); do inject_key 22; done
type_keys 32 58 26 42 38
inject_key 36
for _ in $(seq 1 200); do
  sessions_after_rename="$(cat "$OUT/backend-sessions.json")"
  grep -q '"name":"omega"' <<<"$sessions_after_rename" && break
  sleep 0.05
done
grep -q '"name":"omega"' <<<"$sessions_after_rename"
ctl debug dump "$OUT/nested/sessions-renamed" >/dev/null

# Delete closes the selected session and the catalogue reconciles to three.
inject_key 119
for _ in $(seq 1 200); do
  sessions_after_close="$(cat "$OUT/backend-sessions.json")"
  if ! grep -q '"name":"omega"' <<<"$sessions_after_close" \
    && [[ "$(grep -o '"id":' <<<"$sessions_after_close" | wc -l)" == 3 ]]; then
    break
  fi
  sleep 0.05
done
! grep -q '"name":"omega"' <<<"$sessions_after_close"
[[ "$(grep -o '"id":' <<<"$sessions_after_close" | wc -l)" == 3 ]]

{
  printf '# Native session switcher proof\n\n'
  printf '%s\n' '- initial catalogue: three sessions rendered in a client-realm plugin pane'
  printf '%s\n' "- vanilla Ctrl+O spawn: $sessions_after_spawn"
  printf '%s\n' "- Ctrl+J first focus: $first_focus"
  printf '%s\n' "- Ctrl+J second focus: $second_focus"
  printf '%s\n' "- Ctrl+K previous focus: $previous_focus"
  printf '%s\n' '- Ctrl+Alt+M: focus, hide, and restore completed'
  printf '%s\n' "- pointer switch: $status_after_switch"
  printf '%s\n' "- create: $sessions_after_create"
  printf '%s\n' "- rename: $sessions_after_rename"
  printf '%s\n' "- close: $sessions_after_close"
  printf '%s\n' "- initial screenshot: $OUT/nested/sessions-initial/composed.png"
  printf '%s\n' "- switcher-hidden screenshot: $OUT/nested/switcher-hidden/composed.png"
  printf '%s\n' "- renamed screenshot: $OUT/nested/sessions-renamed/composed.png"
} >"$OUT/proof.md"

touch "$OUT/controller.stop"
for _ in $(seq 1 300); do
  [[ -f "$OUT/controller.done" ]] && break
  sleep 0.05
done
test -f "$OUT/controller.done"

RUST_LOG=off "$BIN" ctl --dbus-destination "$inner_destination" quit >/dev/null || true
RUST_LOG=off "$BIN" ctl --dbus-destination "$outer_destination" quit >/dev/null || true
wait "$test_pid" || true
trap - EXIT

grep -q 'started presentation WASM actor id=session-switcher' "$OUT/client.log"
grep -q 'loaded presentation key policy provider=workspace' "$OUT/client.log"
grep -q 'granted claimed root layout lease actor=session-switcher' "$OUT/client.log"
grep -q 'granted rest layout lease .* actor=workspace' "$OUT/client.log"
grep -q 'accepted leased actor layout .* panes=3' "$OUT/client.log"
grep -q 'requested managed window resize .* logical_size=320x800' "$OUT/client.log"
! grep -q 'rejected actor-submitted layout' "$OUT/client.log"
grep -q 'presentation session snapshot revision=.*sessions=3' "$OUT/client.log"
grep -q 'operation=Create success=true' "$OUT/client.log"
grep -q 'operation=Rename success=true' "$OUT/client.log"
grep -q 'operation=Close success=true' "$OUT/client.log"
test -s "$OUT/nested/sessions-initial/composed.png"
test -s "$OUT/nested/switcher-hidden/composed.png"
test -s "$OUT/nested/sessions-renamed/composed.png"
test "$(magick "$OUT/nested/sessions-renamed/composed.png" -format '%k' info:)" -gt 50
test "$(magick "$OUT/nested/switcher-hidden/composed.png" -format '%k' info:)" -gt 50

echo "session_switcher_nested=verified proof=$OUT/proof.md initial=$OUT/nested/sessions-initial/composed.png renamed=$OUT/nested/sessions-renamed/composed.png"
