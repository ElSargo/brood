param(
    [switch]$Shutdown
)

$ErrorActionPreference = "Stop"

$wslConfig = Join-Path $env:USERPROFILE ".wslconfig"
if (-not (Test-Path $wslConfig)) {
    Write-Output "No .wslconfig exists; stock WSLg is already selected."
    return
}

$backup = "$wslConfig.manifold-stock-backup-$(Get-Date -Format yyyyMMdd-HHmmss)"
Copy-Item -Path $wslConfig -Destination $backup

$lines = Get-Content $wslConfig
$output = New-Object System.Collections.Generic.List[string]
$inWsl2 = $false

foreach ($line in $lines) {
    if ($line -match '^\s*\[.+\]\s*$') {
        $inWsl2 = ($line -match '^\s*\[wsl2\]\s*$')
        $output.Add($line)
        continue
    }

    if ($inWsl2 -and $line -match '^\s*systemDistro\s*=') {
        continue
    }

    $output.Add($line)
}

Set-Content -Path $wslConfig -Value $output -Encoding ascii
Write-Output "Removed systemDistro from $wslConfig"
Write-Output "Backed up previous config to $backup"

if ($Shutdown) {
    Write-Output "Shutting down WSL so stock WSLg is used on next launch..."
    wsl --shutdown
}
