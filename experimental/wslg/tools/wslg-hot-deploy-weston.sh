#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
WSLG_DIR="$ROOT/reference/wslg"
IMAGE_TAG="${WSLG_IMAGE_TAG:-system-distro-x64:latest}"
SOURCE_ROOT="/"
SOURCE_DIR=""
DRY_RUN=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --image)
            IMAGE_TAG="$2"
            shift 2
            ;;
        --source-root)
            SOURCE_ROOT="$2"
            shift 2
            ;;
        --source-dir)
            SOURCE_DIR="$2"
            shift 2
            ;;
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        -h|--help)
            cat <<EOF
Usage: $0 [--image system-distro-x64:latest] [--source-root /] [--source-dir DIR] [--dry-run]

Copies Weston/WSLg runtime binaries from a built private WSLg Docker image
directly into the active private WSLg system distro. Restart WSL after a real
deploy so WSLg loads the new binaries.

Use --source-root /work/build when deploying from the Weston build-stage image
created by experimental/wslg/tools/wslg-build-weston-artifacts.sh.

Use --source-dir reference/wslg/tmp/weston-artifacts to deploy local artifacts
created by experimental/wslg/tools/wslg-build-weston-artifacts.sh without extracting from Docker.
EOF
            exit 0
            ;;
        *)
            echo "Unknown argument: $1" >&2
            exit 2
            ;;
    esac
done

if [[ -n "$SOURCE_DIR" ]]; then
    SOURCE_DIR="$(cd "$SOURCE_DIR" && pwd)"
fi

SOURCE_ROOT="${SOURCE_ROOT%/}"
if [[ -z "$SOURCE_ROOT" ]]; then
    SOURCE_ROOT="/"
fi

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

"$ROOT/experimental/wslg/tools/wslg-ensure-build-deps.sh" >/dev/null

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

if [[ -z "$SOURCE_DIR" ]] && ! PATH="$wrapper_dir:$PATH" docker image inspect "$IMAGE_TAG" >/dev/null 2>&1; then
    echo "ERROR: Docker image '$IMAGE_TAG' does not exist. Run experimental/wslg/tools/wslg-build-private-image.sh first." >&2
    exit 1
fi

if [[ -n "$SOURCE_DIR" ]]; then
    echo "Hot deploy source dir: $SOURCE_DIR"
else
    echo "Hot deploy source image: $IMAGE_TAG"
    echo "Hot deploy source root: $SOURCE_ROOT"
fi
printf '  %s\n' "${FILES[@]}"

if [[ "$DRY_RUN" -eq 1 ]]; then
    exit 0
fi

rm -rf "$extract_dir"
mkdir -p "$extract_dir"

if [[ -z "$SOURCE_DIR" ]]; then
    container_id="$(PATH="$wrapper_dir:$PATH" docker create "$IMAGE_TAG")"
fi
for file in "${FILES[@]}"; do
    mkdir -p "$extract_dir$(dirname "$file")"
    if [[ -n "$SOURCE_DIR" ]]; then
        source_file="$SOURCE_DIR$file"
        if [[ ! -f "$source_file" ]]; then
            echo "ERROR: local artifact '$source_file' does not exist. Run experimental/wslg/tools/wslg-build-weston-artifacts.sh first." >&2
            exit 1
        fi
        cp -a "$source_file" "$extract_dir$file"
    else
        source_file="$SOURCE_ROOT$file"
        PATH="$wrapper_dir:$PATH" docker cp "$container_id:$source_file" "$extract_dir$file"
    fi
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
