#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

require_host_presentation_environment

CLIENT="${MANIFOLD_SMOKE_CLIENT:-}"
if [[ -z "$CLIENT" ]]; then
  if command -v weston-simple-egl >/dev/null 2>&1; then
    CLIENT="$(command -v weston-simple-egl)"
  elif command -v weston-terminal >/dev/null 2>&1; then
    CLIENT="$(command -v weston-terminal)"
  else
    echo "ERR no smoke client found; install weston-simple-egl or set MANIFOLD_SMOKE_CLIENT" >&2
    exit 2
  fi
fi

MAX_CLOSE_DETACH_LATENCY_US="${MANIFOLD_MAX_CLOSE_DETACH_LATENCY_US:-75000}"
if ! [[ "$MAX_CLOSE_DETACH_LATENCY_US" =~ ^[0-9]+$ ]] || (( MAX_CLOSE_DETACH_LATENCY_US < 1 )); then
  echo "ERR MANIFOLD_MAX_CLOSE_DETACH_LATENCY_US must be a positive integer" >&2
  exit 2
fi
MAX_CLOSE_REATTACH_LATENCY_US="${MANIFOLD_MAX_CLOSE_REATTACH_LATENCY_US:-75000}"
if ! [[ "$MAX_CLOSE_REATTACH_LATENCY_US" =~ ^[0-9]+$ ]] || (( MAX_CLOSE_REATTACH_LATENCY_US < 1 )); then
  echo "ERR MANIFOLD_MAX_CLOSE_REATTACH_LATENCY_US must be a positive integer" >&2
  exit 2
fi

cargo build -p manifoldd --offline >/dev/null

exec 9>/tmp/manifold-host-smoke.lock
flock 9

TMPDIR="$(mktemp -d /tmp/manifold-close-smoke.XXXXXX)"
SOCK="$TMPDIR/control.sock"
LOG="$TMPDIR/manifoldd.log"
TRACE="$TMPDIR/client-wayland.log"
WRAPPER="$TMPDIR/client-wrapper.sh"
PS1="$TMPDIR/close-manifold-window.ps1"
CAPTURE_PS1="$ROOT/experimental/wslg/tools/wslg-capture-manifold-window.ps1"
PNG_REATTACHED="$TMPDIR/manifold-window-after-close-resume.png"

cleanup() {
  if [[ -n "${PID:-}" ]] && kill -0 "$PID" 2>/dev/null; then
    target/debug/manifoldd ctl --control-socket "$SOCK" quit >/dev/null 2>&1 || true
    sleep 0.5
    kill "$PID" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

printf '#!/usr/bin/env bash\nexec env WAYLAND_DEBUG=1 %q 2>%q\n' "$CLIENT" "$TRACE" >"$WRAPPER"
chmod +x "$WRAPPER"

RUST_LOG="${RUST_LOG:-manifoldd=info,smithay=warn}" \
  target/debug/manifoldd run --control-socket "$SOCK" -c "$WRAPPER" >"$LOG" 2>&1 &
PID=$!

wait_for_manifoldd_control "$SOCK" "$PID" "$LOG"

ctl() {
  target/debug/manifoldd ctl --control-socket "$SOCK" "$@"
}

capture_window() {
  local png="$1"
  local result
  local rc
  for _ in $(seq 1 20); do
    set +e
    result="$(powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$(wslpath -w "$CAPTURE_PS1")" "$(wslpath -w "$png")" 2>&1 | tr -d '\r')"
    rc=$?
    set -e
    if [[ "$rc" == "0" && -s "$png" ]]; then
      echo "$result"
      return 0
    fi
    sleep 0.1
  done
  echo "ERR visual capture failed for $png" >&2
  echo "$result" >&2
  exit 1
}

field() {
  local status="$1"
  local key="$2"
  sed -n "s/.* ${key}=\\([^ ]*\\).*/\\1/p" <<<"$status"
}

