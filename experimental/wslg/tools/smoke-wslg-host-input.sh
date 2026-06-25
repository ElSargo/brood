#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

require_host_presentation_environment

if ! command -v weston-eventdemo >/dev/null 2>&1; then
  echo "ERR weston-eventdemo is required for WSLg host input smoke" >&2
  exit 2
fi

cargo build -p manifoldd --offline >/dev/null

exec 9>/tmp/manifold-host-smoke.lock
flock 9

TMPDIR="$(mktemp -d /tmp/manifold-host-input-smoke.XXXXXX)"
DBUS_NAME="org.manifold.Test.p$$"
LOG="$TMPDIR/manifoldd.log"
CLIENT_LOG="$TMPDIR/weston-eventdemo.log"
WRAPPER="$TMPDIR/weston-eventdemo-wrapper.sh"
PS1="$TMPDIR/send-host-input.ps1"

cleanup() {
  if [[ -n "${PID:-}" ]] && kill -0 "$PID" 2>/dev/null; then
    target/debug/manifoldd ctl --dbus-name "$DBUS_NAME" quit >/dev/null 2>&1 || true
    sleep 0.5
    kill "$PID" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

cat >"$WRAPPER" <<EOF
#!/usr/bin/env bash
exec stdbuf -oL -eL weston-eventdemo \
  --log-motion --log-button --log-key --log-axis --log-focus --log-redraw --log-resize \
  >"$CLIENT_LOG" 2>&1
EOF
chmod +x "$WRAPPER"

RUST_LOG="${RUST_LOG:-manifoldd=info,smithay=warn}" \
  target/debug/manifoldd run --dbus-name "$DBUS_NAME" -c "$WRAPPER" >"$LOG" 2>&1 &
PID=$!

wait_for_manifoldd_control "$DBUS_NAME" "$PID" "$LOG"

ctl() {
  target/debug/manifoldd ctl --dbus-name "$DBUS_NAME" "$@"
}

field() {
  local status="$1"
  local key="$2"
  sed -n "s/.* ${key}=\\([^ ]*\\).*/\\1/p" <<<"$status"
}

require_delta_ge() {
  local before="$1"
  local after="$2"
  local key="$3"
  local min_delta="$4"
  local before_value after_value
  before_value="$(field "$before" "$key")"
  after_value="$(field "$after" "$key")"
  [[ "$before_value" =~ ^[0-9]+$ && "$after_value" =~ ^[0-9]+$ ]] || {
    echo "ERR missing numeric $key before/after" >&2
    echo "before=$before" >&2
    echo "after=$after" >&2
    exit 1
  }
  (( after_value - before_value >= min_delta )) || {
    echo "ERR expected $key delta >= $min_delta, got before=$before_value after=$after_value" >&2
    echo "before=$before" >&2
    echo "after=$after" >&2
    exit 1
  }
}

for _ in $(seq 1 100); do
  STATUS_BEFORE="$(ctl status)"
  if [[ "$(field "$STATUS_BEFORE" presentation)" == "Attached" ]] &&
    [[ "$(field "$STATUS_BEFORE" windows)" =~ ^[0-9]+$ ]] &&
    (( "$(field "$STATUS_BEFORE" windows)" >= 1 )) &&
    [[ "$(field "$STATUS_BEFORE" frames)" =~ ^[0-9]+$ ]] &&
    (( "$(field "$STATUS_BEFORE" frames)" >= 8 )); then
    break
  fi
  sleep 0.1
done

STATUS_BEFORE="$(ctl status)"
if [[ "$(field "$STATUS_BEFORE" presentation)" != "Attached" ]] ||
  [[ ! "$(field "$STATUS_BEFORE" windows)" =~ ^[0-9]+$ ]] ||
  (( "$(field "$STATUS_BEFORE" windows)" < 1 )) ||
  [[ ! "$(field "$STATUS_BEFORE" frames)" =~ ^[0-9]+$ ]] ||
  (( "$(field "$STATUS_BEFORE" frames)" < 8 )); then
  echo "ERR hosted client did not reach host-input readiness: $STATUS_BEFORE" >&2
  tail -120 "$LOG" >&2 || true
  exit 1
fi

cat >"$PS1" <<'EOF'
$ErrorActionPreference = 'Stop'
Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class ManifoldHostInputSmoke {
  public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc lpEnumFunc, IntPtr lParam);
  [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr hWnd, StringBuilder lpString, int nMaxCount);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int X, int Y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint flags, int dx, int dy, uint data, UIntPtr extraInfo);
  [DllImport("user32.dll")] public static extern void keybd_event(byte virtualKey, byte scanCode, uint flags, UIntPtr extraInfo);
}
"@

$candidates = New-Object System.Collections.Generic.List[object]
[ManifoldHostInputSmoke]::EnumWindows({
  param($hWnd, $lParam)
  if ([ManifoldHostInputSmoke]::IsWindowVisible($hWnd)) {
    $titleBuilder = New-Object System.Text.StringBuilder 512
    [void][ManifoldHostInputSmoke]::GetWindowText($hWnd, $titleBuilder, $titleBuilder.Capacity)
    $title = $titleBuilder.ToString()
    if ($title -match '(?i)manifold') {
      $rect = New-Object ManifoldHostInputSmoke+RECT
      [void][ManifoldHostInputSmoke]::GetWindowRect($hWnd, [ref]$rect)
      $width = [Math]::Max(0, $rect.Right - $rect.Left)
      $height = [Math]::Max(0, $rect.Bottom - $rect.Top)
      $area = $width * $height
      if ($area -gt 10000) {
        $candidates.Add([pscustomobject]@{ Handle = $hWnd; Title = $title; Left = $rect.Left; Top = $rect.Top; Width = $width; Height = $height; Area = $area }) | Out-Null
      }
    }
  }
  $true
}, [IntPtr]::Zero) | Out-Null

