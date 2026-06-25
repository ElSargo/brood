#!/usr/bin/env bash
set -euo pipefail

: "${MANIFOLDD_BIN:?set MANIFOLDD_BIN to the manifoldd executable}"
: "${HARNESS_WASM:?set HARNESS_WASM to the compiled WASM harness}"
: "${FIREFOX_BIN:?set FIREFOX_BIN to the firefox executable}"
: "${WESTON_SIMPLE_SHM:?set WESTON_SIMPLE_SHM to weston-simple-shm}"

WORKDIR="${WORKDIR:-$(mktemp -d /tmp/manifold-firefox-close-reattach.XXXXXX)}"
OUT_DIR="$WORKDIR/nested"
BIN_DIR="$WORKDIR/bin"
HOME_DIR="$WORKDIR/home"
RUNTIME_DIR="$WORKDIR/runtime"
FIREFOX_PROFILE="$WORKDIR/firefox-profile"
HOST_DISPLAY="${DISPLAY:-}"
HOST_WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-}"
HOST_XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-}"

mkdir -p "$OUT_DIR" "$BIN_DIR" "$HOME_DIR" "$RUNTIME_DIR" "$FIREFOX_PROFILE"
chmod 700 "$RUNTIME_DIR"
SHELL_BIN="${BASH:-$(command -v bash)}"

cat >"$BIN_DIR/firefox" <<EOF
#!$SHELL_BIN
exec "$FIREFOX_BIN" \\
  --new-instance \\
  --no-remote \\
  --profile "$FIREFOX_PROFILE" \\
  about:blank
EOF
chmod +x "$BIN_DIR/firefox"

cat >"$BIN_DIR/weston-simple-shm" <<EOF
#!$SHELL_BIN
exec "$WESTON_SIMPLE_SHM"
EOF
chmod +x "$BIN_DIR/weston-simple-shm"

export PATH="$BIN_DIR:$PATH"
export HOME="$HOME_DIR"
export XDG_CACHE_HOME="$WORKDIR/cache"
export XDG_CONFIG_HOME="$WORKDIR/config"
export XDG_DATA_HOME="$WORKDIR/share"
if [[ "${OUTER_HEADFUL:-0}" == "1" ]]; then
  export DISPLAY="$HOST_DISPLAY"
  export WAYLAND_DISPLAY="$HOST_WAYLAND_DISPLAY"
  export XDG_RUNTIME_DIR="$HOST_XDG_RUNTIME_DIR"
else
  export DISPLAY=
  export WAYLAND_DISPLAY=
  export XDG_RUNTIME_DIR="$RUNTIME_DIR"
fi
export GDK_BACKEND=wayland
export MOZ_ENABLE_WAYLAND=1
export MOZ_DBUS_REMOTE=0
export MOZ_DISABLE_CONTENT_SANDBOX=1
export MOZ_DISABLE_GMP_SANDBOX=1
export MOZ_DISABLE_GPU_SANDBOX=1
export MOZ_DISABLE_RDD_SANDBOX=1
export RUST_LOG="${RUST_LOG:-manifoldd=info,smithay=warn}"
TEST_TIMEOUT="${TEST_TIMEOUT:-120s}"

cleanup() {
  :
}
trap cleanup EXIT

TEST_ARGS=(
  test nested
  --force-outer
  --out-dir "$OUT_DIR"
  --harness "$HARNESS_WASM"
)

if [[ "${OUTER_HEADFUL:-0}" == "1" ]]; then
  TEST_ARGS+=(--outer-headful)
fi

if [[ "${INNER_HEADLESS:-0}" == "1" ]]; then
  TEST_ARGS+=(--inner-headless)
fi

timeout --kill-after=5s "$TEST_TIMEOUT" \
  "$MANIFOLDD_BIN" "${TEST_ARGS[@]}" \
  >"$WORKDIR/test.log" 2>&1 || {
    status=$?
    echo "ERR firefox close/reattach nested test failed" >&2
    cat "$WORKDIR/test.log" >&2 || true
    tail -200 "$OUT_DIR/outer.log" >&2 || true
    tail -200 "$OUT_DIR/inner.log" >&2 || true
    exit "$status"
  }

LOG_FILES=("$WORKDIR/test.log" "$OUT_DIR/outer.log" "$OUT_DIR/inner.log")

if grep -n 'TEST_HARNESS_RESULT kind=Failed\|TEST_HARNESS_RESULT kind=Paused' "${LOG_FILES[@]}" >/dev/null 2>&1; then
  echo "ERR WASM test harness did not pass" >&2
  grep -n 'TEST_HARNESS_RESULT' "${LOG_FILES[@]}" >&2 || true
  tail -200 "$OUT_DIR/outer.log" >&2 || true
  tail -200 "$OUT_DIR/inner.log" >&2 || true
  exit 1
fi

if ! grep -n 'TEST_HARNESS_RESULT kind=Passed' "${LOG_FILES[@]}" >/dev/null 2>&1; then
  echo "ERR missing passing WASM test harness result" >&2
  tail -200 "$WORKDIR/test.log" >&2 || true
  tail -200 "$OUT_DIR/outer.log" >&2 || true
  tail -200 "$OUT_DIR/inner.log" >&2 || true
  exit 1
fi

ERRORS="$WORKDIR/protocol-errors.log"
if grep -n -E 'create_immed failed|fatal display error: Protocol error|error in client communication' "${LOG_FILES[@]}" >"$ERRORS" 2>/dev/null; then
  echo "ERR Wayland protocol error detected during Firefox close/reattach test" >&2
  cat "$ERRORS" >&2
  tail -200 "$OUT_DIR/outer.log" >&2 || true
  tail -200 "$OUT_DIR/inner.log" >&2 || true
  exit 1
fi

echo "OK firefox close/reattach nested test passed"
echo "workdir=$WORKDIR"
echo "outer_log=$OUT_DIR/outer.log"
echo "inner_log=$OUT_DIR/inner.log"
