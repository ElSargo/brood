#!/usr/bin/env bash
set -euo pipefail

missing=()
command -v dockerd >/dev/null 2>&1 || missing+=(docker.io)
command -v go >/dev/null 2>&1 || missing+=(golang-go)

if ((${#missing[@]})); then
    sudo apt-get update
    sudo DEBIAN_FRONTEND=noninteractive apt-get install -y "${missing[@]}"
fi

if ! sudo docker info >/dev/null 2>&1; then
    sudo nohup dockerd --host=unix:///var/run/docker.sock \
        >/tmp/manifold-wslg-dockerd.log 2>&1 &
fi

for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
    if sudo docker info >/dev/null 2>&1; then
        break
    fi
    sleep 1
done

if ! sudo docker info >/dev/null 2>&1; then
    echo "ERROR: Docker daemon did not become ready." >&2
    tail -80 /tmp/manifold-wslg-dockerd.log >&2 || true
    exit 1
fi

echo "docker=$(sudo docker version --format '{{.Server.Version}}')"
echo "go=$(go version)"
