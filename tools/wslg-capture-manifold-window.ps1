param([Parameter(Mandatory=$true)][string]$OutputPath)
$ErrorActionPreference = 'Stop'

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class ManifoldWslgCapture {
  public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc lpEnumFunc, IntPtr lParam);
  [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr hWnd, StringBuilder lpString, int nMaxCount);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hwnd, IntPtr hdcBlt, uint nFlags);
}
"@

$candidates = New-Object System.Collections.Generic.List[object]
[ManifoldWslgCapture]::EnumWindows({
  param($hWnd, $lParam)
  if ([ManifoldWslgCapture]::IsWindowVisible($hWnd)) {
    $titleBuilder = New-Object System.Text.StringBuilder 512
    [void][ManifoldWslgCapture]::GetWindowText($hWnd, $titleBuilder, $titleBuilder.Capacity)
    $title = $titleBuilder.ToString()
    if ($title -match '(?i)manifold') {
      $rect = New-Object ManifoldWslgCapture+RECT
      [void][ManifoldWslgCapture]::GetWindowRect($hWnd, [ref]$rect)
      $width = [Math]::Max(0, $rect.Right - $rect.Left)
      $height = [Math]::Max(0, $rect.Bottom - $rect.Top)
      $area = $width * $height
      if ($area -gt 10000) {
        $candidates.Add([pscustomobject]@{ Handle = $hWnd; Title = $title; Width = $width; Height = $height; Area = $area }) | Out-Null
      }
    }
  }
  $true
}, [IntPtr]::Zero) | Out-Null

if ($candidates.Count -eq 0) {
  throw 'No visible WSLg Manifold window found'
}

$target = $candidates | Sort-Object Area -Descending | Select-Object -First 1
$bitmap = New-Object System.Drawing.Bitmap $target.Width, $target.Height, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$hdc = $graphics.GetHdc()
try {
  $ok = [ManifoldWslgCapture]::PrintWindow($target.Handle, $hdc, 2)
} finally {
  $graphics.ReleaseHdc($hdc)
  $graphics.Dispose()
}
if (-not $ok) {
  throw ("PrintWindow failed for handle=0x{0:x}" -f $target.Handle.ToInt64())
}

$bitmap.Save($OutputPath, [System.Drawing.Imaging.ImageFormat]::Png)
$stepX = [Math]::Max(1, [Math]::Floor($bitmap.Width / 96))
$stepY = [Math]::Max(1, [Math]::Floor($bitmap.Height / 96))
$unique = New-Object 'System.Collections.Generic.HashSet[int]'
$sampled = 0
$bright = 0
$colorful = 0
for ($y = 0; $y -lt $bitmap.Height; $y += $stepY) {
  for ($x = 0; $x -lt $bitmap.Width; $x += $stepX) {
    $pixel = $bitmap.GetPixel($x, $y)
    [void]$unique.Add($pixel.ToArgb())
    $sampled += 1
    $max = [Math]::Max($pixel.R, [Math]::Max($pixel.G, $pixel.B))
    $min = [Math]::Min($pixel.R, [Math]::Min($pixel.G, $pixel.B))
    if (($pixel.R + $pixel.G + $pixel.B) -gt 90) { $bright += 1 }
    if (($max - $min) -gt 24) { $colorful += 1 }
  }
}
$bitmap.Dispose()

if ($unique.Count -lt 8) {
  throw "Capture looks blank: unique_colors=$($unique.Count) sampled=$sampled"
}
if ($bright -lt 8 -and $colorful -lt 8) {
  throw "Capture lacks visible client content: bright_samples=$bright colorful_samples=$colorful sampled=$sampled"
}

"handle=0x{0:x} title={1} size={2}x{3} sampled={4} unique_colors={5} bright_samples={6} colorful_samples={7} png={8}" -f $target.Handle.ToInt64(), $target.Title, $target.Width, $target.Height, $sampled, $unique.Count, $bright, $colorful, $OutputPath
