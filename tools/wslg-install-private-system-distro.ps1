param(
    [string]$Distro = "Ubuntu-24.04",
    [string]$WslVhdPath = "/home/sargo/code/manifold/reference/wslg/system_x64.vhd",
    [string]$InstallDir = "$env:USERPROFILE\manifold-wslg",
    [switch]$Shutdown
)

$ErrorActionPreference = "Stop"

$exists = wsl -d $Distro -- test -f $WslVhdPath
if ($LASTEXITCODE -ne 0) {
    throw "VHD not found in WSL: $WslVhdPath. Run tools/wslg-build-private-system-distro.sh first."
}

$windowsSource = (wsl -d $Distro -- wslpath -w $WslVhdPath).Trim()
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

$target = Join-Path $InstallDir "system_x64.vhd"
Copy-Item -Force -Path $windowsSource -Destination $target

$wslConfig = Join-Path $env:USERPROFILE ".wslconfig"
if (Test-Path $wslConfig) {
    $backup = "$wslConfig.manifold-backup-$(Get-Date -Format yyyyMMdd-HHmmss)"
    Copy-Item -Path $wslConfig -Destination $backup
    Write-Output "Backed up existing .wslconfig to $backup"
    $lines = Get-Content $wslConfig
} else {
    $lines = @()
}

$escapedTarget = $target.Replace("\", "\\")
$output = New-Object System.Collections.Generic.List[string]
$inWsl2 = $false
$sawWsl2 = $false
$setSystemDistro = $false

foreach ($line in $lines) {
    if ($line -match '^\s*\[.+\]\s*$') {
        if ($inWsl2 -and -not $setSystemDistro) {
            $output.Add("systemDistro=$escapedTarget")
            $setSystemDistro = $true
        }
        $inWsl2 = ($line -match '^\s*\[wsl2\]\s*$')
        if ($inWsl2) {
            $sawWsl2 = $true
        }
        $output.Add($line)
        continue
    }

    if ($inWsl2 -and $line -match '^\s*systemDistro\s*=') {
        $output.Add("systemDistro=$escapedTarget")
        $setSystemDistro = $true
    } else {
        $output.Add($line)
    }
}

if (-not $sawWsl2) {
    if ($output.Count -gt 0 -and $output[$output.Count - 1] -ne "") {
        $output.Add("")
    }
    $output.Add("[wsl2]")
    $output.Add("systemDistro=$escapedTarget")
} elseif ($inWsl2 -and -not $setSystemDistro) {
    $output.Add("systemDistro=$escapedTarget")
}

Set-Content -Path $wslConfig -Value $output -Encoding ascii
Write-Output "Installed private WSLg VHD at $target"
Write-Output "Updated $wslConfig with systemDistro=$escapedTarget"

if ($Shutdown) {
    Write-Output "Shutting down WSL so the private system distro is used on next launch..."
    wsl --shutdown
}
