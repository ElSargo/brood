#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
# Invoke through nix develop. Both lanes use private offscreen compositors.
# Preserve a failing production lane and still exercise resize-proof.
export BROOD_SESSION_SWITCHER_PACKAGE="${BROOD_SESSION_SWITCHER_PACKAGE:-$(nix build .#brood --no-link --print-out-paths)}"
OUT="${BROOD_PLUGIN_DRAG_OUT:-$(mktemp -d /tmp/brood-plugin-drags.XXXXXX)}"
export BROOD_PLUGIN_DRAG_TEST=1
export BROOD_TRACE_RESIZE=1
export BROOD_TRACE_RESIZE_PIPELINE=1
result=0
BROOD_SESSION_SWITCHER_OUT="$OUT/plugins" \
  bash "$ROOT/tools/test-session-switcher-plugin-nested.sh" || result=1
BROOD_PLUGIN_DRAG_PROOF_ONLY=1 BROOD_SESSION_SWITCHER_OUT="$OUT/resize-proof" \
  bash "$ROOT/tools/test-session-switcher-plugin-nested.sh" || result=1
python3 "$ROOT/tools/render-plugin-drag-report.py" "$OUT" || result=1
echo "plugin_drag_artifacts=$OUT"
exit "$result"
