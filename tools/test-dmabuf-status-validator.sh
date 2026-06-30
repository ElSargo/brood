#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TMPDIR="$(mktemp -d /tmp/manifold-dmabuf-validator.XXXXXX)"
trap 'rm -rf "$TMPDIR"' EXIT

run_validator() {
  local status_json="$1"
  local trace="$2"
  local require_explicit_sync="$3"
  local expected_rc="$4"
  local name="$5"
  local trace_path="$TMPDIR/$name.trace"
  local output_path="$TMPDIR/$name.out"

  printf '%s\n' "$trace" > "$trace_path"
  set +e
  python3 "$ROOT/tools/validate-dmabuf-status.py" \
    --status-json "$status_json" \
    --trace "$trace_path" \
    --require-explicit-sync "$require_explicit_sync" \
    > "$output_path" 2>&1
  local rc=$?
  set -e

  if [[ "$rc" != "$expected_rc" ]]; then
    echo "ERR $name expected rc=$expected_rc got rc=$rc" >&2
    cat "$output_path" >&2
    exit 1
  fi
}

DISABLED_STATUS='{"dmabuf_formats":0,"dmabuf":"none","dmabuf_disabled":"no_dmabuf_texture_formats","explicit_sync":"none","explicit_sync_disabled":"no_dmabuf_texture_formats","dmabuf_attempts":0,"dmabuf_accepted":0,"dmabuf_rejected":0,"dmabuf_explicit_sync_release_points":0,"dmabuf_explicit_sync_blockers":0,"dmabuf_implicit_blockers":0,"dmabuf_blocker_failures":0,"dmabuf_last_import":"none","dmabuf_last_error":"none","dmabuf_last_blocker_error":"none","egl_wl_bind":"EglExtensionNotSupported_[\"EGL_WL_bind_wayland_display\"]_"}'

POSITIVE_V3_STATUS='{"dmabuf_formats":12,"dmabuf":"v3","dmabuf_disabled":"v3_no_render_node","explicit_sync":"none","explicit_sync_disabled":"v3_no_render_node","windows":1,"frames":3,"dmabuf_attempts":2,"dmabuf_accepted":1,"dmabuf_rejected":0,"dmabuf_last_import":"64x64:XR24:Linear:planes1","dmabuf_blocker_failures":0,"dmabuf_last_blocker_error":"none","dmabuf_explicit_sync_blockers":0,"dmabuf_implicit_blockers":1}'

POSITIVE_V3_DEFAULT_FEEDBACK_STATUS='{"dmabuf_formats":12,"dmabuf":"v3","dmabuf_disabled":"v3_default_feedback_error","explicit_sync":"none","explicit_sync_disabled":"v3_default_feedback_error","windows":1,"frames":3,"dmabuf_attempts":2,"dmabuf_accepted":1,"dmabuf_rejected":0,"dmabuf_last_import":"64x64:XR24:Linear:planes1","dmabuf_blocker_failures":0,"dmabuf_last_blocker_error":"none","dmabuf_explicit_sync_blockers":0,"dmabuf_implicit_blockers":1}'

POSITIVE_V3_MISSING_REASON_STATUS='{"dmabuf_formats":12,"dmabuf":"v3","dmabuf_disabled":"none","explicit_sync":"none","explicit_sync_disabled":"v3_no_render_node","windows":1,"frames":3,"dmabuf_attempts":2,"dmabuf_accepted":1,"dmabuf_rejected":0,"dmabuf_last_import":"64x64:XR24:Linear:planes1","dmabuf_blocker_failures":0,"dmabuf_last_blocker_error":"none","dmabuf_explicit_sync_blockers":0,"dmabuf_implicit_blockers":1}'

POSITIVE_V3_INCOHERENT_SYNC_REASON_STATUS='{"dmabuf_formats":12,"dmabuf":"v3","dmabuf_disabled":"v3_no_render_node","explicit_sync":"none","explicit_sync_disabled":"no_dmabuf_texture_formats","windows":1,"frames":3,"dmabuf_attempts":2,"dmabuf_accepted":1,"dmabuf_rejected":0,"dmabuf_last_import":"64x64:XR24:Linear:planes1","dmabuf_blocker_failures":0,"dmabuf_last_blocker_error":"none","dmabuf_explicit_sync_blockers":0,"dmabuf_implicit_blockers":1}'

POSITIVE_V4_DISABLED_REASON_STATUS='{"dmabuf_formats":12,"dmabuf":"v4","dmabuf_disabled":"v3_no_render_node","explicit_sync":"none","explicit_sync_disabled":"v3_no_render_node","windows":1,"frames":3,"dmabuf_attempts":2,"dmabuf_accepted":1,"dmabuf_rejected":0,"dmabuf_last_import":"64x64:XR24:Linear:planes1","dmabuf_blocker_failures":0,"dmabuf_last_blocker_error":"none","dmabuf_explicit_sync_blockers":0,"dmabuf_implicit_blockers":1}'

