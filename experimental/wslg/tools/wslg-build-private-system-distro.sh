#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
WSLG_DIR="$ROOT/reference/wslg"

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
PATH="$wrapper_dir:$PATH" ./build-and-export.sh

echo "Built private WSLg VHD:"
ls -lh "$WSLG_DIR/system_x64.vhd"