require_field_ge() {
  local status="$1"
  local key="$2"
  local min="$3"
  local value
  value="$(field "$status" "$key")"
  [[ -n "$value" ]] || {
    echo "ERR missing $key in status: $status" >&2
    exit 1
  }
  (( value >= min )) || {
    echo "ERR expected $key >= $min, got $value in status: $status" >&2
    exit 1
  }
}

wait_status() {
  local predicate="$1"
  local status
  for _ in $(seq 1 100); do
    status="$(ctl status)"
    if eval "$predicate"; then
      echo "$status"
      return 0
    fi
    sleep 0.1
  done
  echo "ERR timed out waiting for status predicate: $predicate" >&2
  ctl status >&2 || true
  tail -120 "$LOG" >&2 || true
  exit 1
}

STATUS1="$(wait_status '[[ "$(field "$status" presentation)" == "Attached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" frames)" =~ ^[0-9]+$ ]] && (( "$(field "$status" frames)" >= 2 ))')"
FRAMES1="$(field "$STATUS1" frames)"
COMMITS1="$(field "$STATUS1" commits)"
DETACHES1="$(field "$STATUS1" detach_count)"
require_field_ge "$STATUS1" windows 1
require_field_ge "$STATUS1" frames 2

cat >"$PS1" <<'EOF'
$ErrorActionPreference = 'Stop'
Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class ManifoldCloseSmoke {
  public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc lpEnumFunc, IntPtr lParam);
  [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr hWnd, StringBuilder lpString, int nMaxCount);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
  [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr hWnd, uint Msg, UIntPtr wParam, IntPtr lParam);
}
"@

$candidates = New-Object System.Collections.Generic.List[object]
[ManifoldCloseSmoke]::EnumWindows({
  param($hWnd, $lParam)
  if ([ManifoldCloseSmoke]::IsWindowVisible($hWnd)) {
    $titleBuilder = New-Object System.Text.StringBuilder 512
    [void][ManifoldCloseSmoke]::GetWindowText($hWnd, $titleBuilder, $titleBuilder.Capacity)
    $title = $titleBuilder.ToString()
    if ($title -match '(?i)manifold') {
      $rect = New-Object ManifoldCloseSmoke+RECT
      [void][ManifoldCloseSmoke]::GetWindowRect($hWnd, [ref]$rect)
      $area = [Math]::Max(0, $rect.Right - $rect.Left) * [Math]::Max(0, $rect.Bottom - $rect.Top)
      $candidates.Add([pscustomobject]@{ Handle = $hWnd; Title = $title; Area = $area }) | Out-Null
    }
  }
  $true
}, [IntPtr]::Zero) | Out-Null

if ($candidates.Count -eq 0) {
  throw 'No WSLg Manifold window found'
}

$WM_CLOSE = 0x0010
$closed = New-Object System.Collections.Generic.List[string]
foreach ($target in ($candidates | Sort-Object Area -Descending)) {
  [void][ManifoldCloseSmoke]::PostMessage($target.Handle, $WM_CLOSE, [UIntPtr]::Zero, [IntPtr]::Zero)
  $closed.Add(("handle=0x{0:x} title={1} area={2}" -f $target.Handle.ToInt64(), $target.Title, $target.Area)) | Out-Null
}

"closed " + ($closed -join "; ")
EOF

powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$(wslpath -w "$PS1")" | tr -d '\r'

HOST_WM_CLOSE_DELIVERED=no
for _ in $(seq 1 20); do
  STATUS2="$(ctl status)"
  if [[ "$(field "$STATUS2" presentation)" == "Detached" ]] &&
    [[ "$(field "$STATUS2" windows)" =~ ^[0-9]+$ ]] &&
    (( "$(field "$STATUS2" windows)" >= 1 )) &&
    [[ "$(field "$STATUS2" detach_count)" =~ ^[0-9]+$ ]] &&
    (( "$(field "$STATUS2" detach_count)" > DETACHES1 )); then
    HOST_WM_CLOSE_DELIVERED=yes
    break
  fi
  sleep 0.1
