#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
PACKAGE="${BROOD_CHROMIUM_PACKAGE:-}"
if [[ -z "$PACKAGE" ]]; then
  PACKAGE="$(nix build path:.#brood --no-link --print-out-paths)"
fi
OUT="${BROOD_CHROMIUM_OUT:-$(mktemp -d /tmp/brood-chromium.XXXXXX)}"
for size in ${BROOD_CHROMIUM_SIZE:-1920x1080 1280x720}; do
  BROOD_CHROMIUM_SIZE="$size" python3 "$ROOT/tools/test-chromium-nested.py" \
    --package "$PACKAGE" --out-dir "$OUT/$size"
done
