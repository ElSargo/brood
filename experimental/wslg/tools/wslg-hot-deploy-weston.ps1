param(
    [string]$Image = "system-distro-x64:latest",
    [string]$WestonImage = "system-distro-x64-weston-build:latest",
    [string]$WestonArtifactDir = "reference/wslg/tmp/weston-artifacts",
    [switch]$WestonOnly,
    [switch]$SkipBuild,
    [switch]$NoShutdown,
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"
$root = Resolve-Path (Join-Path $PSScriptRoot "..\..\..")

Push-Location $root
try {
    $deployImage = $Image
    $sourceRoot = "/"
    $sourceDir = $null

    if (-not $SkipBuild) {
        if ($WestonOnly) {
            Write-Output "Building Weston hot-deploy artifacts..."
            $env:WSLG_WESTON_IMAGE_TAG = $WestonImage
            $env:WSLG_WESTON_ARTIFACT_DIR = $WestonArtifactDir
            bash experimental/wslg/tools/wslg-build-weston-artifacts.sh
            $sourceDir = $WestonArtifactDir
        } else {
            Write-Output "Building private WSLg Docker image..."
            $env:WSLG_IMAGE_TAG = $Image
            bash experimental/wslg/tools/wslg-build-private-image.sh
        }

        if ($LASTEXITCODE -ne 0) {
            throw "Build failed with exit code $LASTEXITCODE"
        }
    } elseif ($WestonOnly) {
        $sourceDir = $WestonArtifactDir
    }

    $bashArgs = @("experimental/wslg/tools/wslg-hot-deploy-weston.sh", "--image", $deployImage, "--source-root", $sourceRoot)
    if ($sourceDir) {
        $bashArgs += @("--source-dir", $sourceDir)
    }
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
