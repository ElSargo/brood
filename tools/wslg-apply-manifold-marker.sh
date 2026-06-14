#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WSLG_DIR="$ROOT/reference/wslg"
MARKER_PATCH="$ROOT/tools/patches/wslg-manifold-marker.patch"
WSLG_LOCAL_EXPORT_PATCH="$ROOT/tools/patches/wslg-speed-up-local-export.patch"
WESTON_FASTPATH_PATCH="$ROOT/tools/patches/wslg-require-gfxredir-fastpath.patch"
WESTON_SHM_RETRY_PATCH="$ROOT/tools/patches/wslg-retry-gfxredir-shm-open.patch"
WESTON_PIXMAN_PATCH="$ROOT/tools/patches/wslg-invert-pixman-control.patch"
WESTON_GL_PATCH="$ROOT/tools/patches/wslg-invert-shader-poc.patch"
WESTON_COMPOSITE_PATCH="$ROOT/tools/patches/wslg-composite-two-clients-poc.patch"
MARKER_VALUE="manifold-wslg-fastpath-required"

apply_patch_if_missing() {
    local repo_dir="$1"
    local patch_file="$2"
    local target_file="$3"
    local sentinel="$4"
    local label="$5"
    shift 5

    if grep -Fq "$sentinel" "$target_file"; then
        echo "$label already applied."
    else
        git -C "$repo_dir" apply "$@" "$patch_file"
        echo "Applied $label."
    fi
}

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

apply_patch_if_missing \
    "$WSLG_DIR" \
    "$WSLG_LOCAL_EXPORT_PATCH" \
    "$WSLG_DIR/build-and-export.sh" \
    'tmp/system_x64.tar' \
    "WSLg local export speedup" \
    "--unidiff-zero"

if [[ -d "$WSLG_DIR/vendor/weston/.git" ]]; then
    apply_patch_if_missing \
        "$WSLG_DIR/vendor/weston" \
        "$WESTON_FASTPATH_PATCH" \
        "$WSLG_DIR/vendor/weston/libweston/backend-rdp/rdprail.c" \
        "refusing slow RDPGFX fallback" \
        "WSLg Weston gfxredir fast-path guard" \
        "--unidiff-zero"

    apply_patch_if_missing \
        "$WSLG_DIR/vendor/weston" \
        "$WESTON_SHM_RETRY_PATCH" \
        "$WSLG_DIR/vendor/weston/libweston/backend-rdp/rdputil.c" \
        "RDP_SHARED_MEMORY_OPEN_RETRY_COUNT" \
        "WSLg Weston gfxredir shared-memory open retry" \
        "--unidiff-zero"

    apply_patch_if_missing \
        "$WSLG_DIR" \
        "$WESTON_PIXMAN_PATCH" \
        "$WSLG_DIR/vendor/weston/libweston/pixman-renderer.c" \
        "pixel[0] = 0xff - pixel[0]" \
        "WSLg Weston pixman compositor inversion"

    apply_patch_if_missing \
        "$WSLG_DIR" \
        "$WESTON_GL_PATCH" \
        "$WSLG_DIR/vendor/weston/libweston/renderer-gl/gl-renderer.c" \
        "alpha * (1.0 - color.rgb)" \
        "WSLg Weston GL compositor inversion"

    apply_patch_if_missing \
        "$WSLG_DIR" \
        "$WESTON_COMPOSITE_PATCH" \
        "$WSLG_DIR/vendor/weston/libweston/backend-rdp/rdprail.c" \
        "MANIFOLD_COMPOSITE_WIDTH" \
        "WSLg Weston two-client composite PoC"
else
    echo "WSLg Weston vendor checkout is missing; run tools/wslg-bootstrap-vendors.sh before applying Weston patches." >&2
    exit 1
fi

git -C "$WSLG_DIR" diff -- Dockerfile
git -C "$WSLG_DIR" diff -- .dockerignore build-and-export.sh
git -C "$WSLG_DIR/vendor/weston" diff -- \
    include/libweston/backend-rdp.h \
    libweston/backend-rdp/rdprail.c \
    libweston/backend-rdp/rdputil.c \
    libweston/backend-rdp/rdp.h \
    libweston/pixman-renderer.c \
    libweston/renderer-gl/gl-renderer.c
