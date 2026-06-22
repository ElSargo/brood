#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
WSLG_DIR="$ROOT/reference/wslg"
VENDOR_DIR="$WSLG_DIR/vendor"

if [[ ! -d "$WSLG_DIR/.git" ]]; then
    "$ROOT/experimental/wslg/tools/wslg-bootstrap-reference.sh"
fi

mkdir -p "$VENDOR_DIR"

clone_or_fetch() {
    local url="$1"
    local dir="$2"
    local ref="$3"

    if [[ -d "$dir/.git" ]]; then
        if git -C "$dir" rev-parse --is-shallow-repository | grep -qx true; then
            git -C "$dir" fetch --unshallow --tags
        else
            git -C "$dir" fetch --all --prune --tags
        fi
        git -C "$dir" switch "$ref" 2>/dev/null || git -C "$dir" checkout "$ref"
    else
        git clone --branch "$ref" "$url" "$dir"
    fi

    git -C "$dir" status --short --branch
}

clone_or_fetch https://github.com/microsoft/FreeRDP-mirror "$VENDOR_DIR/FreeRDP" working
clone_or_fetch https://github.com/microsoft/weston-mirror "$VENDOR_DIR/weston" working
clone_or_fetch https://github.com/microsoft/PulseAudio-mirror "$VENDOR_DIR/pulseaudio" working
clone_or_fetch https://github.com/microsoft/DirectX-Headers.git "$VENDOR_DIR/DirectX-Headers-1.0" v1.608.0
clone_or_fetch https://gitlab.freedesktop.org/mesa/mesa.git "$VENDOR_DIR/mesa" mesa-23.1.0

echo "WSLg vendors are ready in $VENDOR_DIR"
