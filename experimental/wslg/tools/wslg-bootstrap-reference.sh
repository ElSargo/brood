#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
WSLG_DIR="$ROOT/reference/wslg"

if [[ -d "$WSLG_DIR/.git" ]]; then
    git -C "$WSLG_DIR" fetch --all --prune
else
    mkdir -p "$ROOT/reference"
    git clone https://github.com/microsoft/wslg "$WSLG_DIR"
fi

git -C "$WSLG_DIR" switch manifold-wslg-impl 2>/dev/null \
    || git -C "$WSLG_DIR" switch -c manifold-wslg-impl
git -C "$WSLG_DIR" status --short --branch

cat <<'MSG'

WSLg reference checkout is ready.

Next upstream vendor clones, matching WSLg's CONTRIBUTING.md:

  git clone https://github.com/microsoft/FreeRDP-mirror reference/wslg/vendor/FreeRDP -b working
  git clone https://github.com/microsoft/weston-mirror reference/wslg/vendor/weston -b working
  git clone https://github.com/microsoft/PulseAudio-mirror reference/wslg/vendor/pulseaudio -b working
  git clone https://github.com/microsoft/DirectX-Headers.git reference/wslg/vendor/DirectX-Headers-1.0 -b v1.608.0
  git clone https://gitlab.freedesktop.org/mesa/mesa.git reference/wslg/vendor/mesa -b mesa-23.1.0

Run those when you are ready to build a private WSLg system distro.
MSG
