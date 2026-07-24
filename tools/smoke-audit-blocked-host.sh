#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TMPDIR="$(mktemp -d /tmp/manifold-audit-blocked-host.XXXXXX)"
RUNTIME_DIR="$TMPDIR/runtime-dir"
LOG="$TMPDIR/audit-blocked-host.log"
FAIL_LOG="$TMPDIR/audit-blocked-host-runnable-failure.log"
FAKEBIN="$TMPDIR/fakebin"
mkdir -p "$RUNTIME_DIR"

set +e
env \
  XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY=missing-wayland-0 \
  DISPLAY=:99 \
  ./tools/audit-v1-capabilities.sh > "$LOG" 2>&1
RC=$?
set -e

OVERALL="$(grep '^overall=' "$LOG" | tail -1 || true)"
if [[ "$OVERALL" != overall=blocked_host_display_unavailable\ failures=0\ skips=0* ]]; then
  echo "ERR expected blocked host display with no runnable failures, got rc=$RC overall=${OVERALL:-none}" >&2
  tail -160 "$LOG" >&2 || true
  exit 1
fi
if [[ "$RC" == "0" ]]; then
  echo "ERR blocked-host audit unexpectedly exited rc=0" >&2
  tail -160 "$LOG" >&2 || true
  exit 1
fi

for check in cargo_fmt cargo_check cargo_test headless_wayland_globals headless_native_client headless_input_forwarding headless_control_errors headless_status_json headless_status_parity headless_client_exit headless_multi_client headless_multi_client_input_focus dmabuf_status_validator dmabuf_backend_classifier; do
  grep -q "^check=$check result=pass rc=0 " "$LOG" || {
    echo "ERR expected $check to pass before host block" >&2
    tail -160 "$LOG" >&2 || true
    exit 1
  }
done

for req in rust_baseline nested_wayland_globals native_wayland_client_nesting input_forwarding dmabuf_validator_logic; do
  grep -q "^requirement=$req result=pass " "$LOG" || {
    echo "ERR expected requirement $req to pass in blocked-host audit" >&2
    tail -160 "$LOG" >&2 || true
    exit 1
  }
done

for req in frontend_window_rendering close_frontend_keep_client_alive reopen_frontend_resume_client low_latency_cadence dmabuf_advertisement_correctness positive_dmabuf_import positive_explicit_sync_fences; do
  grep -q "^requirement=$req result=blocked .*host display unavailable before full audit" "$LOG" || {
    echo "ERR expected requirement $req to be host-blocked" >&2
    tail -160 "$LOG" >&2 || true
    exit 1
  }
done

grep -q '^check=host_presentation_preflight result=blocked rc=1 ' "$LOG" || {
  echo "ERR missing blocked host preflight check" >&2
  tail -160 "$LOG" >&2 || true
  exit 1
}
grep -q '^  host_display_diagnostic=begin$' "$LOG" || {
  echo "ERR missing host display diagnostic start" >&2
  tail -160 "$LOG" >&2 || true
  exit 1
}
grep -q "^  wayland_socket=$RUNTIME_DIR/missing-wayland-0 socket=no exists=no$" "$LOG" || {
  echo "ERR missing forced Wayland socket diagnostic" >&2
  tail -160 "$LOG" >&2 || true
  exit 1
}
grep -q '^  x11_socket=/tmp/.X11-unix/X99 socket=no exists=no$' "$LOG" || {
  echo "ERR missing forced X11 socket diagnostic" >&2
  tail -160 "$LOG" >&2 || true
  exit 1
}
grep -q '^  host_display_diagnostic=end$' "$LOG" || {
  echo "ERR missing host display diagnostic end" >&2
  tail -160 "$LOG" >&2 || true
  exit 1
}

REAL_CARGO="$(command -v cargo)"
mkdir -p "$FAKEBIN"
cat > "$FAKEBIN/cargo" << EOF
#!/usr/bin/env bash
if [[ "\${1:-}" == "fmt" ]]; then
  echo "forced cargo fmt failure for blocked-host audit precedence" >&2
  exit 42
fi
exec "$REAL_CARGO" "\$@"
EOF
chmod +x "$FAKEBIN/cargo"

set +e
env \
  PATH="$FAKEBIN:$PATH" \
  XDG_RUNTIME_DIR="$RUNTIME_DIR" \
  WAYLAND_DISPLAY=missing-wayland-0 \
  DISPLAY=:99 \
  ./tools/audit-v1-capabilities.sh > "$FAIL_LOG" 2>&1
FAIL_RC=$?
set -e

FAIL_OVERALL="$(grep '^overall=' "$FAIL_LOG" | tail -1 || true)"
if [[ "$FAIL_OVERALL" != overall=fail\ failures=1\ skips=0\ host_display=unavailable* ]]; then
  echo "ERR expected runnable failure to take precedence over host block, got rc=$FAIL_RC overall=${FAIL_OVERALL:-none}" >&2
  tail -180 "$FAIL_LOG" >&2 || true
  exit 1
fi
if [[ "$FAIL_RC" == "0" ]]; then
  echo "ERR runnable-failure audit unexpectedly exited rc=0" >&2
  tail -180 "$FAIL_LOG" >&2 || true
  exit 1
fi
grep -q '^check=cargo_fmt result=fail rc=42 ' "$FAIL_LOG" || {
  echo "ERR missing forced cargo_fmt failure" >&2
  tail -180 "$FAIL_LOG" >&2 || true
  exit 1
}
grep -q '^check=host_presentation_preflight result=blocked rc=1 ' "$FAIL_LOG" || {
  echo "ERR missing host preflight block in runnable-failure audit" >&2
  tail -180 "$FAIL_LOG" >&2 || true
  exit 1
}
grep -q '^requirement=rust_baseline result=fail ' "$FAIL_LOG" || {
  echo "ERR expected rust_baseline requirement to fail" >&2
  tail -180 "$FAIL_LOG" >&2 || true
  exit 1
}

echo "OK blocked-host audit smoke passed"
echo "audit_result=$OVERALL"
echo "failure_precedence_result=$FAIL_OVERALL"
echo "rc=$RC"
echo "failure_precedence_rc=$FAIL_RC"
echo "log=$LOG"
echo "failure_precedence_log=$FAIL_LOG"
