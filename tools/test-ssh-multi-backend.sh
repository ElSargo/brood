#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if (($# == 0)); then
  echo "usage: test-ssh-multi-backend.sh USER@HOST [--connect-host HOST]" >&2
  exit 2
fi
TARGET="$1"
shift
CONNECT_HOST=""
if (($# > 0)); then
  if [[ "$1" != "--connect-host" || $# != 2 ]]; then
    echo "usage: test-ssh-multi-backend.sh USER@HOST [--connect-host HOST]" >&2
    exit 2
  fi
  CONNECT_HOST="$2"
fi

if [[ -z "${BROOD_SSH_MULTI_BACKEND_DBUS:-}" ]]; then
  REEXEC_ARGS=("$0" "$TARGET")
  if [[ -n "$CONNECT_HOST" ]]; then
    REEXEC_ARGS+=(--connect-host "$CONNECT_HOST")
  fi
  exec env \
    -u brood_session \
    -u BROOD_SURFACE_SESSION_ID \
    -u BROOD_PANE_ID \
    -u DBUS_SESSION_BUS_ADDRESS \
    BROOD_SSH_MULTI_BACKEND_DBUS=1 \
    dbus-run-session -- "${REEXEC_ARGS[@]}"
fi

for tool in awk cargo cc grim jq nix pkg-config rg ssh sway swaymsg wtype; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "ERR $tool is required for the SSH multi-backend proof" >&2
    exit 2
  fi
done

OUT="${BROOD_SSH_MULTI_BACKEND_OUT:-$(mktemp -d /tmp/brood-ssh-multi-backend.XXXXXX)}"
RUNTIME_DIR="$OUT/runtime"
SWAY_CONFIG="$OUT/sway.conf"
COMPOSITOR_LOG="$OUT/compositor.log"
DEPLOY_LOG="$OUT/deploy.log"
TREE_JSON="$OUT/presentation-tree.json"
SCREENSHOT="$OUT/session-scoped-local.png"
DIRECTORY_SCREENSHOT="$OUT/remote-directory-filtered.png"
PROOF="$OUT/proof.json"
PROFILE="${BROOD_SSH_MULTI_BACKEND_PROFILE:-0}"
CLIENT_PROCESS_BEFORE="$OUT/client-process-before.txt"
CLIENT_PROCESS_AFTER="$OUT/client-process-after.txt"
REMOTE_PROCESS_BEFORE="$OUT/remote-process-before.txt"
REMOTE_PROCESS_AFTER="$OUT/remote-process-after.txt"
CLIENT_PERF_STAT="$OUT/client-perf-stat.csv"
CLIENT_PERF_DATA="$OUT/client-perf.data"
CLIENT_PERF_REPORT="$OUT/client-perf-report.txt"
ROUTE_PING="$OUT/route-ping.log"
mkdir -p "$OUT" "$RUNTIME_DIR"
chmod 700 "$RUNTIME_DIR"

COMPOSITOR_PID=""
DEPLOY_PID=""
PROFILE_PERF_PID=""
PROFILE_RECORD_PID=""
PROFILE_PING_PID=""

presentation_ids() {
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" SWAYSOCK="$SWAYSOCK" \
    swaymsg -t get_tree -r 2>/dev/null |
    jq -r '.. | objects | select(.type? == "con" and .pid? != null) | .id'
}

process_profile_snapshot() {
  local process_id="$1"
  echo "pid=$process_id"
  awk '
    { runtime += $1; wait += $2; slices += $3 }
    END {
      printf "task_schedstat=%d %d %d\n", runtime, wait, slices
    }
  ' "/proc/$process_id"/task/*/schedstat
  awk '
    $1 == "voluntary_ctxt_switches:" { voluntary += $2 }
    $1 == "nonvoluntary_ctxt_switches:" { involuntary += $2 }
    END {
      printf "task_context_switches=%d %d\n", voluntary, involuntary
    }
  ' "/proc/$process_id"/task/*/status
  sed -n '/^VmRSS:/p; /^Threads:/p' "/proc/$process_id/status"
  echo "stat=$(<"/proc/$process_id/stat")"
}

close_presentation() {
  local id
  while read -r id; do
    [[ -n "$id" ]] || continue
    env XDG_RUNTIME_DIR="$RUNTIME_DIR" SWAYSOCK="$SWAYSOCK" \
      swaymsg "[con_id=$id] kill" >/dev/null 2>&1 || true
  done < <(presentation_ids)
}

stop_remote_fallback() {
  local backend state helper
  backend="$(
    sed -n 's/.*ssh-deploy=ready target=[^ ]* backend=\([^ ]*\) connect=.*/\1/p' \
      "$DEPLOY_LOG" |
      tail -1
  )"
  [[ -n "$backend" ]] || return 0
  state="${backend%/backend}"
  helper="$backend/bin/brood-ssh-backend-session"
  ssh -T -o BatchMode=yes "$TARGET" "$helper" stop "$state" \
    >/dev/null 2>&1 || true
}

cleanup() {
  local result=$?
  set +e
  if [[ -n "$DEPLOY_PID" ]] && kill -0 "$DEPLOY_PID" 2>/dev/null; then
    close_presentation
    for _ in $(seq 1 100); do
      kill -0 "$DEPLOY_PID" 2>/dev/null || break
      sleep 0.1
    done
  fi
  if [[ -n "$DEPLOY_PID" ]] && kill -0 "$DEPLOY_PID" 2>/dev/null; then
    stop_remote_fallback
    kill "$DEPLOY_PID" >/dev/null 2>&1 || true
    wait "$DEPLOY_PID" >/dev/null 2>&1 || true
  fi
  if [[ -n "$COMPOSITOR_PID" ]] && kill -0 "$COMPOSITOR_PID" 2>/dev/null; then
    kill "$COMPOSITOR_PID" >/dev/null 2>&1 || true
    wait "$COMPOSITOR_PID" >/dev/null 2>&1 || true
  fi
  if [[ -n "$PROFILE_PERF_PID" ]] && kill -0 "$PROFILE_PERF_PID" 2>/dev/null; then
    kill "$PROFILE_PERF_PID" >/dev/null 2>&1 || true
    wait "$PROFILE_PERF_PID" >/dev/null 2>&1 || true
  fi
  if [[ -n "$PROFILE_RECORD_PID" ]] && kill -0 "$PROFILE_RECORD_PID" 2>/dev/null; then
    kill "$PROFILE_RECORD_PID" >/dev/null 2>&1 || true
    wait "$PROFILE_RECORD_PID" >/dev/null 2>&1 || true
  fi
  if [[ -n "$PROFILE_PING_PID" ]] && kill -0 "$PROFILE_PING_PID" 2>/dev/null; then
    kill "$PROFILE_PING_PID" >/dev/null 2>&1 || true
    wait "$PROFILE_PING_PID" >/dev/null 2>&1 || true
  fi
  if ((result != 0)); then
    echo "deploy_log=$DEPLOY_LOG" >&2
    tail -n 240 "$DEPLOY_LOG" >&2 || true
    echo "compositor_log=$COMPOSITOR_LOG" >&2
    tail -n 100 "$COMPOSITOR_LOG" >&2 || true
  fi
  return "$result"
}
trap 'echo "ERR command=$BASH_COMMAND line=$LINENO" >&2' ERR
trap cleanup EXIT

ssh -T -o BatchMode=yes -o ConnectTimeout=10 "$TARGET" uname -m \
  | rg -q '^(x86_64|aarch64)$'
REMOTE_HOME="$(
  ssh -T -o BatchMode=yes -o ConnectTimeout=10 "$TARGET" \
    'printf %s "$HOME"'
)"
[[ "$REMOTE_HOME" == /* ]] || {
  echo "ERR remote HOME is not an absolute directory" >&2
  exit 1
}

cargo build --release -p brood-client -p brood-remote -p broodd --bins --locked

build_component() {
  local manifest="$1"
  local core="$2"
  local component="$3"
  cargo build \
    --release \
    --target wasm32-unknown-unknown \
    --manifest-path "$manifest" \
    --locked
  [[ ! -e "$component" ]] || chmod u+w "$component"
  wasm-tools component new "$core" --output "$component"
  wasm-tools validate "$component"
}

build_component \
  examples/wasm-init-actor/Cargo.toml \
  examples/wasm-init-actor/target/wasm32-unknown-unknown/release/brood_wasm_init_actor_example.wasm \
  examples/wasm-init-actor/target/wasm32-unknown-unknown/release/brood_wasm_init_actor_example.component.wasm
build_component \
  plugins/picker/Cargo.toml \
  plugins/picker/target/wasm32-unknown-unknown/release/brood_picker_plugin.wasm \
  plugins/picker/target/wasm32-unknown-unknown/release/brood_picker_plugin.component.wasm
build_component \
  plugins/keybindings/Cargo.toml \
  plugins/keybindings/target/wasm32-unknown-unknown/release/brood_keybindings_plugin.wasm \
  plugins/keybindings/target/wasm32-unknown-unknown/release/brood_keybindings_plugin.component.wasm
build_component \
  plugins/diff-viewer/Cargo.toml \
  plugins/diff-viewer/target/wasm32-unknown-unknown/release/brood_diff_viewer_plugin.wasm \
  plugins/diff-viewer/target/wasm32-unknown-unknown/release/brood_diff_viewer_plugin.component.wasm
build_component \
  plugins/neovim-client/Cargo.toml \
  plugins/neovim-client/target/wasm32-unknown-unknown/release/brood_neovim_client_plugin.wasm \
  plugins/neovim-client/target/wasm32-unknown-unknown/release/brood_neovim_client_plugin.component.wasm
build_component \
  plugins/coding-agent/Cargo.toml \
  plugins/coding-agent/target/wasm32-unknown-unknown/release/brood_coding_agent_plugin.wasm \
  plugins/coding-agent/target/wasm32-unknown-unknown/release/brood_coding_agent_plugin.component.wasm
install -D \
  plugins/coding-agent/target/wasm32-unknown-unknown/release/brood_coding_agent_plugin.component.wasm \
  examples/wasm-init-actor/target/wasm32-unknown-unknown/release/brood_coding_agent_plugin.component.wasm
cargo build \
  --release \
  --target wasm32-unknown-unknown \
  --manifest-path plugins/terminal-session/Cargo.toml \
  --locked
[[ ! -e plugins/terminal-session/target/wasm32-unknown-unknown/release/brood_terminal_session_plugin.component.wasm ]] \
  || chmod u+w plugins/terminal-session/target/wasm32-unknown-unknown/release/brood_terminal_session_plugin.component.wasm
wasm-tools component new \
  plugins/terminal-session/target/wasm32-unknown-unknown/release/brood_terminal_session_plugin.wasm \
  --output \
  plugins/terminal-session/target/wasm32-unknown-unknown/release/brood_terminal_session_plugin.component.wasm
install -D \
  plugins/terminal-session/target/wasm32-unknown-unknown/release/brood_terminal_session_plugin.component.wasm \
  examples/wasm-init-actor/target/wasm32-unknown-unknown/release/brood_terminal_session_plugin.component.wasm
cargo build \
  --release \
  --target wasm32-unknown-unknown \
  --manifest-path plugins/terminal-ui/Cargo.toml \
  --locked
[[ ! -e plugins/terminal-ui/target/wasm32-unknown-unknown/release/brood_terminal_ui_plugin.component.wasm ]] \
  || chmod u+w plugins/terminal-ui/target/wasm32-unknown-unknown/release/brood_terminal_ui_plugin.component.wasm
wasm-tools component new \
  plugins/terminal-ui/target/wasm32-unknown-unknown/release/brood_terminal_ui_plugin.wasm \
  --output \
  plugins/terminal-ui/target/wasm32-unknown-unknown/release/brood_terminal_ui_plugin.component.wasm
install -D \
  plugins/terminal-ui/target/wasm32-unknown-unknown/release/brood_terminal_ui_plugin.component.wasm \
  plugins/session-switcher/target/wasm32-unknown-unknown/release/brood_terminal_ui_plugin.component.wasm
cargo build \
  --release \
  --target wasm32-unknown-unknown \
  --manifest-path plugins/session-switcher/Cargo.toml \
  --locked
[[ ! -e plugins/session-switcher/target/wasm32-unknown-unknown/release/brood_session_switcher_plugin.component.wasm ]] \
  || chmod u+w plugins/session-switcher/target/wasm32-unknown-unknown/release/brood_session_switcher_plugin.component.wasm
wasm-tools component new \
  plugins/session-switcher/target/wasm32-unknown-unknown/release/brood_session_switcher_plugin.wasm \
  --output \
  plugins/session-switcher/target/wasm32-unknown-unknown/release/brood_session_switcher_plugin.component.wasm
cargo build \
  --release \
  --target wasm32-unknown-unknown \
  --manifest-path plugins/status-bar/Cargo.toml \
  --locked
[[ ! -e plugins/session-switcher/target/wasm32-unknown-unknown/release/brood_status_bar_plugin.component.wasm ]] \
  || chmod u+w plugins/session-switcher/target/wasm32-unknown-unknown/release/brood_status_bar_plugin.component.wasm
wasm-tools component new \
  plugins/status-bar/target/wasm32-unknown-unknown/release/brood_status_bar_plugin.wasm \
  --output \
  plugins/session-switcher/target/wasm32-unknown-unknown/release/brood_status_bar_plugin.component.wasm
cargo build \
  --release \
  --target wasm32-unknown-unknown \
  --manifest-path plugins/inbox/Cargo.toml \
  --locked
[[ ! -e plugins/session-switcher/target/wasm32-unknown-unknown/release/brood_inbox_plugin.component.wasm ]] \
  || chmod u+w plugins/session-switcher/target/wasm32-unknown-unknown/release/brood_inbox_plugin.component.wasm
wasm-tools component new \
  plugins/inbox/target/wasm32-unknown-unknown/release/brood_inbox_plugin.wasm \
  --output \
  plugins/session-switcher/target/wasm32-unknown-unknown/release/brood_inbox_plugin.component.wasm
cargo build \
  --release \
  --target wasm32-unknown-unknown \
  --manifest-path plugins/worktree-manager/Cargo.toml \
  --locked
[[ ! -e plugins/worktree-manager/target/wasm32-unknown-unknown/release/brood_worktree_manager_plugin.component.wasm ]] \
  || chmod u+w plugins/worktree-manager/target/wasm32-unknown-unknown/release/brood_worktree_manager_plugin.component.wasm
wasm-tools component new \
  plugins/worktree-manager/target/wasm32-unknown-unknown/release/brood_worktree_manager_plugin.wasm \
  --output \
  plugins/worktree-manager/target/wasm32-unknown-unknown/release/brood_worktree_manager_plugin.component.wasm

read -r -a WAYLAND_BUILD_FLAGS <<< "$(pkg-config --cflags --libs wayland-client xkbcommon)"
cc \
  -std=c11 \
  -Wall \
  -Wextra \
  -Werror \
  tools/virtual-pointer-input.c \
  -o "$OUT/virtual-pointer-input" \
  "${WAYLAND_BUILD_FLAGS[@]}"
cc \
  -std=c11 \
  -Wall \
  -Wextra \
  -Werror \
  tools/virtual-keyboard-key.c \
  -o "$OUT/virtual-keyboard-key" \
  "${WAYLAND_BUILD_FLAGS[@]}"

printf '%s\n' \
  'xwayland disable' \
  'output HEADLESS-1 mode 1280x720' \
  'seat seat0 fallback true' \
  'default_border none' \
  'default_floating_border none' \
  > "$SWAY_CONFIG"

env XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WLR_BACKENDS=headless \
  WLR_HEADLESS_OUTPUTS=1 \
  WLR_LIBINPUT_NO_DEVICES=1 \
  sway \
  --unsupported-gpu \
  --config "$SWAY_CONFIG" \
  > "$COMPOSITOR_LOG" 2>&1 &
COMPOSITOR_PID=$!

SWAYSOCK=""
WAYLAND_SOCKET=""
for _ in $(seq 1 200); do
  SWAYSOCK="$(
    find "$RUNTIME_DIR" -maxdepth 1 -type s -name 'sway-ipc.*.sock' -print -quit
  )"
  WAYLAND_SOCKET="$(
    find "$RUNTIME_DIR" -maxdepth 1 -type s -name 'wayland-*' -printf '%f\n' |
      head -1
  )"
  [[ -n "$SWAYSOCK" && -n "$WAYLAND_SOCKET" ]] && break
  kill -0 "$COMPOSITOR_PID" 2>/dev/null || {
    echo "ERR headless Sway exited during startup" >&2
    exit 1
  }
  sleep 0.05
done
[[ -n "$SWAYSOCK" && -n "$WAYLAND_SOCKET" ]] || {
  echo "ERR timed out waiting for the headless Sway sockets" >&2
  exit 1
}
export SWAYSOCK

env \
  -u brood_session \
  -u BROOD_SURFACE_SESSION_ID \
  -u BROOD_PANE_ID \
  XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  BROOD_KEYBINDINGS_PATH="$OUT/keybindings.json" \
  WGPU_BACKEND=vulkan \
  BROOD_TRACE_PLUGIN_INPUT=1 \
  BROOD_REMOTE_BIN="$ROOT/target/release/brood-remote" \
  BROOD_SSH_CONNECT_HOST="$CONNECT_HOST" \
  RUST_LOG="${RUST_LOG:-broodd=info,brood_client=info,smithay=warn,wgpu_core=warn,wgpu_hal=warn}" \
  target/release/brood-client \
  > "$DEPLOY_LOG" 2>&1 &
DEPLOY_PID=$!

for _ in $(seq 1 1200); do
  if rg -a -q 'started presentation WASM actor id=session-switcher ' "$DEPLOY_LOG" \
    && [[ -n "$(presentation_ids)" ]]; then
    break
  fi
  kill -0 "$DEPLOY_PID" 2>/dev/null || {
    echo "ERR local presentation exited before the backend picker was ready" >&2
    exit 1
  }
  sleep 0.1
done
rg -a -q 'started presentation WASM actor id=session-switcher ' "$DEPLOY_LOG" || {
  echo "ERR timed out waiting for the in-client backend picker" >&2
  exit 1
}

# The actor starts before its delegated layout has placed the sidebar. Wait for
# the real 1280x720 two-pane composition so the click is expressed in stable
# presentation coordinates rather than racing the initial full-pane layout.
for _ in $(seq 1 300); do
  rg -a -q 'sent plugin pane resize .* logical_size=320x720 ' "$DEPLOY_LOG" \
    && break
  kill -0 "$DEPLOY_PID" 2>/dev/null || {
    echo "ERR presentation exited before the backend sidebar settled" >&2
    exit 1
  }
  sleep 0.1
done
rg -a -q 'sent plugin pane resize .* logical_size=320x720 ' "$DEPLOY_LOG" || {
  echo "ERR timed out waiting for the backend sidebar layout" >&2
  exit 1
}

PICKER_LAYOUT_BASELINE="$(
  rg -a -c 'accepted leased actor layout items=3 panes=3 ' "$DEPLOY_LOG" || true
)"
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  "$OUT/virtual-pointer-input" drag 1210 70 1210 70 1 1280 720
for _ in $(seq 1 100); do
  PICKER_LAYOUT_CURRENT="$(
    rg -a -c 'accepted leased actor layout items=3 panes=3 ' "$DEPLOY_LOG" || true
  )"
  ((PICKER_LAYOUT_CURRENT > PICKER_LAYOUT_BASELINE)) && break
  sleep 0.05
done
((PICKER_LAYOUT_CURRENT > PICKER_LAYOUT_BASELINE)) || {
  echo "ERR backend picker did not open from the in-client New action" >&2
  exit 1
}
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  wtype -s 2 "Connect remote backend" -k Return
sleep 1
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  wtype -s 2 "$TARGET" -k Return

for _ in $(seq 1 3600); do
  rg -a -q 'connected in-client SSH backend=' "$DEPLOY_LOG" && break
  kill -0 "$DEPLOY_PID" 2>/dev/null || {
    echo "ERR presentation exited during in-client SSH deployment" >&2
    exit 1
  }
  sleep 0.25
done
rg -a -q 'connected in-client SSH backend=' "$DEPLOY_LOG" || {
  echo "ERR timed out waiting for in-client SSH deployment readiness" >&2
  exit 1
}

for _ in $(seq 1 1200); do
  mapfile -t BACKENDS < <(
    rg -a -o 'backend actor message backend=[0-9]+' "$DEPLOY_LOG" |
      sed 's/.*=//' |
      sort -nu
  )
  if ((${#BACKENDS[@]} >= 2)) \
    && rg -a -q 'started presentation WASM actor id=terminal-ui kind=' "$DEPLOY_LOG" \
    && rg -a -q 'started presentation WASM actor id=terminal-ui-b[0-9]+-s[0-9]+ kind=' "$DEPLOY_LOG" \
    && [[ -n "$(presentation_ids)" ]]; then
    break
  fi
  kill -0 "$DEPLOY_PID" 2>/dev/null || {
    echo "ERR SSH deployment exited before both backend realms became interactive" >&2
    exit 1
  }
  sleep 0.1
done

mapfile -t BACKENDS < <(
  rg -a -o 'backend actor message backend=[0-9]+' "$DEPLOY_LOG" |
    sed 's/.*=//' |
    sort -nu
)
((${#BACKENDS[@]} >= 2)) || {
  echo "ERR fewer than two backends delivered realm-qualified actor traffic" >&2
  exit 1
}
rg -a -q 'started presentation WASM actor id=terminal-ui kind=' "$DEPLOY_LOG"
rg -a -q 'started presentation WASM actor id=terminal-ui-b[0-9]+-s[0-9]+ kind=' "$DEPLOY_LOG"
if rg -a -q 'dropped backend actor message' "$DEPLOY_LOG"; then
  echo "ERR a backend actor message was rejected during the multi-backend proof" >&2
  exit 1
fi
mapfile -t SESSION_ROWS < <(
  sed -n \
    's/.*backend session catalogue backend=\([0-9][0-9]*\) active=[0-9][0-9]* name="\([^"]*\)".*/\1 \2/p' \
    "$DEPLOY_LOG" |
    awk '!($1 in names) { names[$1] = $2 } END { for (backend in names) print backend, names[backend] }' |
    sort -n
)
((${#SESSION_ROWS[@]} >= 2)) || {
  echo "ERR fewer than two backend session names were published" >&2
  exit 1
}
[[ "${SESSION_ROWS[0]#* }" != "${SESSION_ROWS[1]#* }" ]] || {
  echo "ERR local and remote backends generated the same default session name" >&2
  exit 1
}

READY="$(
  rg -a 'ssh-deploy=ready ' "$DEPLOY_LOG" |
    tail -1
)"
ACTORS="$(
  rg -a -o 'started presentation WASM actor id=terminal-ui[^ ]*' "$DEPLOY_LOG" |
    sed 's/.*id=//' |
    sort -u |
    jq -Rsc 'split("\n") | map(select(length > 0))'
)"
BACKEND_JSON="$(
  printf '%s\n' "${BACKENDS[@]}" |
    jq -Rsc 'split("\n") | map(select(length > 0) | tonumber)'
)"
SESSION_NAMES_JSON="$(
  printf '%s\n' "${SESSION_ROWS[@]}" |
    jq -Rsc '
      split("\n")
      | map(select(length > 0) | split(" "))
      | map({backend: (.[0] | tonumber), name: .[1]})
    '
)"
REMOTE_BACKEND="$(
  rg -a -o 'connected in-client SSH backend=BackendId\([0-9]+\)' \
    "$DEPLOY_LOG" |
    sed -n 's/.*BackendId(\([0-9][0-9]*\)).*/\1/p' |
    head -1
)"
[[ -n "$REMOTE_BACKEND" ]] || {
  echo "ERR could not identify the network backend" >&2
  exit 1
}
LOCAL_BACKEND="$(
  printf '%s\n' "${BACKENDS[@]}" |
    rg -v "^${REMOTE_BACKEND}$" |
    head -1
)"
[[ -n "$LOCAL_BACKEND" ]] || {
  echo "ERR could not identify the ordinary local backend" >&2
  exit 1
}

