#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TMPDIR="$(mktemp -d /tmp/manifold-dmabuf-classifier.XXXXXX)"
trap 'rm -rf "$TMPDIR"' EXIT

run_classifier() {
  local name="$1"
  local expected_rc="$2"
  local expected_classification="$3"
  shift 3
  local output="$TMPDIR/$name.out"

  set +e
  python3 "$ROOT/tools/classify-dmabuf-backend.py" "$@" > "$output" 2>&1
  local rc=$?
  set -e

  if [[ "$rc" != "$expected_rc" ]]; then
    echo "ERR $name expected rc=$expected_rc got rc=$rc" >&2
    cat "$output" >&2
    exit 1
  fi
  if [[ "$expected_rc" == "0" ]]; then
    grep -qx "classification=$expected_classification" "$output" || {
      echo "ERR $name expected classification=$expected_classification" >&2
      cat "$output" >&2
      exit 1
    }
    grep -q '^summary=' "$output" || {
      echo "ERR $name missing summary" >&2
      cat "$output" >&2
      exit 1
    }
    grep -q '^next=' "$output" || {
      echo "ERR $name missing next step" >&2
      cat "$output" >&2
      exit 1
    }
  fi
}

run_classifier fallback-no-import 0 fallback_no_dmabuf_import \
  --dmabuf none \
  --dmabuf-formats 0 \
  --explicit-sync none \
  --explicit-sync-disabled no_dmabuf_texture_formats

run_classifier drm-capable 0 drm_capable_dmabuf_and_explicit_sync \
  --dmabuf v4 \
  --dmabuf-formats 32 \
  --explicit-sync v1 \
  --explicit-sync-disabled none

run_classifier v4-no-sync 0 dmabuf_v4_without_explicit_sync \
  --dmabuf v4 \
  --dmabuf-formats 32 \
  --explicit-sync none \
  --explicit-sync-disabled drm_syncobj_eventfd_unsupported

run_classifier v4-no-sync-bad-reason 0 incoherent_dmabuf_status \
  --dmabuf v4 \
  --dmabuf-formats 32 \
  --explicit-sync none \
  --explicit-sync-disabled v3_no_render_node

run_classifier v3-no-feedback 0 dmabuf_import_without_v4_feedback \
  --dmabuf v3 \
  --dmabuf-formats 12 \
  --explicit-sync none \
  --explicit-sync-disabled v3_no_render_node

run_classifier positive-formats-without-dmabuf 0 incoherent_dmabuf_status \
  --dmabuf none \
  --dmabuf-formats 12 \
  --explicit-sync none \
  --explicit-sync-disabled none

run_classifier negative-formats 2 unused \
  --dmabuf none \
  --dmabuf-formats -1 \
  --explicit-sync none \
  --explicit-sync-disabled none

echo "OK dmabuf backend classifier tests passed"
