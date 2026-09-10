# Sourced by the isolated session-switcher harness via `brood test themes`.
# The caller owns the offscreen compositor, process cleanup and capture tools.
BASE_CONFIG="$OUT/theme-base.toml"
cp "$CONFIG" "$BASE_CONFIG"
mkdir -p "$OUT/themes"
wait_theme() {
  local name="$1"
  for _ in $(seq 1 300); do
    local revision
    revision="$(rg '^THEME_PUBLISHED ' "$CLIENT_LOG" | tail -1 | sed -n "s/^THEME_PUBLISHED revision=\([0-9]*\) name=$name actors=.*/\1/p")"
    if [[ -n "$revision" ]] && rg -q "facet native pair committed .*actor=status-bar .*theme_revision=$revision " "$CLIENT_LOG" \
      && rg -q "facet native pair committed .*actor=session-switcher .*theme_revision=$revision " "$CLIENT_LOG"; then
      sleep 0.15
      return 0
    fi
    kill -0 "$CLIENT_PID" 2>/dev/null || return 1
    sleep 0.05
  done
  echo "ERR theme $name never committed" >&2
  return 1
}
select_theme() {
  { printf 'theme = "%s"\n' "$1"; cat "$BASE_CONFIG"; } > "$CONFIG.next"
  mv "$CONFIG.next" "$CONFIG"
}
wait_theme ashen
sleep 0.5
capture "$OUT/theme-ashen.png"
source "$ROOT/tools/test-theme-switcher-live.sh"
STATUS_BEFORE="$(sed -n 's/.*facet native pair committed .*actor=status-bar .*snapshot=\([0-9]*\) .*/\1/p' "$CLIENT_LOG" | tail -1)"
STARTS_BEFORE="$(rg -c 'started presentation WASM actor' "$CLIENT_LOG")"
select_theme github_light
wait_theme github_light
capture "$OUT/theme-github-light.png"
STATUS_AFTER="$(sed -n 's/.*facet native pair committed .*actor=status-bar .*snapshot=\([0-9]*\) .*/\1/p' "$CLIENT_LOG" | tail -1)"
[[ "$STATUS_BEFORE" == "$STATUS_AFTER" ]]
[[ "$STARTS_BEFORE" == "$(rg -c 'started presentation WASM actor' "$CLIENT_LOG")" ]]

# Real keyboard input opens the shortcuts picker; exercise filtering and scroll.
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  "$OUT/virtual-keyboard-key" --ctrl-shift-only 53
sleep 0.5
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" wtype 'primer' -k BackSpace -k BackSpace -k BackSpace -k BackSpace -k BackSpace -k BackSpace -s 100 'term'
sleep 0.3
capture "$OUT/theme-light-picker.png"
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  "$OUT/virtual-keyboard-key" --plain 108
select_theme github_light_high_contrast
wait_theme github_light_high_contrast
capture "$OUT/theme-inherited-picker.png"
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  "$OUT/virtual-keyboard-key" --plain 1

# Invalid reloads preserve the complete last valid revision.
PUBLISHED_BEFORE="$(rg -c '^THEME_PUBLISHED' "$CLIENT_LOG")"
select_theme does-not-exist
for _ in $(seq 1 100); do
  rg -q 'THEME_REJECTED.*does-not-exist' "$CLIENT_LOG" && break
  sleep 0.05
done
rg -q 'THEME_REJECTED.*does-not-exist' "$CLIENT_LOG"
[[ "$PUBLISHED_BEFORE" == "$(rg -c '^THEME_PUBLISHED' "$CLIENT_LOG")" ]]

# Palette override, partial style and supported modifiers through a user theme.
cat > "$OUT/themes/local.toml" <<'THEME'
inherits = "github_light"
"ui.statusline" = { fg = "#123456", bg = "#f5f0e0", modifiers = ["bold", "italic"], underline = { style = "double_line", color = "#885522" } }
THEME
select_theme local
wait_theme local
capture "$OUT/theme-user-style.png"
# Parent edits must invalidate the child snapshot as well.
cat > "$OUT/themes/local.toml" <<'THEME'
inherits = "ashen"
"ui.statusline" = { fg = "#abcdef", bg = "#202020" }
THEME
sleep 0.3

# Rapid changes overlap render preparation; the final valid choice must win.
for _ in $(seq 1 6); do
  select_theme ashen
  sleep 0.04
  select_theme github_light
  sleep 0.04
done
select_theme github_light_high_contrast
wait_theme github_light_high_contrast
sleep 0.5
capture "$OUT/theme-final.png"
# Re-expose a pane whose retained output predates several theme revisions.
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  "$OUT/virtual-keyboard-key" --ctrl-shift-only 53
sleep 0.5
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  "$OUT/virtual-pointer-input" scroll 650 340 200 8 1280 720
sleep 0.2
capture "$OUT/theme-reopened-scrolled.png"
# Start a real terminal and switch themes while its actor processes output.
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  "$OUT/virtual-keyboard-key" --plain 1
ctl surface-session new "Theme proof" > "$OUT/theme-session.txt"
for _ in $(seq 1 400); do
  rg -q 'facet native pair committed .*actor=terminal-ui ' "$CLIENT_LOG" && break
  sleep 0.05
done
rg -q 'facet native pair committed .*actor=terminal-ui ' "$CLIENT_LOG"
sleep 0.5
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  "$OUT/virtual-pointer-click" 250 200 1280 720
TERMINAL_COMMAND='printf "\033[2J\033[H"; for i in $(seq 1 80); do printf "\033[38;2;180;180;180mliteral RGB\033[0m  theme row %s\n" "$i"; sleep 0.05; done'
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  wtype 'primer' -k BackSpace -k BackSpace -k BackSpace -k BackSpace -k BackSpace -k BackSpace -s 100 "$TERMINAL_COMMAND" -k Return
sleep 0.15
TERMINAL_BEFORE="$(rg -c 'facet native pair committed .*actor=terminal-ui ' "$CLIENT_LOG")"
select_theme ashen
wait_theme ashen
capture "$OUT/theme-terminal-dark.png"
select_theme github_light_high_contrast
wait_theme github_light_high_contrast
sleep 0.3
capture "$OUT/theme-terminal-light.png"
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  "$OUT/virtual-pointer-input" drag 9 38 235 38 10 1280 720
capture "$OUT/theme-terminal-selection.png"
[[ "$(rg -c 'facet native pair committed .*actor=terminal-ui ' "$CLIENT_LOG")" -gt "$TERMINAL_BEFORE" ]]
ctl status > "$OUT/theme-server-metrics.txt"
ctl actors list >> "$OUT/theme-server-metrics.txt"
python3 "$ROOT/tools/check-theme-proof.py" "$CLIENT_LOG" "$OUT" "$STATUS_BEFORE"
echo "theme_proof_artifacts=$OUT"