# The backend actor enumerates the remote host before opening the picker.
# Fuzzy-match the exact home path, preserve a screenshot of the filtered
# picker, and select it to create a session rooted there.
REMOTE_SESSION_NAME="${REMOTE_HOME##*/}"
for _ in $(seq 1 600); do
  if rg -a -q \
    "backend actor message backend=${REMOTE_BACKEND} realm=Daemon .*recipient=session-switcher body=.*${REMOTE_HOME}.*\"success\":true,\"type\":\"directory-list\"" \
    "$DEPLOY_LOG"; then
    break
  fi
  sleep 0.1
done
rg -a -q \
  "backend actor message backend=${REMOTE_BACKEND} realm=Daemon .*recipient=session-switcher body=.*${REMOTE_HOME}.*\"success\":true,\"type\":\"directory-list\"" \
  "$DEPLOY_LOG" || {
  echo "ERR remote backend did not return $REMOTE_HOME in its directory catalogue" >&2
  exit 1
}
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  wtype -s 2 "$REMOTE_HOME"
sleep 0.2
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  grim -o HEADLESS-1 "$DIRECTORY_SCREENSHOT"
[[ -s "$DIRECTORY_SCREENSHOT" ]]
REMOTE_LAYOUT_BASELINE="$(
  rg -a -c 'accepted leased actor layout items=2 panes=2 ' "$DEPLOY_LOG" || true
)"
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  "$OUT/virtual-keyboard-key" --none 28