if ($candidates.Count -eq 0) {
  throw 'No visible WSLg Manifold window found'
}

$target = $candidates | Sort-Object Area -Descending | Select-Object -First 1
$x = $target.Left + [Math]::Min([Math]::Floor($target.Width / 2), 220)
$y = $target.Top + [Math]::Min([Math]::Floor($target.Height / 2), 180)
[void][ManifoldHostInputSmoke]::ShowWindow($target.Handle, 9)
[void][ManifoldHostInputSmoke]::SetForegroundWindow($target.Handle)
Start-Sleep -Milliseconds 250
[void][ManifoldHostInputSmoke]::SetCursorPos(($x - 20), ($y - 20))
Start-Sleep -Milliseconds 120
[void][ManifoldHostInputSmoke]::SetCursorPos($x, $y)
Start-Sleep -Milliseconds 150

$MOUSEEVENTF_LEFTDOWN = 0x0002
$MOUSEEVENTF_LEFTUP = 0x0004
$MOUSEEVENTF_MOVE = 0x0001
$KEYEVENTF_KEYUP = 0x0002
$VK_A = 0x41

[ManifoldHostInputSmoke]::mouse_event($MOUSEEVENTF_MOVE, 12, 8, 0, [UIntPtr]::Zero)
Start-Sleep -Milliseconds 80
[ManifoldHostInputSmoke]::mouse_event($MOUSEEVENTF_MOVE, -8, 10, 0, [UIntPtr]::Zero)
Start-Sleep -Milliseconds 80
[ManifoldHostInputSmoke]::mouse_event($MOUSEEVENTF_MOVE, 6, -6, 0, [UIntPtr]::Zero)
Start-Sleep -Milliseconds 120

for ($i = 0; $i -lt 3; $i++) {
  [void][ManifoldHostInputSmoke]::SetCursorPos($x, $y)
  Start-Sleep -Milliseconds 80
  [ManifoldHostInputSmoke]::mouse_event($MOUSEEVENTF_LEFTDOWN, 0, 0, 0, [UIntPtr]::Zero)
  Start-Sleep -Milliseconds 60
  [ManifoldHostInputSmoke]::mouse_event($MOUSEEVENTF_LEFTUP, 0, 0, 0, [UIntPtr]::Zero)
  Start-Sleep -Milliseconds 160
}

for ($i = 0; $i -lt 2; $i++) {
  [ManifoldHostInputSmoke]::keybd_event($VK_A, 0, 0, [UIntPtr]::Zero)
  Start-Sleep -Milliseconds 60
  [ManifoldHostInputSmoke]::keybd_event($VK_A, 0, $KEYEVENTF_KEYUP, [UIntPtr]::Zero)
  Start-Sleep -Milliseconds 120
}

"handle=0x{0:x} title={1} point={2},{3} size={4}x{5}" -f $target.Handle.ToInt64(), $target.Title, $x, $y, $target.Width, $target.Height
EOF

HOST_INPUT_RESULT="$(powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$(wslpath -w "$PS1")" | tr -d '\r')"

for _ in $(seq 1 50); do
  STATUS_AFTER="$(ctl status)"
  if [[ "$(field "$STATUS_AFTER" pointer_button_events)" =~ ^[0-9]+$ ]] &&
    (( "$(field "$STATUS_AFTER" pointer_button_events)" - "$(field "$STATUS_BEFORE" pointer_button_events)" >= 2 )) &&
    [[ "$(field "$STATUS_AFTER" keyboard_events)" =~ ^[0-9]+$ ]] &&
    (( "$(field "$STATUS_AFTER" keyboard_events)" - "$(field "$STATUS_BEFORE" keyboard_events)" >= 2 )); then
    break
  fi
  sleep 0.1
done

STATUS_AFTER="$(ctl status)"
require_delta_ge "$STATUS_BEFORE" "$STATUS_AFTER" pointer_motion_events 1
require_delta_ge "$STATUS_BEFORE" "$STATUS_AFTER" pointer_button_events 2
require_delta_ge "$STATUS_BEFORE" "$STATUS_AFTER" keyboard_events 2

grep -q "focus x:" "$CLIENT_LOG" || {
  echo "ERR eventdemo did not log pointer focus from host input" >&2
  cat "$CLIENT_LOG" >&2 || true
  exit 1
}
grep -q "button time: .*button: 272, state: pressed" "$CLIENT_LOG" || {
  echo "ERR eventdemo did not log host button press" >&2
  cat "$CLIENT_LOG" >&2 || true
  exit 1
}
grep -q "button time: .*button: 272, state: released" "$CLIENT_LOG" || {
  echo "ERR eventdemo did not log host button release" >&2
  cat "$CLIENT_LOG" >&2 || true
  exit 1
}
grep -q "key key: .*state: pressed" "$CLIENT_LOG" || {
  echo "ERR eventdemo did not log host key press" >&2
  cat "$CLIENT_LOG" >&2 || true
  exit 1
}
grep -q "key key: .*state: released" "$CLIENT_LOG" || {
  echo "ERR eventdemo did not log host key release" >&2
  cat "$CLIENT_LOG" >&2 || true
  exit 1
}

echo "OK WSLg host input smoke passed"
echo "host_input=$HOST_INPUT_RESULT"
echo "status_before=$STATUS_BEFORE"
echo "status_after=$STATUS_AFTER"
echo "client_log=$CLIENT_LOG"
echo "log=$LOG"

ctl quit >/dev/null || true
wait "$PID" || true
trap - EXIT