done

if [[ "$HOST_WM_CLOSE_DELIVERED" == no ]]; then
  ctl close-frontend >/dev/null
fi

STATUS2="$(wait_status '[[ "$(field "$status" presentation)" == "Detached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" detach_count)" =~ ^[0-9]+$ ]] && (( "$(field "$status" detach_count)" > DETACHES1 ))')"
require_field_ge "$STATUS2" windows 1
CALLBACKS2="$(field "$STATUS2" client_frame_callbacks)"
[[ "$CALLBACKS2" =~ ^[0-9]+$ ]] || {
  echo "ERR missing client_frame_callbacks in detached status: $STATUS2" >&2
  exit 1
}
LAST_DETACH_LATENCY="$(field "$STATUS2" last_detach_latency_us)"
[[ "$LAST_DETACH_LATENCY" =~ ^[0-9]+$ ]] || {
  echo "ERR missing measured close detach latency: $STATUS2" >&2
  exit 1
}
(( LAST_DETACH_LATENCY <= MAX_CLOSE_DETACH_LATENCY_US )) || {
  echo "ERR close detach latency ${LAST_DETACH_LATENCY}us exceeds ${MAX_CLOSE_DETACH_LATENCY_US}us" >&2
  echo "status=$STATUS2" >&2
  exit 1
}

ctl attach >/dev/null
STATUS3="$(wait_status '[[ "$(field "$status" presentation)" == "Attached" ]] && [[ "$(field "$status" windows)" =~ ^[0-9]+$ ]] && (( "$(field "$status" windows)" >= 1 )) && [[ "$(field "$status" frames)" =~ ^[0-9]+$ ]] && (( "$(field "$status" frames)" > FRAMES1 )) && [[ "$(field "$status" client_frame_callbacks)" =~ ^[0-9]+$ ]] && (( "$(field "$status" client_frame_callbacks)" > CALLBACKS2 ))')"
require_field_ge "$STATUS3" commits "$COMMITS1"
require_field_ge "$STATUS3" attach_count 2
LAST_ATTACH_TO_FRAME_LATENCY="$(field "$STATUS3" last_attach_to_frame_us)"
[[ "$LAST_ATTACH_TO_FRAME_LATENCY" =~ ^[0-9]+$ ]] || {
  echo "ERR missing measured close reattach-to-frame latency: $STATUS3" >&2
  exit 1
}
(( LAST_ATTACH_TO_FRAME_LATENCY <= MAX_CLOSE_REATTACH_LATENCY_US )) || {
  echo "ERR close reattach-to-frame latency ${LAST_ATTACH_TO_FRAME_LATENCY}us exceeds ${MAX_CLOSE_REATTACH_LATENCY_US}us" >&2
  echo "status=$STATUS3" >&2
  exit 1
}
CAPTURE_REATTACHED="$(capture_window "$PNG_REATTACHED")"

echo "OK WSLg close/resume smoke passed"
echo "host_wm_close_delivered=$HOST_WM_CLOSE_DELIVERED"
echo "last_detach_latency_us=$LAST_DETACH_LATENCY"
echo "max_close_detach_latency_budget_us=$MAX_CLOSE_DETACH_LATENCY_US"
echo "last_attach_to_frame_us=$LAST_ATTACH_TO_FRAME_LATENCY"
echo "max_close_reattach_latency_budget_us=$MAX_CLOSE_REATTACH_LATENCY_US"
echo "client_frame_callbacks_after_detach=$CALLBACKS2"
echo "capture_reattached=$CAPTURE_REATTACHED"
echo "png_reattached=$PNG_REATTACHED"
echo "client=$CLIENT"
echo "status_initial=$STATUS1"
echo "status_after_wm_close=$STATUS2"
echo "status_reattached=$STATUS3"
echo "log=$LOG"
echo "wayland_trace=$TRACE"

ctl quit >/dev/null || true
wait "$PID" || true
trap - EXIT