for _ in $(seq 1 600); do
  REMOTE_LAYOUT_CURRENT="$(
    rg -a -c 'accepted leased actor layout items=2 panes=2 ' "$DEPLOY_LOG" || true
  )"
  if rg -a -q \
    "backend session catalogue backend=${REMOTE_BACKEND} active=[0-9]+ name=\"${REMOTE_SESSION_NAME}\"" \
    "$DEPLOY_LOG" \
    && ((REMOTE_LAYOUT_CURRENT > REMOTE_LAYOUT_BASELINE)); then
    break
  fi
  sleep 0.1
done
rg -a -q \
  "backend session catalogue backend=${REMOTE_BACKEND} active=[0-9]+ name=\"${REMOTE_SESSION_NAME}\"" \
  "$DEPLOY_LOG" || {
  echo "ERR fuzzy directory selection did not create the remote session in $REMOTE_HOME" >&2
  exit 1
}
((REMOTE_LAYOUT_CURRENT > REMOTE_LAYOUT_BASELINE))
REMOTE_CREATED_SESSION_ID="$(
  sed -n \
    "s/.*backend session catalogue backend=${REMOTE_BACKEND} active=\\([0-9][0-9]*\\) name=\"${REMOTE_SESSION_NAME}\".*/\\1/p" \
    "$DEPLOY_LOG" |
    tail -1
)"
[[ -n "$REMOTE_CREATED_SESSION_ID" ]]

