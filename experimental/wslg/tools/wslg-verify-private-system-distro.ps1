param(
    [string]$Distro = "Ubuntu-24.04"
)

$ErrorActionPreference = "Stop"

Write-Output "Checking WSLg system distro marker..."
wsl --system -d $Distro -- sh -lc "cat /etc/manifold-wslg.txt 2>/dev/null || true; echo '--- versions ---'; grep -E '^(WSLg|wslg:|weston:|manifold-wslg:)' /etc/versions.txt 2>/dev/null || true; echo '--- processes ---'; ps -ax | grep -E 'WSLGd|weston' | grep -v grep || true"
