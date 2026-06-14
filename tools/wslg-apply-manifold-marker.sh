#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WSLG_DIR="$ROOT/reference/wslg"
PATCH="$ROOT/tools/patches/wslg-manifold-marker.patch"

if [[ ! -d "$WSLG_DIR/.git" ]]; then
    "$ROOT/tools/wslg-bootstrap-reference.sh"
fi

if grep -q "MANIFOLD_WSLG_MARKER" "$WSLG_DIR/Dockerfile"; then
    echo "WSLg Dockerfile already contains the Manifold marker patch."
else
    git -C "$WSLG_DIR" apply "$PATCH"
fi

git -C "$WSLG_DIR" diff -- Dockerfile