# Switch back through the real session fuzzy picker. Remote terminal actors
# stay alive, but only the selected local session may appear in the default
# layout.
#
# The directory picker closes before the newly created terminal actor has
# necessarily finished its deferred focus/layout transition. Do not inject a
# global shortcut into that transition.
for _ in $(seq 1 300); do
  if rg -a -q \
    "started presentation WASM actor id=terminal-ui-b${REMOTE_BACKEND}-s${REMOTE_CREATED_SESSION_ID} kind=" \
    "$DEPLOY_LOG" \
    && sed -n \
      "/started presentation WASM actor id=terminal-ui-b${REMOTE_BACKEND}-s${REMOTE_CREATED_SESSION_ID} kind=/,\$p" \
      "$DEPLOY_LOG" \
      | rg -a -q 'sent plugin pane resize .* logical_size=960x720 '; then
    break
  fi
  sleep 0.05
done
rg -a -q \
  "started presentation WASM actor id=terminal-ui-b${REMOTE_BACKEND}-s${REMOTE_CREATED_SESSION_ID} kind=" \
  "$DEPLOY_LOG"
sed -n \
  "/started presentation WASM actor id=terminal-ui-b${REMOTE_BACKEND}-s${REMOTE_CREATED_SESSION_ID} kind=/,\$p" \
  "$DEPLOY_LOG" \
  | rg -a -q 'sent plugin pane resize .* logical_size=960x720 '
