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

for tool in cargo grim jq nix rg ssh sway swaymsg; do
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
SCREENSHOT="$OUT/local-and-remote.png"
PROOF="$OUT/proof.json"
mkdir -p "$OUT" "$RUNTIME_DIR"
chmod 700 "$RUNTIME_DIR"

COMPOSITOR_PID=""
DEPLOY_PID=""

presentation_ids() {
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" SWAYSOCK="$SWAYSOCK" \
    swaymsg -t get_tree -r 2>/dev/null |
    jq -r '.. | objects | select(.type? == "con" and .pid? != null) | .id'
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

cargo build -p brood-remote -p broodd --bins --locked

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

DEPLOY_ARGS=(ssh "$TARGET")
if [[ -n "$CONNECT_HOST" ]]; then
  DEPLOY_ARGS+=(--connect-host "$CONNECT_HOST")
fi
env \
  -u brood_session \
  -u BROOD_SURFACE_SESSION_ID \
  -u BROOD_PANE_ID \
  XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  WGPU_BACKEND=vulkan \
  BROOD_TRACE_PLUGIN_INPUT=1 \
  RUST_LOG="${RUST_LOG:-broodd=info,brood_client=info,smithay=warn,wgpu_core=warn,wgpu_hal=warn}" \
  target/debug/brood-remote "${DEPLOY_ARGS[@]}" \
  > "$DEPLOY_LOG" 2>&1 &
DEPLOY_PID=$!

for _ in $(seq 1 3600); do
  rg -a -q 'ssh-deploy=ready ' "$DEPLOY_LOG" && break
  kill -0 "$DEPLOY_PID" 2>/dev/null || {
    echo "ERR SSH deployment exited before publishing its listener" >&2
    exit 1
  }
  sleep 0.25
done
rg -a -q 'ssh-deploy=ready ' "$DEPLOY_LOG" || {
  echo "ERR timed out waiting for SSH deployment readiness" >&2
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

env XDG_RUNTIME_DIR="$RUNTIME_DIR" SWAYSOCK="$SWAYSOCK" \
  swaymsg -t get_tree -r > "$TREE_JSON"
env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
  grim -o HEADLESS-1 "$SCREENSHOT"
[[ -s "$SCREENSHOT" ]]

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
REMOTE_BACKEND="$(
  rg -a -o 'selected presentation backend=BackendId\([0-9]+\) transport carrier=Network' \
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

jq -n \
  --arg target "$TARGET" \
  --arg ready "$READY" \
  --arg screenshot "$SCREENSHOT" \
  --argjson remote_backend_id "$REMOTE_BACKEND" \
  --argjson local_backend_id "$LOCAL_BACKEND" \
  --argjson backend_ids "$BACKEND_JSON" \
  --argjson presentation_actors "$ACTORS" \
  '{
    target: $target,
    deployment_ready: $ready,
    backend_ids: $backend_ids,
    remote_backend_id: $remote_backend_id,
    local_backend_id: $local_backend_id,
    presentation_actors: $presentation_actors,
    local_and_remote_actor_messages: ($backend_ids | length >= 2),
    realm_scoped_frontends: ($presentation_actors | length >= 2),
    rejected_backend_actor_messages: false,
    clean_remote_shutdown: true,
    screenshot: $screenshot
  }' > "$PROOF"

echo "ssh_multi_backend=verified target=$TARGET backends=${BACKENDS[*]}"
echo "proof=$PROOF"
echo "screenshot=$SCREENSHOT"
