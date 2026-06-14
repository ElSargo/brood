#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WSLG_DIR="$ROOT/reference/wslg"
MARKER_PATCH="$ROOT/tools/patches/wslg-manifold-marker.patch"
WESTON_FASTPATH_PATCH="$ROOT/tools/patches/wslg-require-gfxredir-fastpath.patch"
MARKER_VALUE="manifold-wslg-fastpath-required"

if [[ ! -d "$WSLG_DIR/.git" ]]; then
    "$ROOT/tools/wslg-bootstrap-reference.sh"
fi

if grep -q "MANIFOLD_WSLG_MARKER" "$WSLG_DIR/Dockerfile"; then
    if grep -q "MANIFOLD_WSLG_MARKER=\"$MARKER_VALUE\"" "$WSLG_DIR/Dockerfile"; then
        echo "WSLg Dockerfile already contains the Manifold marker patch."
    else
        sed -i -E "s/MANIFOLD_WSLG_MARKER=\"[^\"]+\"/MANIFOLD_WSLG_MARKER=\"$MARKER_VALUE\"/" "$WSLG_DIR/Dockerfile"
        echo "Updated WSLg Dockerfile Manifold marker to $MARKER_VALUE."
    fi
else
    git -C "$WSLG_DIR" apply "$MARKER_PATCH"
fi

if [[ -d "$WSLG_DIR/vendor/weston/.git" ]]; then
    if grep -q "refusing slow RDPGFX fallback" "$WSLG_DIR/vendor/weston/libweston/backend-rdp/rdprail.c"; then
        echo "WSLg Weston already contains the gfxredir fast-path guard."
    else
        git -C "$WSLG_DIR/vendor/weston" apply "$WESTON_FASTPATH_PATCH"
    fi
else
    echo "WSLg Weston vendor checkout is missing; run tools/wslg-bootstrap-vendors.sh before applying Weston patches." >&2
    exit 1
fi

git -C "$WSLG_DIR" diff -- Dockerfile
git -C "$WSLG_DIR/vendor/weston" diff -- libweston/backend-rdp/rdprail.c