sleep 0.2

# Exercise steady-state printable echo through the actual remote terminal actor
# at a rate below the display refresh. This produces backend-qualified
# key-to-present samples without collapsing many keys into one frame.
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  "$OUT/virtual-pointer-input" drag 100 100 100 100 1 1280 720
if [[ "$PROFILE" == 1 ]]; then
  process_profile_snapshot "$DEPLOY_PID" > "$CLIENT_PROCESS_BEFORE"
  ssh -T -o BatchMode=yes "$TARGET" '
    process_id="$(pgrep -n -x brood-remote || true)"
    echo "pid=$process_id"
    if [[ -n "$process_id" ]]; then
      echo "schedstat=$(<"/proc/$process_id/schedstat")"
      sed -n \
        "/^voluntary_ctxt_switches:/p; /^nonvoluntary_ctxt_switches:/p; /^VmRSS:/p; /^Threads:/p" \
        "/proc/$process_id/status"
      echo "stat=$(<"/proc/$process_id/stat")"
    fi
  ' > "$REMOTE_PROCESS_BEFORE"
  PROFILE_TIDS="$(
    printf '%s\n' "/proc/$DEPLOY_PID"/task/* |
      sed 's|.*/||' |
      paste -sd,
  )"
  perf stat \
    -x, \
    -o "$CLIENT_PERF_STAT" \
    -e task-clock,context-switches,cpu-migrations,page-faults,cycles,instructions \
    -t "$PROFILE_TIDS" \
    -- sleep 4 &
  PROFILE_PERF_PID=$!
  if [[ "${BROOD_SSH_MULTI_BACKEND_PERF_RECORD:-0}" == 1 ]]; then
    perf record \
      -q \
      -o "$CLIENT_PERF_DATA" \
      -g \
      --call-graph dwarf \
      -t "$PROFILE_TIDS" \
      -- sleep 4 &
    PROFILE_RECORD_PID=$!
  fi
  ROUTE_HOST="$(
    sed -n 's/.* connect=\([^: ]*\):[0-9][0-9]* .*/\1/p' <<< "$READY"
  )"
  ping -D -n -i 0.01 -c 400 "$ROUTE_HOST" > "$ROUTE_PING" 2>&1 &
  PROFILE_PING_PID=$!