POSITIVE_V4_SYNC_UNSUPPORTED_STATUS='{"dmabuf_formats":12,"dmabuf":"v4","dmabuf_disabled":"none","explicit_sync":"none","explicit_sync_disabled":"drm_syncobj_eventfd_unsupported","windows":1,"frames":3,"dmabuf_attempts":2,"dmabuf_accepted":1,"dmabuf_rejected":0,"dmabuf_last_import":"64x64:XR24:Linear:planes1","dmabuf_blocker_failures":0,"dmabuf_last_blocker_error":"none","dmabuf_explicit_sync_blockers":0,"dmabuf_implicit_blockers":1}'

POSITIVE_V4_INCOHERENT_SYNC_REASON_STATUS='{"dmabuf_formats":12,"dmabuf":"v4","dmabuf_disabled":"none","explicit_sync":"none","explicit_sync_disabled":"v3_no_render_node","windows":1,"frames":3,"dmabuf_attempts":2,"dmabuf_accepted":1,"dmabuf_rejected":0,"dmabuf_last_import":"64x64:XR24:Linear:planes1","dmabuf_blocker_failures":0,"dmabuf_last_blocker_error":"none","dmabuf_explicit_sync_blockers":0,"dmabuf_implicit_blockers":1}'

POSITIVE_REJECTED_IMPORT_STATUS='{"dmabuf_formats":12,"dmabuf":"v3","dmabuf_disabled":"v3_no_render_node","explicit_sync":"none","explicit_sync_disabled":"v3_no_render_node","windows":1,"frames":3,"dmabuf_attempts":2,"dmabuf_accepted":1,"dmabuf_rejected":1,"dmabuf_last_import":"64x64:XR24:Linear:planes1","dmabuf_blocker_failures":0,"dmabuf_last_blocker_error":"none","dmabuf_explicit_sync_blockers":0,"dmabuf_implicit_blockers":1}'

POSITIVE_NO_BLOCKER_STATUS='{"dmabuf_formats":12,"dmabuf":"v3","dmabuf_disabled":"v3_no_render_node","explicit_sync":"none","explicit_sync_disabled":"v3_no_render_node","windows":1,"frames":3,"dmabuf_attempts":1,"dmabuf_accepted":1,"dmabuf_rejected":0,"dmabuf_last_import":"64x64:XR24:Linear:planes1","dmabuf_blocker_failures":0,"dmabuf_last_blocker_error":"none","dmabuf_explicit_sync_blockers":0,"dmabuf_implicit_blockers":0}'

POSITIVE_MISSING_FORMATS_STATUS='{"dmabuf":"v3","dmabuf_disabled":"v3_no_render_node","explicit_sync":"none","explicit_sync_disabled":"v3_no_render_node","windows":1,"frames":3,"dmabuf_attempts":2,"dmabuf_accepted":1,"dmabuf_rejected":0,"dmabuf_last_import":"64x64:XR24:Linear:planes1","dmabuf_blocker_failures":0,"dmabuf_last_blocker_error":"none","dmabuf_explicit_sync_blockers":0,"dmabuf_implicit_blockers":1}'

POSITIVE_STRING_COUNTER_STATUS='{"dmabuf_formats":12,"dmabuf":"v3","dmabuf_disabled":"v3_no_render_node","explicit_sync":"none","explicit_sync_disabled":"v3_no_render_node","windows":1,"frames":3,"dmabuf_attempts":"2","dmabuf_accepted":1,"dmabuf_rejected":0,"dmabuf_last_import":"64x64:XR24:Linear:planes1","dmabuf_blocker_failures":0,"dmabuf_last_blocker_error":"none","dmabuf_explicit_sync_blockers":0,"dmabuf_implicit_blockers":1}'

POSITIVE_SYNC_STATUS='{"dmabuf_formats":12,"dmabuf":"v4","dmabuf_disabled":"none","explicit_sync":"v1","explicit_sync_disabled":"none","windows":1,"frames":4,"dmabuf_attempts":2,"dmabuf_accepted":2,"dmabuf_rejected":0,"dmabuf_last_import":"64x64:XR24:Linear:planes1","dmabuf_blocker_failures":0,"dmabuf_last_blocker_error":"none","dmabuf_explicit_sync_blockers":1,"dmabuf_implicit_blockers":0,"dmabuf_explicit_sync_release_points":1}'

SYNC_WITH_V3_STATUS='{"dmabuf_formats":12,"dmabuf":"v3","dmabuf_disabled":"v3_no_render_node","explicit_sync":"v1","explicit_sync_disabled":"none","windows":1,"frames":4,"dmabuf_attempts":2,"dmabuf_accepted":2,"dmabuf_rejected":0,"dmabuf_last_import":"64x64:XR24:Linear:planes1","dmabuf_blocker_failures":0,"dmabuf_last_blocker_error":"none","dmabuf_explicit_sync_blockers":1,"dmabuf_implicit_blockers":0,"dmabuf_explicit_sync_release_points":1}'

