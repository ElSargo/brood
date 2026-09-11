#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
export BROOD_SESSION_SWITCHER_PACKAGE="${BROOD_SESSION_SWITCHER_PACKAGE:-$(nix build .#brood --no-link --print-out-paths)}"
export BROOD_SESSION_SWITCHER_OUT="${BROOD_SESSION_SWITCHER_OUT:-$(mktemp -d /tmp/brood-workspace-layouts.XXXXXX)}"
export RUST_LOG=broodd=info,brood_client=info,smithay=warn,wgpu_core=warn,wgpu_hal=warn
export BROOD_PLUGIN_DRAG_TEST=1 BROOD_LAYOUT_TEST=1
export BROOD_TRACE_RESIZE=1 BROOD_TRACE_RESIZE_PIPELINE=1
bash "$ROOT/tools/test-session-switcher-plugin-nested.sh"
echo "workspace_layout_artifacts=$BROOD_SESSION_SWITCHER_OUT"
