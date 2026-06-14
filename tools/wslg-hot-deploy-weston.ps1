param(
    [string]$Image = "system-distro-x64:latest",
    [switch]$SkipBuild,
    [switch]$NoShutdown,
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"
$root = Resolve-Path (Join-Path $PSScriptRoot "..")

Push-Location $root
try {
    if (-not $SkipBuild) {
        Write-Output "Building private WSLg Docker image..."
        $env:WSLG_IMAGE_TAG = $Image
        bash tools/wslg-build-private-image.sh
        if ($LASTEXITCODE -ne 0) {
            throw "Image build failed with exit code $LASTEXITCODE"
        }
    }

    $bashArgs = @("tools/wslg-hot-deploy-weston.sh", "--image", $Image)
    if ($DryRun) {
        $bashArgs += "--dry-run"
    }

    Write-Output "Copying Weston/WSLg binaries into the active private system distro..."
    bash @bashArgs
    if ($LASTEXITCODE -ne 0) {
        throw "Hot deploy failed with exit code $LASTEXITCODE"
    }

    if (-not $DryRun -and -not $NoShutdown) {
        Write-Output "Shutting down WSL so WSLg reloads the deployed binaries..."
        wsl --shutdown
    } elseif (-not $DryRun) {
        Write-Output "Hot deploy copied files. Run 'wsl --shutdown' before verifying the new binaries."
    }
} finally {
    Pop-Location
}