fi
REMOTE_LATENCY_TEXT="$(printf 'a%.0s' {1..80})"
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  wtype -d "${BROOD_SSH_MULTI_BACKEND_KEY_DELAY_MS:-30}" "$REMOTE_LATENCY_TEXT"
sleep 0.5
if [[ "$PROFILE" == 1 ]]; then
  wait "$PROFILE_PERF_PID"
  PROFILE_PERF_PID=""
  if [[ -n "$PROFILE_RECORD_PID" ]]; then
    wait "$PROFILE_RECORD_PID"
    PROFILE_RECORD_PID=""
    perf report \
      -i "$CLIENT_PERF_DATA" \
      --stdio \
      --no-children \
      --percent-limit 0.1 \
      > "$CLIENT_PERF_REPORT"
  fi
  wait "$PROFILE_PING_PID"
  PROFILE_PING_PID=""
  process_profile_snapshot "$DEPLOY_PID" > "$CLIENT_PROCESS_AFTER"
  ssh -T -o BatchMode=yes "$TARGET" '
    process_id="$(pgrep -n -x brood-remote || true)"
    echo "pid=$process_id"
    if [[ -n "$process_id" ]]; then
      echo "schedstat=$(<"/proc/$process_id/schedstat")"
      sed -n \
        "/^voluntary_ctxt_switches:/p; /^nonvoluntary_ctxt_switches:/p; /^VmRSS:/p; /^Threads:/p" \
        "/proc/$process_id/status"
      echo "stat=$(<"/proc/$process_id/stat")"
    fi
  ' > "$REMOTE_PROCESS_AFTER"