MISSING_RELEASE_STATUS='{"dmabuf_formats":12,"dmabuf":"v4","dmabuf_disabled":"none","explicit_sync":"v1","explicit_sync_disabled":"none","windows":1,"frames":4,"dmabuf_attempts":2,"dmabuf_accepted":2,"dmabuf_rejected":0,"dmabuf_last_import":"64x64:XR24:Linear:planes1","dmabuf_blocker_failures":0,"dmabuf_last_blocker_error":"none","dmabuf_explicit_sync_blockers":1,"dmabuf_implicit_blockers":0,"dmabuf_explicit_sync_release_points":0}'

MASKED_EXPLICIT_BLOCKER_FAILURE_STATUS='{"dmabuf_formats":12,"dmabuf":"v4","dmabuf_disabled":"none","explicit_sync":"v1","explicit_sync_disabled":"none","windows":1,"frames":4,"dmabuf_attempts":2,"dmabuf_accepted":2,"dmabuf_rejected":0,"dmabuf_last_import":"64x64:XR24:Linear:planes1","dmabuf_blocker_failures":0,"dmabuf_last_blocker_error":"none","dmabuf_explicit_sync_blockers":0,"dmabuf_implicit_blockers":1,"dmabuf_explicit_sync_release_points":1}'

DMABUF_TRACE='[1] wl_registry@2.global(3, "zwp_linux_dmabuf_v1", 4)'
SYNCOBJ_TRACE='[1] wl_registry@2.global(3, "zwp_linux_dmabuf_v1", 4)
[1] wl_registry@2.global(4, "wp_linux_drm_syncobj_manager_v1", 1)'

run_validator "$DISABLED_STATUS" "" 0 77 "disabled-path-skips"
run_validator "$DISABLED_STATUS" "$DMABUF_TRACE" 0 1 "disabled-path-rejects-advertised-global"
run_validator "$DISABLED_STATUS" "$SYNCOBJ_TRACE" 0 1 "disabled-path-rejects-syncobj-global"
run_validator "$POSITIVE_V3_STATUS" "$DMABUF_TRACE" 0 0 "positive-v3-passes"
run_validator "$POSITIVE_V3_DEFAULT_FEEDBACK_STATUS" "$DMABUF_TRACE" 0 0 "positive-v3-default-feedback-fallback-passes"
run_validator "$POSITIVE_V3_MISSING_REASON_STATUS" "$DMABUF_TRACE" 0 1 "positive-v3-requires-disabled-reason"
run_validator "$POSITIVE_V3_INCOHERENT_SYNC_REASON_STATUS" "$DMABUF_TRACE" 0 1 "positive-v3-rejects-incoherent-sync-disabled-reason"
run_validator "$POSITIVE_V4_DISABLED_REASON_STATUS" "$DMABUF_TRACE" 0 1 "positive-v4-rejects-disabled-reason"
run_validator "$POSITIVE_V4_SYNC_UNSUPPORTED_STATUS" "$DMABUF_TRACE" 0 0 "positive-v4-sync-unsupported-passes"
run_validator "$POSITIVE_V4_INCOHERENT_SYNC_REASON_STATUS" "$DMABUF_TRACE" 0 1 "positive-v4-rejects-v3-sync-disabled-reason"
run_validator "$POSITIVE_REJECTED_IMPORT_STATUS" "$DMABUF_TRACE" 0 1 "positive-rejects-failed-import"
run_validator "$POSITIVE_NO_BLOCKER_STATUS" "$DMABUF_TRACE" 0 1 "positive-requires-readiness-blocker"
run_validator "$POSITIVE_MISSING_FORMATS_STATUS" "$DMABUF_TRACE" 0 1 "positive-requires-format-count"
run_validator "$POSITIVE_STRING_COUNTER_STATUS" "$DMABUF_TRACE" 0 1 "positive-requires-numeric-counters"
run_validator "$POSITIVE_SYNC_STATUS" "$DMABUF_TRACE" 0 1 "sync-status-requires-syncobj-trace"
run_validator "$SYNC_WITH_V3_STATUS" "$SYNCOBJ_TRACE" 1 1 "sync-requires-dmabuf-v4"
run_validator "$MISSING_RELEASE_STATUS" "$SYNCOBJ_TRACE" 1 1 "strict-sync-requires-release-point"
run_validator "$MASKED_EXPLICIT_BLOCKER_FAILURE_STATUS" "$SYNCOBJ_TRACE" 1 1 "strict-sync-rejects-implicit-fallback"
run_validator "$POSITIVE_SYNC_STATUS" "$SYNCOBJ_TRACE" 1 0 "strict-sync-passes"

echo "OK dmabuf status validator tests passed"
