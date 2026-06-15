#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WSLG_DIR="$ROOT/reference/wslg"
BASE_IMAGE_TAG="${WSLG_WESTON_IMAGE_TAG:-system-distro-x64-weston-build:latest}"
ARTIFACT_DIR="${WSLG_WESTON_ARTIFACT_DIR:-$WSLG_DIR/tmp/weston-artifacts}"
REFRESH_BASE=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --image)
            BASE_IMAGE_TAG="$2"
            shift 2
            ;;
        --artifact-dir)
            ARTIFACT_DIR="$2"
            shift 2
            ;;
        --refresh-base)
            REFRESH_BASE=1
            shift
            ;;
        -h|--help)
            cat <<EOF
Usage: $0 [--image system-distro-x64-weston-build:latest] [--artifact-dir DIR] [--refresh-base]

Builds Weston incrementally inside a reusable WSLg dev-stage container and
copies the hot-deploy runtime files to DIR. The base image is built from the
WSLg Dockerfile's dev target only when missing, or when --refresh-base is used.
EOF
            exit 0
            ;;
        *)
            echo "Unknown argument: $1" >&2
            exit 2
            ;;
    esac
done

"$ROOT/tools/wslg-bootstrap-reference.sh"

if [[ ! -d "$WSLG_DIR/vendor/weston/.git" ]]; then
    echo "WSLg vendor sources are missing; bootstrapping them now."
    "$ROOT/tools/wslg-bootstrap-vendors.sh"
fi

WSLG_APPLY_SHOW_DIFF=0 \
WSLG_APPLY_PIXMAN_INVERT="${WSLG_APPLY_PIXMAN_INVERT:-0}" \
    "$ROOT/tools/wslg-apply-manifold-marker.sh"
"$ROOT/tools/wslg-ensure-build-deps.sh"

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

vendor_commit() { git -C "$1" rev-parse HEAD 2>/dev/null || echo "dev"; }

build_base_image() {
    local wslg_version
    local wslg_commit
    local directx_headers_version
    local freerdp_commit
    local mesa_version
    local pulseaudio_commit
    local weston_commit

    wslg_version=$(./devops/get-nuget-version.sh "-Beta" || true)
    wslg_version=${wslg_version:-dev}
    wslg_commit=$(git rev-parse HEAD 2>/dev/null || echo "dev")
    directx_headers_version=$(vendor_commit vendor/DirectX-Headers-1.0)
    freerdp_commit=$(vendor_commit vendor/FreeRDP)
    mesa_version=$(vendor_commit vendor/mesa)
    pulseaudio_commit=$(vendor_commit vendor/pulseaudio)
    weston_commit=$(vendor_commit vendor/weston)

    echo "=== Building reusable Weston base image (tag=$BASE_IMAGE_TAG WSLG_VERSION=$wslg_version WSLG_COMMIT=$wslg_commit) ==="
    PATH="$wrapper_dir:$PATH" docker build -f Dockerfile --target dev -t "$BASE_IMAGE_TAG" . \
        --build-arg WSLG_VERSION="$wslg_version" \
        --build-arg WSLG_COMMIT="$wslg_commit" \
        --build-arg WSLG_ARCH=x86_64 \
        --build-arg DIRECTX_HEADERS_VERSION="$directx_headers_version" \
        --build-arg FREERDP_COMMIT="$freerdp_commit" \
        --build-arg MESA_VERSION="$mesa_version" \
        --build-arg PULSEAUDIO_COMMIT="$pulseaudio_commit" \
        --build-arg WESTON_COMMIT="$weston_commit"
}

if [[ "$REFRESH_BASE" -eq 1 ]] || ! PATH="$wrapper_dir:$PATH" docker image inspect "$BASE_IMAGE_TAG" >/dev/null 2>&1; then
    build_base_image
else
    echo "Using reusable Weston base image: $BASE_IMAGE_TAG"
fi

mkdir -p "$ARTIFACT_DIR"
ARTIFACT_DIR="$(cd "$ARTIFACT_DIR" && pwd)"

echo "=== Building Weston incrementally from $WSLG_DIR/vendor/weston ==="
PATH="$wrapper_dir:$PATH" docker run --rm \
    -v "$WSLG_DIR/vendor/weston:/work/vendor/weston" \
    -v "$ARTIFACT_DIR:/work/weston-artifacts" \
    "$BASE_IMAGE_TAG" \
    /bin/bash -lc '
set -euo pipefail

git config --global --add safe.directory /work/vendor/weston || true
cd /work/vendor/weston

meson_args=(
    --prefix="${PREFIX}"
    --buildtype="${BUILDTYPE}"
    -Dbackend-default=rdp
    -Dbackend-drm=false
    -Dbackend-drm-screencast-vaapi=false
    -Dbackend-headless=false
    -Dbackend-wayland=false
    -Dbackend-x11=false
    -Dbackend-fbdev=false
    -Dcolor-management-colord=false
    -Dscreenshare=false
    -Dsystemd=false
    -Dwslgd=true
    -Dremoting=false
    -Dpipewire=false
    -Dshell-fullscreen=false
    -Dcolor-management-lcms=false
    -Dshell-ivi=false
    -Dshell-kiosk=false
    -Ddemo-clients=false
    -Dsimple-clients=[]
    -Dtools=[]
    -Dresize-pool=false
    -Dwcap-decode=false
    -Dtest-junit-xml=false
)

if [[ -f build/build.ninja ]]; then
    /usr/bin/meson setup --reconfigure "${meson_args[@]}" build
else
    rm -rf build
    /usr/bin/meson setup "${meson_args[@]}" build
fi

ninja -C build -j"$(nproc)" install

files=(
    /usr/bin/WSLGd
    /usr/bin/weston
    /usr/lib/libweston-9.so.0.0.0
    /usr/lib/libweston-desktop-9.so.0.0.0
    /usr/lib/libweston-9/rdp-backend.so
    /usr/lib/libweston-9/gl-renderer.so
    /usr/lib/libweston-9/xwayland.so
    /usr/lib/weston/libexec_weston.so.0.0.0
    /usr/lib/weston/rdprail-shell.so
    /usr/lib/weston/wslgd-notify.so
    /usr/libexec/weston-rdprail-shell
)

rm -rf /work/weston-artifacts/usr
for file in "${files[@]}"; do
    src="/work/build$file"
    dest="/work/weston-artifacts$file"
    if [[ ! -f "$src" ]]; then
        echo "missing built artifact: $src" >&2
        exit 1
    fi
    mkdir -p "$(dirname "$dest")"
    cp -a "$src" "$dest"
done
'

echo "Built Weston hot-deploy artifacts: $ARTIFACT_DIR"
