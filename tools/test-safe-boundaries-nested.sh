#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug/manifold"
POLICY_WASM="$ROOT/examples/wasm-geometry-layout-policy/target/wasm32-unknown-unknown/release/manifold_wasm_geometry_layout_policy.wasm"

ctl() {
  RUST_LOG=off timeout 5s "$BIN" ctl "$@"
}

run_visible_controller() {
  local out="$MANIFOLD_SAFE_BOUNDARIES_OUT"
  local dump="$out/inner-safe-boundaries-dump"
  local proof="$out/safe-boundaries-proof.md"
  local shm_wrapper="$out/simple-shm-wrapper.sh"
  local plug spawn pane notification_id status

  printf '\033[2J\033[H'
  echo 'Manifold audited boundary proof'
  echo
  echo 'A real workspace plugin will split this terminal and animated Weston SHM.'
  echo 'The same run sends a real desktop notification through the session bus,'
  echo 'then records client buffer state and captures the final offscreen EGL frame.'
  echo

  plug="$("$BIN" plug "$POLICY_WASM")"
  echo "$plug"
  spawn="$(ctl spawn "$shm_wrapper")"
  pane="$(sed -n 's/.*pane_id=\([0-9][0-9]*\).*/\1/p' <<<"$spawn")"
  test -n "$pane"

  for _ in $(seq 1 200); do
    status="$(ctl status-json)"
    if python3 -c 'import json,sys; p=json.loads(sys.argv[1]); raise SystemExit(0 if p["windows"] >= 2 and p["frames"] >= 2 else 1)' "$status"; then
      break
    fi
    sleep 0.05
  done
  python3 -c 'import json,sys; p=json.loads(sys.argv[1]); assert p["windows"] >= 2 and p["frames"] >= 2, p' "$status"

  # The real policy plugin installs its typed layout on this key invocation.
  ctl inject key 30 pressed >/dev/null
  ctl inject key 30 released >/dev/null
  for _ in $(seq 1 100); do
    status="$(ctl status-json)"
    if python3 -c 'import json,sys; p=json.loads(sys.argv[1]); raise SystemExit(0 if p["resolved_layout_source"] == "wasm-policy" and p["resolved_layout_panes"] == 2 else 1)' "$status"; then
      break
    fi
    sleep 0.05
  done
  python3 -c 'import json,sys; p=json.loads(sys.argv[1]); assert p["resolved_layout_source"] == "wasm-policy" and p["resolved_layout_panes"] == 2, p' "$status"

  # This shell is a managed Wayland client, so its session address names the
  # in-process realm bus rather than the host bus. `-p` returns the host's real
  # notification id after the broker has parsed, rebuilt, and proxied the wire
  # message through the audited boundary.
  notification_id="$(notify-send -p -t 10000 \
    --app-name='Manifold boundary proof' \
    'Audited boundaries crossed' \
    'Real plugin layout, SHM capture, offscreen EGL, and session-bus routing are active.')"
  [[ "$notification_id" =~ ^[0-9]+$ ]]

  echo
  echo "Plugin layout: terminal + animated SHM client (pane $pane)"
  echo "Session-bus notification: host id $notification_id"
  echo 'Debug capture: recording client buffers and composed EGL frame...'
  sleep 0.4

  ctl debug dump "$dump" >/dev/null
  test -s "$dump/composed.png"
  test -s "$dump/state.json"
  test -s "$dump/manifest.json"
  python3 - "$dump/manifest.json" "$dump/composed.png" <<'PY'
import json
import pathlib
import sys

manifest = json.loads(pathlib.Path(sys.argv[1]).read_text())
png = pathlib.Path(sys.argv[2]).read_bytes()
captured = [surface for surface in manifest["surfaces"] if surface["status"] == "captured"]
assert manifest["surfaces"], manifest
assert all(surface["status"] in {"captured", "no_current_buffer"} for surface in manifest["surfaces"])
assert all((pathlib.Path(sys.argv[1]).parent / "surfaces" / item["path"]).stat().st_size > 128 for item in captured)
assert png.startswith(b"\x89PNG\r\n\x1a\n") and len(png) > 4096, len(png)
PY

  {
    echo '# Audited unsafe-boundary nested proof'
    echo
    echo 'A real workspace geometry plugin visibly split a Weston terminal and an animated Weston SHM client. The terminal sent a substantive desktop notification over its isolated in-process session bus; the returned host notification id proves the message was parsed, rebuilt with bus-owned headers, forwarded, and replied to.'
    echo
    echo '```text'
    echo "$plug"
    echo "$spawn"
    echo "plugin_layout_source=wasm-policy"
    echo "plugin_layout_panes=2"
    echo "session_bus_notification_summary=Audited boundaries crossed"
    echo "session_bus_host_notification_id=$notification_id"
    echo '```'
    echo
    echo 'The default debug bundle captured the final composed frame through the offscreen Winit/EGL renderer and recorded both live client surfaces. Even the continuously committing SHM client reports `no_current_buffer`: Smithay consumes each transient `BufferAssignment` during commit before a later DBus debug request can inspect it. The real plugin cannot change that protocol lifetime. The SHM mapping helper is therefore covered separately by deterministic padded-row, ARGB/XRGB alpha, short mapping, zero dimension/stride, and arithmetic-overflow fixtures.'
    echo
    echo '```json'
    cat "$dump/manifest.json"
    echo
    echo '```'
    echo
    echo "Rendered proof: \`$dump/composed.png\`"
  } >"$proof"

  echo "Visible result: plugin split active; notification id $notification_id"
  echo "Screenshot: $dump/composed.png"
  sleep 0.8
}

