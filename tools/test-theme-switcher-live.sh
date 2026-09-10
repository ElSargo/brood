# Sourced by the offscreen themes workload after its first Ashen capture.
theme_key() {
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    "$OUT/virtual-keyboard-key" "$@"
}
theme_picker_snapshot() {
  local revision
  revision="$(sed -n 's/.*facet native pair committed .*actor=picker .*snapshot=\([0-9]*\) .*/\1/p' "$CLIENT_LOG" | tail -1)"
  printf '%s\n' "${revision:-0}"
}
theme_wait_picker_frame() {
  local prior="$1" pane
  pane="$(sed -n 's/.*started presentation WASM actor id=picker .* pane=\([0-9]*\) .*/\1/p' "$CLIENT_LOG" | tail -1)"
  for _ in $(seq 1 200); do
    if (( $(theme_picker_snapshot) > prior )) \
      && [[ "$(rg '^focused (local plugin pane=|away from plugin pane=|backend-managed window=)' "$CLIENT_LOG" | tail -1)" == "focused local plugin pane=$pane" ]] \
      && awk '/^accepted synchronized layout / { pending = 1 } /^(completed|aborted) synchronized layout / { pending = 0 } END { exit pending }' "$CLIENT_LOG"; then
      return 0
    fi
    sleep 0.05
  done
  echo 'ERR picker did not commit a focused frame' >&2
  return 1
}
open_theme_picker() {
  local before prior
  prior="$(theme_picker_snapshot)"
  before="$(rg -c 'sender=picker recipient=theme-switcher body=.*"type":"picker-created"' "$CLIENT_LOG" || true)"
  theme_key 35
  for _ in $(seq 1 200); do
    local after
    after="$(rg -c 'sender=picker recipient=theme-switcher body=.*"type":"picker-created"' "$CLIENT_LOG" || true)"
    if (( ${after:-0} > ${before:-0} )); then
      if ! rg 'sender=picker recipient=theme-switcher body=.*"type":"picker-created"' "$CLIENT_LOG" | tail -1 | rg -q '"queued":true'; then
        theme_wait_picker_frame "$prior"
      fi
      return 0
    fi
    sleep 0.05
  done
  echo 'ERR theme picker did not open' >&2
  return 1
}
theme_query() {
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    wtype 'primer' -k BackSpace -k BackSpace -k BackSpace -k BackSpace -k BackSpace -k BackSpace -s 60 "$1"
}

open_theme_picker
capture "$OUT/theme-switcher-open.png"
# One arrow advances exactly one item (saved Ashen is first, then sorted Acme).
theme_key --plain 108
wait_theme acme
cmp "$CONFIG" "$BASE_CONFIG"
capture "$OUT/theme-switcher-keyboard-preview.png"
# Hover changes the same focus, without pressing or selecting the item.
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  "$OUT/virtual-pointer-input" hover 650 185 250 1280 720
wait_theme ashen
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  "$OUT/virtual-pointer-input" hover 650 215 250 1280 720
wait_theme acme
cmp "$CONFIG" "$BASE_CONFIG"
capture "$OUT/theme-switcher-hover-preview.png"
theme_key --plain 1
wait_theme ashen
cmp "$CONFIG" "$BASE_CONFIG"
# Move away before reopening so the pointer does not replace keyboard focus.
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  "$OUT/virtual-pointer-input" hover 100 100 50 1280 720

open_theme_picker
theme_query github_light_high_contrast
wait_theme github_light_high_contrast
cmp "$CONFIG" "$BASE_CONFIG"
capture "$OUT/theme-switcher-filter-preview.png"
# No matches restore the saved theme; Escape also leaves disk untouched.
theme_query zzzzzzzzzz
wait_theme ashen
capture "$OUT/theme-switcher-empty.png"
theme_key --plain 1
cmp "$CONFIG" "$BASE_CONFIG"

# A concurrent malformed config edit prevents saving. Roll back the preview,
# preserve the failed input, and report the error through the real status bar.
open_theme_picker
theme_query github_light_high_contrast
wait_theme github_light_high_contrast
printf '[broken\n' > "$CONFIG.next"
mv "$CONFIG.next" "$CONFIG"
theme_key --plain 28
wait_theme ashen
rg -q '^THEME_REJECTED commit:' "$CLIENT_LOG"
[[ "$(cat "$CONFIG")" == '[broken' ]]
for _ in $(seq 1 100); do
  rg -q 'sender=theme-switcher recipient=status-bar body=.*parse theme config before saving' "$CLIENT_LOG" && break
  sleep 0.05
done
rg -q 'sender=theme-switcher recipient=status-bar body=.*parse theme config before saving' "$CLIENT_LOG"
capture "$OUT/theme-switcher-save-error.png"
cp "$BASE_CONFIG" "$CONFIG.next"
mv "$CONFIG.next" "$CONFIG"

# Queued requests must not preview before the foreground picker completes.
QUEUED_PRIOR="$(theme_picker_snapshot)"
theme_key --ctrl-shift-only 53
theme_wait_picker_frame "$QUEUED_PRIOR"
open_theme_picker
rg -q 'sender=picker recipient=theme-switcher body=.*"queued":true' "$CLIENT_LOG"
cmp "$CONFIG" "$BASE_CONFIG"
QUEUED_PRIOR="$(theme_picker_snapshot)"
theme_key --plain 1
theme_wait_picker_frame "$QUEUED_PRIOR"
theme_query github_light_high_contrast
wait_theme github_light_high_contrast
theme_key --plain 28
for _ in $(seq 1 200); do
  if python3 - "$CONFIG" <<'PY'
import sys, tomllib
with open(sys.argv[1], 'rb') as f:
    assert tomllib.load(f).get('theme') == 'github_light_high_contrast'
PY
  then break; fi
  sleep 0.05
done
python3 - "$CONFIG" "$BASE_CONFIG" <<'PY'
import sys, tomllib
with open(sys.argv[1], 'rb') as f: selected = tomllib.load(f)
with open(sys.argv[2], 'rb') as f: original = tomllib.load(f)
assert selected.pop('theme') == 'github_light_high_contrast'
assert selected == original
PY
sleep 0.3
capture "$OUT/theme-switcher-selected.png"
# Reopening and cancelling must restore the newly committed choice.
open_theme_picker
theme_query ashen
wait_theme ashen
theme_key --plain 1
wait_theme github_light_high_contrast
capture "$OUT/theme-switcher-cancel-after-save.png"
cp "$CONFIG" "$OUT/theme-switcher-saved.toml"
open_theme_picker
theme_query ashen
wait_theme ashen
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  "$OUT/virtual-pointer-input" click 650 185 1280 720
for _ in $(seq 1 100); do
  rg -q '^theme = "ashen"' "$CONFIG" && break
  sleep 0.05
done
rg -q '^theme = "ashen"' "$CONFIG"

# Restore the workload's initial config before continuing its reload checks.
cp "$BASE_CONFIG" "$CONFIG.next"
mv "$CONFIG.next" "$CONFIG"
wait_theme ashen
