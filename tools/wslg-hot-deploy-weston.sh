#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WSLG_DIR="$ROOT/reference/wslg"
IMAGE_TAG="${WSLG_IMAGE_TAG:-system-distro-x64:latest}"
DRY_RUN=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --image)
            IMAGE_TAG="$2"
            shift 2
            ;;
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        -h|--help)
            cat <<EOF
Usage: $0 [--image system-distro-x64:latest] [--dry-run]

Copies Weston/WSLg runtime binaries from a built private WSLg Docker image
directly into the active private WSLg system distro. Restart WSL after a real
deploy so WSLg loads the new binaries.
EOF
            exit 0
            ;;
        *)
            echo "Unknown argument: $1" >&2
            exit 2
            ;;
    esac
done

FILES=(
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

"$ROOT/tools/wslg-ensure-build-deps.sh" >/dev/null

if ! command -v wsl.exe >/dev/null 2>&1; then
    echo "ERROR: wsl.exe is required so this script can write to the active WSLg system distro." >&2
    exit 1
fi

wrapper_dir="$(mktemp -d)"
extract_dir="$WSLG_DIR/tmp/hot-deploy-weston"
container_id=""
cleanup() {
    if [[ -n "$container_id" ]]; then
        PATH="$wrapper_dir:$PATH" docker rm "$container_id" >/dev/null 2>&1 || true
    fi
    rm -rf "$extract_dir"
    rm -rf "$wrapper_dir"
}
trap cleanup EXIT

cat >"$wrapper_dir/docker" <<'EOF'
#!/usr/bin/env bash
exec sudo /usr/bin/docker "$@"
EOF
chmod +x "$wrapper_dir/docker"

if ! PATH="$wrapper_dir:$PATH" docker image inspect "$IMAGE_TAG" >/dev/null 2>&1; then
    echo "ERROR: Docker image '$IMAGE_TAG' does not exist. Run tools/wslg-build-private-image.sh first." >&2
    exit 1
fi

echo "Hot deploy source image: $IMAGE_TAG"
printf '  %s\n' "${FILES[@]}"

if [[ "$DRY_RUN" -eq 1 ]]; then
    exit 0
fi

rm -rf "$extract_dir"
mkdir -p "$extract_dir"

container_id="$(PATH="$wrapper_dir:$PATH" docker create "$IMAGE_TAG")"
for file in "${FILES[@]}"; do
    mkdir -p "$extract_dir$(dirname "$file")"
    PATH="$wrapper_dir:$PATH" docker cp "$container_id:$file" "$extract_dir$file"
done

install_script="$extract_dir/install.sh"
{
    printf '%s\n' '#!/bin/sh'
    printf '%s\n' 'set -eu'
    printf 'extract_dir=%s\n' "'$extract_dir'"
    printf '%s\n' 'for dest in "$@"; do'
    printf '%s\n' '    src="$extract_dir$dest"'
    printf '%s\n' '    if [ ! -f "$src" ]; then'
    printf '%s\n' '        echo "missing extracted file: $src" >&2'
    printf '%s\n' '        exit 1'
    printf '%s\n' '    fi'
    printf '%s\n' '    mkdir -p "$(dirname "$dest")"'
    printf '%s\n' '    cp "$src" "$dest.manifold-new"'
    printf '%s\n' '    chown root:root "$dest.manifold-new"'
    printf '%s\n' '    chmod 0755 "$dest.manifold-new"'
    printf '%s\n' '    mv -f "$dest.manifold-new" "$dest"'
    printf '%s\n' '    echo "installed $dest"'
    printf '%s\n' 'done'
    printf '%s\n' 'ldconfig'
    printf '%s\n' 'sync'
} >"$install_script"
chmod +x "$install_script"

wsl.exe --system -u root -- sh "$install_script" "${FILES[@]}"

echo "Hot deploy complete. Restart WSL to load the new WSLg binaries."
