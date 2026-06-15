<#
.SYNOPSIS
Rebuild, install, and smoke-test the private WSLg path with two Chrome windows.

.DESCRIPTION
Builds the private WSLg system distro VHD from the local reference checkout,
installs that VHD through .wslconfig, restarts WSL by default, opens two
isolated Chrome/Chromium browser windows in the target distro, and verifies the
private WSLg marker.

The default path is intentionally the full private-VHD redeploy because hot
copying Weston binaries into the running system distro does not survive
wsl --shutdown.

.EXAMPLE
.\tools\wslg-redeploy-two-chrome.ps1

.EXAMPLE
.\tools\wslg-redeploy-two-chrome.ps1 -SkipBuild -SkipInstall
#>
param(
    [string]$Distro = "Ubuntu-24.04",
    [string]$InstallDir = "$env:USERPROFILE\manifold-wslg",
    [string]$LeftUrl = "data:text/html,%3Ctitle%3EManifold%20WSLg%20left%3C/title%3E%3Cbody%20style%3D%22margin%3A0%3Bheight%3A100vh%3Bdisplay%3Agrid%3Bplace-items%3Acenter%3Bbackground%3A%23d71920%3Bcolor%3Awhite%3Bfont%3A700%2048px%20system-ui%2Csans-serif%3B%22%3ELEFT%20CHROME%3C/body%3E",
    [string]$RightUrl = "data:text/html,%3Ctitle%3EManifold%20WSLg%20right%3C/title%3E%3Cbody%20style%3D%22margin%3A0%3Bheight%3A100vh%3Bdisplay%3Agrid%3Bplace-items%3Acenter%3Bbackground%3A%2300a651%3Bcolor%3Awhite%3Bfont%3A700%2048px%20system-ui%2Csans-serif%3B%22%3ERIGHT%20CHROME%3C/body%3E",
    [switch]$SkipBuild,
    [switch]$SkipInstall,
    [switch]$SkipVerify,
    [switch]$SkipChrome,
    [switch]$NoShutdown,
    [int]$WindowWidth = 1600,
    [int]$WindowHeight = 1000
)

$ErrorActionPreference = "Stop"
$root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$vhdPath = Join-Path $root "reference\wslg\system_x64.vhd"

function Invoke-NativeStep {
    param(
        [string]$Name,
        [scriptblock]$Command
    )

    Write-Output ""
    Write-Output "=== $Name ==="
    & $Command
    if ($LASTEXITCODE -ne 0) {
        throw "$Name failed with exit code $LASTEXITCODE"
    }
}

function Invoke-PowerShellStep {
    param(
        [string]$Name,
        [scriptblock]$Command
    )

    Write-Output ""
    Write-Output "=== $Name ==="
    & $Command
}

Push-Location $root
try {
    if (-not $SkipBuild) {
        Invoke-NativeStep "Build private WSLg system distro VHD" {
            bash tools/wslg-build-private-system-distro.sh
        }
    } elseif (-not $SkipInstall -and -not (Test-Path -LiteralPath $vhdPath)) {
        throw "SkipBuild was requested, but no VHD exists at $vhdPath"
    }

    if (-not $SkipInstall) {
        if (-not (Test-Path -LiteralPath $vhdPath)) {
            throw "VHD not found at $vhdPath"
        }

        Invoke-PowerShellStep "Install private WSLg system distro" {
            $installArgs = @{
                Distro = $Distro
                WslVhdPath = $vhdPath
                InstallDir = $InstallDir
            }

            if (-not $NoShutdown) {
                $installArgs.Shutdown = $true
            }

            & (Join-Path $PSScriptRoot "wslg-install-private-system-distro.ps1") @installArgs
        }
    }

    if ($NoShutdown -and -not $SkipInstall) {
        Write-Warning "Installed the private VHD without restarting WSL. Existing WSLg processes may still be running old binaries."
    }

    if (-not $SkipChrome) {
        $launchScript = @'
set -euo pipefail

left_url="$1"
right_url="$2"
window_width="$3"
window_height="$4"

chrome=""
for candidate in google-chrome-stable google-chrome chromium chromium-browser; do
    if command -v "$candidate" >/dev/null 2>&1; then
        chrome="$candidate"
        break
    fi
done

if [[ -z "$chrome" ]]; then
    echo "ERROR: no Chrome/Chromium executable found in this distro." >&2
    echo "Install google-chrome-stable or chromium, then rerun this script with -SkipBuild -SkipInstall." >&2
    exit 1
fi

stamp="$(date +%Y%m%d-%H%M%S)"
left_profile="/tmp/manifold-wslg-chrome-left-$stamp"
right_profile="/tmp/manifold-wslg-chrome-right-$stamp"
mkdir -p "$left_profile" "$right_profile"

launch_window() {
    local profile="$1"
    local title="$2"
    local url="$3"
    local log="/tmp/manifold-wslg-$title-$stamp.log"
    local args=(
        "--user-data-dir=$profile"
        "--no-first-run"
        "--no-default-browser-check"
        "--disable-session-crashed-bubble"
        "--enable-features=UseOzonePlatform"
        "--ozone-platform=wayland"
        "--class=manifold-wslg-$title"
        "--window-size=${window_width},${window_height}"
        "--new-window"
        "$url"
    )

    if command -v setsid >/dev/null 2>&1; then
        setsid -f "$chrome" "${args[@]}" >"$log" 2>&1
    else
        nohup "$chrome" "${args[@]}" >"$log" 2>&1 &
    fi

    echo "launched $title chrome: $chrome ${args[*]}"
    echo "log: $log"
}

launch_window "$left_profile" left "$left_url"
sleep 1
launch_window "$right_profile" right "$right_url"
'@

        Invoke-NativeStep "Open two WSLg Chrome test windows" {
            $launchScript | wsl.exe -d $Distro -- bash -s -- $LeftUrl $RightUrl $WindowWidth $WindowHeight
        }
    }

    if (-not $SkipVerify) {
        Start-Sleep -Seconds 3
        Invoke-NativeStep "Verify active private WSLg system distro" {
            & (Join-Path $PSScriptRoot "wslg-verify-private-system-distro.ps1") -Distro $Distro
        }
    }
} finally {
    Pop-Location
}