fi

LOCAL_SESSION_NAME="$(
  printf '%s\n' "${SESSION_ROWS[@]}" |
    awk -v backend="$LOCAL_BACKEND" '$1 == backend { print $2; exit }'
)"
[[ -n "$LOCAL_SESSION_NAME" ]]
LOCAL_LAYOUT_BASELINE="$(
  rg -a -c 'accepted leased actor layout items=2 panes=2 ' "$DEPLOY_LOG" || true
)"
SESSION_PICKER_BASELINE="$(
  rg -a -c 'accepted leased actor layout items=3 panes=3 ' "$DEPLOY_LOG" || true
)"
ACTIVATE_COMPLETION_BASELINE="$(
  rg -a -c \
    'presentation session command completed actor=session-switcher .* operation=Activate success=true ' \
    "$DEPLOY_LOG" || true
)"
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  "$OUT/virtual-keyboard-key" 25
for _ in $(seq 1 100); do
  SESSION_PICKER_CURRENT="$(
    rg -a -c 'accepted leased actor layout items=3 panes=3 ' "$DEPLOY_LOG" || true
  )"
  ((SESSION_PICKER_CURRENT > SESSION_PICKER_BASELINE)) && break
  sleep 0.05
done
((SESSION_PICKER_CURRENT > SESSION_PICKER_BASELINE)) || {
  echo "ERR session picker did not open before switching back locally" >&2
  exit 1
}
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  wtype -s 2 "$LOCAL_SESSION_NAME"
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  "$OUT/virtual-keyboard-key" --none 28
for _ in $(seq 1 300); do
  LOCAL_LAYOUT_CURRENT="$(
    rg -a -c 'accepted leased actor layout items=2 panes=2 ' "$DEPLOY_LOG" || true
  )"
  ACTIVATE_COMPLETION_CURRENT="$(
    rg -a -c \
      'presentation session command completed actor=session-switcher .* operation=Activate success=true ' \
      "$DEPLOY_LOG" || true
  )"
  if ((LOCAL_LAYOUT_CURRENT > LOCAL_LAYOUT_BASELINE)) \
    && ((ACTIVATE_COMPLETION_CURRENT > ACTIVATE_COMPLETION_BASELINE)); then
    break
  fi
  sleep 0.1
done
((LOCAL_LAYOUT_CURRENT > LOCAL_LAYOUT_BASELINE))
((ACTIVATE_COMPLETION_CURRENT > ACTIVATE_COMPLETION_BASELINE))

# Input after the switch must reach the local terminal actor. This both
# identifies which same-sized two-pane composition won and proves the live
# remote terminal actor is not stealing input while inactive.
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  "$OUT/virtual-pointer-input" drag 100 100 100 100 1 1280 720
sleep 0.2
INPUT_LOG_LINE="$(wc -l < "$DEPLOY_LOG")"
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  wtype -s 2 "echo LOCAL_SCOPE_OK" -k Return
for _ in $(seq 1 100); do
  if tail -n "+$((INPUT_LOG_LINE + 1))" "$DEPLOY_LOG" \
    | rg -a -q \
      'presentation actor message realm=Session\([0-9]+\) sender=terminal-ui recipient=terminal-session body=I'; then
    break
  fi
  sleep 0.05
done
tail -n "+$((INPUT_LOG_LINE + 1))" "$DEPLOY_LOG" \
  | rg -a -q \
    'presentation actor message realm=Session\([0-9]+\) sender=terminal-ui recipient=terminal-session body=I'
if tail -n "+$((INPUT_LOG_LINE + 1))" "$DEPLOY_LOG" \
  | rg -a -q \
    'presentation actor message realm=Session\([0-9]+\) sender=terminal-ui-b[0-9]+-s[0-9]+ recipient=terminal-session body=I'; then
  echo "ERR inactive remote terminal actor received input after switching locally" >&2
  exit 1
fi

env XDG_RUNTIME_DIR="$RUNTIME_DIR" SWAYSOCK="$SWAYSOCK" \
  swaymsg -t get_tree -r > "$TREE_JSON"
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  grim -o HEADLESS-1 "$SCREENSHOT"
[[ -s "$SCREENSHOT" ]]

close_presentation
for _ in $(seq 1 300); do
  kill -0 "$DEPLOY_PID" 2>/dev/null || break
  sleep 0.1
done
if kill -0 "$DEPLOY_PID" 2>/dev/null; then
  echo "ERR presentation did not close cleanly" >&2
  exit 1
