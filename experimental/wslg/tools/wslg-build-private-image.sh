#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
WSLG_DIR="$ROOT/reference/wslg"
IMAGE_TAG="${WSLG_IMAGE_TAG:-system-distro-x64:latest}"

"$ROOT/experimental/wslg/tools/wslg-bootstrap-reference.sh"

if [[ ! -d "$WSLG_DIR/vendor/weston/.git" ]]; then
    echo "WSLg vendor sources are missing; bootstrapping them now."
    "$ROOT/experimental/wslg/tools/wslg-bootstrap-vendors.sh"
fi

"$ROOT/experimental/wslg/tools/wslg-apply-manifold-marker.sh"
"$ROOT/experimental/wslg/tools/wslg-ensure-build-deps.sh"

if [[ ! -f "$WSLG_DIR/vendor/pulseaudio/.tarball-version" ]]; then
    printf '17.0\n' > "$WSLG_DIR/vendor/pulseaudio/.tarball-version"
fi

wrapper_dir="$(mktemp -d)"
cleanup() {
    rm -rf "$wrapper_dir"
}
trap cleanup EXIT

cat >"$wrapper_dir/docker" <<'EOF'
#!/usr/bin/env bash
exec sudo /usr/bin/docker "$@"
EOF
chmod +x "$wrapper_dir/docker"

cd "$WSLG_DIR"

WSLG_VERSION=$(./devops/get-nuget-version.sh "-Beta" || true)
WSLG_VERSION=${WSLG_VERSION:-dev}
WSLG_COMMIT=$(git rev-parse HEAD 2>/dev/null || echo "dev")

vendor_commit() { git -C "$1" rev-parse HEAD 2>/dev/null || echo "dev"; }
DIRECTX_HEADERS_VERSION=$(vendor_commit vendor/DirectX-Headers-1.0)
FREERDP_COMMIT=$(vendor_commit vendor/FreeRDP)
MESA_VERSION=$(vendor_commit vendor/mesa)
PULSEAUDIO_COMMIT=$(vendor_commit vendor/pulseaudio)
WESTON_COMMIT=$(vendor_commit vendor/weston)

echo "=== Building Docker image only (tag=$IMAGE_TAG WSLG_VERSION=$WSLG_VERSION WSLG_COMMIT=$WSLG_COMMIT) ==="
PATH="$wrapper_dir:$PATH" docker build -f Dockerfile -t "$IMAGE_TAG" . \
    --build-arg WSLG_VERSION="$WSLG_VERSION" \
    --build-arg WSLG_COMMIT="$WSLG_COMMIT" \
    --build-arg WSLG_ARCH=x86_64 \
    --build-arg DIRECTX_HEADERS_VERSION="$DIRECTX_HEADERS_VERSION" \
    --build-arg FREERDP_COMMIT="$FREERDP_COMMIT" \
    --build-arg MESA_VERSION="$MESA_VERSION" \
    --build-arg PULSEAUDIO_COMMIT="$PULSEAUDIO_COMMIT" \
    --build-arg WESTON_COMMIT="$WESTON_COMMIT"

echo "Built private WSLg image: $IMAGE_TAG"