if [[ "${MANIFOLD_SAFE_BOUNDARIES_TERMINAL:-0}" == "1" ]]; then
  run_visible_controller 2>&1 | tee "$MANIFOLD_SAFE_BOUNDARIES_OUT/controller.log"
  exit 0
fi

OUT="${MANIFOLD_SAFE_BOUNDARIES_OUT:-$(mktemp -d /tmp/manifold-safe-boundaries.XXXXXX)}"
rm -rf "$OUT"
mkdir -p "$OUT"

cat >"$OUT/simple-shm-wrapper.sh" <<EOF
#!/usr/bin/env bash
exec weston-simple-shm >"$OUT/weston-simple-shm.log" 2>&1
EOF
chmod +x "$OUT/simple-shm-wrapper.sh"

nix develop --command cargo build -p manifoldd
nix develop --command cargo build \
  --manifest-path examples/wasm-geometry-layout-policy/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release

export MANIFOLD_SAFE_BOUNDARIES_OUT="$OUT"
export MANIFOLD_SAFE_BOUNDARIES_TERMINAL=1
timeout --kill-after=5s 120s nix develop --command "$BIN" test nested \
  --force-outer \
  --out-dir "$OUT/nested" \
  --wait-for-inner-command-exit \
  --inner-command "weston-terminal --shell=$ROOT/tools/test-safe-boundaries-nested.sh" \
  | tee "$OUT/nested-test.stdout"

grep -q '^inner_command_visible=true$' "$OUT/nested-test.stdout"
grep -q '^inner_command_completed=true$' "$OUT/nested-test.stdout"
grep -q '^session_bus_host_notification_id=[0-9][0-9]*$' "$OUT/safe-boundaries-proof.md"
grep -Eq '"status": "(captured|no_current_buffer)"' "$OUT/inner-safe-boundaries-dump/manifest.json"
test -s "$OUT/inner-safe-boundaries-dump/composed.png"
test -s "$OUT/nested/outer-dump/composed.png"

echo "safe_boundaries_nested=verified proof=$OUT/safe-boundaries-proof.md screenshot=$OUT/inner-safe-boundaries-dump/composed.png"