fi
set +e
wait "$DEPLOY_PID"
DEPLOY_STATUS=$?
set -e
DEPLOY_PID=""
((DEPLOY_STATUS == 0)) || {
  echo "ERR SSH deployment exited with status $DEPLOY_STATUS" >&2
  exit 1
}

INPUT_METRIC="$(
  sed -n 's/^plugin input diagnostics //p' "$DEPLOY_LOG" |
    tail -1
)"
REMOTE_BACKEND_METRIC="$(
  jq -c \
    --argjson backend "$REMOTE_BACKEND" \
    '.by_backend[] | select(.backend == $backend)' \
    <<< "$INPUT_METRIC"
)"
REMOTE_INPUT_METRIC="$(jq -c '.latency' <<< "$REMOTE_BACKEND_METRIC")"
REMOTE_STAGE_METRIC="$(jq -c '.stages' <<< "$REMOTE_BACKEND_METRIC")"
REMOTE_TRACE_METRIC="$(jq -c '.traces' <<< "$REMOTE_BACKEND_METRIC")"
REMOTE_RELAY_METRIC="$(
  sed -n 's/.*remote terminal relay diagnostics //p' "$DEPLOY_LOG" |
    tail -1
)"
REMOTE_QUIC_METRIC="$(
  sed -n 's/.*remote relay QUIC diagnostics //p' "$DEPLOY_LOG" |
    tail -1
)"
REMOTE_RELAY_METRIC="${REMOTE_RELAY_METRIC:-null}"
REMOTE_QUIC_METRIC="${REMOTE_QUIC_METRIC:-null}"
if [[ "$PROFILE" != 1 ]]; then
  jq -e '
  .key_to_present_samples >= 20
  and .p95_key_to_present_us > 0
  and .p95_key_to_present_us <= 20000
  and .p99_key_to_present_us > 0
  and .p99_key_to_present_us <= 25000
  and .stddev_key_to_present_us >= 0
  and .stddev_key_to_present_us <= 6000
  ' <<< "$REMOTE_INPUT_METRIC" > /dev/null
  jq -e '
  .remote_round_trip.samples >= 20
  and .remote_round_trip.p95_us <= 12000
  and .remote_round_trip.p99_us <= 20000
  and .remote_round_trip.stddev_us <= 5000
  and .plugin_render.samples >= 20
  and .plugin_render.p95_us <= 15000
  and .plugin_render.p99_us <= 17000
  and .plugin_render.stddev_us <= 6000
  and .presentation.samples >= 20
  and .presentation.p95_us <= 8000
  and .presentation.p99_us <= 16667
  and .presentation.stddev_us <= 4000
  ' <<< "$REMOTE_STAGE_METRIC" > /dev/null
fi

jq -n \
  --arg target "$TARGET" \
  --arg ready "$READY" \
  --arg screenshot "$SCREENSHOT" \
  --arg directory_screenshot "$DIRECTORY_SCREENSHOT" \
  --arg remote_directory "$REMOTE_HOME" \
  --argjson remote_backend_id "$REMOTE_BACKEND" \
  --argjson local_backend_id "$LOCAL_BACKEND" \
  --argjson backend_ids "$BACKEND_JSON" \
  --argjson session_names "$SESSION_NAMES_JSON" \
  --argjson presentation_actors "$ACTORS" \
  --argjson remote_terminal_latency "$REMOTE_INPUT_METRIC" \
  --argjson remote_terminal_stages "$REMOTE_STAGE_METRIC" \
  --argjson remote_terminal_traces "$REMOTE_TRACE_METRIC" \
  --argjson remote_relay_stages "$REMOTE_RELAY_METRIC" \
  --argjson remote_quic "$REMOTE_QUIC_METRIC" \
  --arg profile "$PROFILE" \
  --arg client_perf_stat "$CLIENT_PERF_STAT" \
  --arg route_ping "$ROUTE_PING" \
  '{
    target: $target,
    deployment_ready: $ready,
    backend_ids: $backend_ids,
    remote_backend_id: $remote_backend_id,
    local_backend_id: $local_backend_id,
    session_names: $session_names,
    presentation_actors: $presentation_actors,
    remote_terminal_latency: $remote_terminal_latency,
    remote_terminal_stages: $remote_terminal_stages,
    remote_terminal_traces: $remote_terminal_traces,
    remote_relay_stages: $remote_relay_stages,
    remote_quic: $remote_quic,
    local_and_remote_actor_messages: ($backend_ids | length >= 2),
    realm_scoped_frontends: ($presentation_actors | length >= 2),
    in_client_ssh_connection: true,
    session_scoped_default_visibility: true,
    remote_directory_fuzzy_selection: true,
    remote_directory: $remote_directory,
    randomized_session_names: true,
    rejected_backend_actor_messages: false,
    clean_remote_shutdown: true,
    profiling: ($profile == "1"),
    client_perf_stat: $client_perf_stat,
    route_ping: $route_ping,
    screenshot: $screenshot,
    directory_screenshot: $directory_screenshot
  }' > "$PROOF"

echo "ssh_multi_backend=verified target=$TARGET backends=${BACKENDS[*]}"
echo "proof=$PROOF"
echo "screenshot=$SCREENSHOT"
echo "directory_screenshot=$DIRECTORY_SCREENSHOT"
