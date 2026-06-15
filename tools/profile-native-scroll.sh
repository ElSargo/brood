#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

require_host_presentation_environment

XKB_LIB_DIR="${MANIFOLD_XKBCOMMON_LIB_DIR:-/nix/store/vcf7irc4an6ffxi1qin2kwv7qdggnfcr-libxkbcommon-1.13.1/lib}"
export LIBRARY_PATH="$XKB_LIB_DIR${LIBRARY_PATH:+:$LIBRARY_PATH}"
export LD_LIBRARY_PATH="$XKB_LIB_DIR${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

DURATION_SECONDS="${MANIFOLD_NATIVE_SCROLL_DURATION_SECONDS:-6.5}"
ACTIVE_START_US="${MANIFOLD_NATIVE_SCROLL_ACTIVE_START_US:-1000000}"
ACTIVE_END_US="${MANIFOLD_NATIVE_SCROLL_ACTIVE_END_US:-5500000}"
MIN_SCROLL_DELTA_Y="${MANIFOLD_NATIVE_SCROLL_MIN_DELTA_Y:-1000}"
MIN_FRAME_FPS="${MANIFOLD_NATIVE_SCROLL_MIN_FRAME_FPS:-59.0}"
MIN_COMMIT_FPS="${MANIFOLD_NATIVE_SCROLL_MIN_COMMIT_FPS:-59.0}"
MAX_FRAME_INTERVAL_P95_US="${MANIFOLD_NATIVE_SCROLL_MAX_FRAME_INTERVAL_P95_US:-20000}"
MAX_COMMIT_TO_FRAME_P95_US="${MANIFOLD_NATIVE_SCROLL_MAX_COMMIT_TO_FRAME_P95_US:-20000}"
MAX_INPUT_TO_FRAME_P95_US="${MANIFOLD_NATIVE_SCROLL_MAX_INPUT_TO_FRAME_P95_US:-20000}"
MIN_INPUT_COMMIT_FRAME_RATIO="${MANIFOLD_NATIVE_SCROLL_MIN_INPUT_COMMIT_FRAME_RATIO:-0}"
OUTPUT_DIR="${MANIFOLD_NATIVE_SCROLL_OUTPUT_DIR:-}"

if [[ -z "$OUTPUT_DIR" ]]; then
  TMPDIR="$(mktemp -d /tmp/manifold-native-scroll.XXXXXX)"
else
  TMPDIR="$OUTPUT_DIR"
  rm -rf "$TMPDIR"
  mkdir -p "$TMPDIR"
fi

SITE_DIR="$TMPDIR/site"
START_FLAG="$TMPDIR/start-scroll"
SOCK="$TMPDIR/control.sock"
LOG="$TMPDIR/manifoldd.log"
HTTP_LOG="$TMPDIR/http.log"
SAMPLES="$TMPDIR/samples.jsonl"
FINAL_STATUS="$TMPDIR/final-status.json"
METRICS="$TMPDIR/metrics.txt"
WRAPPER="$TMPDIR/browser-wrapper.sh"
BROWSER_PROFILE="$TMPDIR/browser-profile"

cleanup() {
  if [[ -n "${PID:-}" ]] && kill -0 "$PID" 2>/dev/null; then
    target/debug/manifoldd ctl --control-socket "$SOCK" quit >/dev/null 2>&1 || true
    sleep 0.5
    kill "$PID" >/dev/null 2>&1 || true
  fi
  if [[ -n "${HTTP_PID:-}" ]]; then
    kill "$HTTP_PID" >/dev/null 2>&1 || true
  fi
  if [[ -n "${INJECT_PID:-}" ]]; then
    wait "$INJECT_PID" 2>/dev/null || true
  fi
  pkill -f -- "$BROWSER_PROFILE" >/dev/null 2>&1 || true
}
trap cleanup EXIT

mkdir -p "$SITE_DIR" "$BROWSER_PROFILE"

cat >"$SITE_DIR/scroll.html" <<'HTML'
<!doctype html>
<meta charset="utf-8">
<title>Manifold Native Scroll Profile</title>
<style>
  html { scroll-behavior: auto; }
  body { margin: 0; font: 16px system-ui, sans-serif; background: #111; color: #eee; }
  .row { height: 56px; display: flex; align-items: center; padding: 0 24px; border-bottom: 1px solid #333; }
  .row:nth-child(4n) { background: #1d2835; }
  .row:nth-child(4n+1) { background: #202820; }
  .row:nth-child(4n+2) { background: #282026; }
  .row:nth-child(4n+3) { background: #252513; }
</style>
<script>
  function sleep(ms) {
    return new Promise((resolve) => setTimeout(resolve, ms));
  }

  async function waitForStart() {
    for (;;) {
      try {
        const response = await fetch(`/start?cache=${Date.now()}`, { cache: "no-store" });
        if (response.status === 204) return;
      } catch (_) {}
      await sleep(50);
    }
  }

  function reportScrollY(y) {
    fetch(`/scroll-y/${Math.round(y)}`, { cache: "no-store" }).catch(() => {});
  }

  addEventListener("load", async () => {
    const fragment = document.createDocumentFragment();
    for (let i = 0; i < 2400; i++) {
      const row = document.createElement("div");
      row.className = "row";
      row.textContent = `Native scroll cadence row ${i.toString().padStart(4, "0")}`;
      fragment.appendChild(row);
    }
    document.body.appendChild(fragment);

    await waitForStart();

    let start = 0;
    let lastReport = 0;
    function step(timestamp) {
      if (!start) start = timestamp;
      window.scrollBy(0, 10);
      if (timestamp - lastReport > 500) {
        lastReport = timestamp;
        reportScrollY(scrollY);
      }
      if (timestamp - start < 5500) {
        requestAnimationFrame(step);
      } else {
        reportScrollY(scrollY);
      }
    }
    requestAnimationFrame(step);
  });
</script>
HTML

cat >"$TMPDIR/http-server.py" <<'PY'
import http.server
import pathlib
import sys
import urllib.parse

site = pathlib.Path(sys.argv[1])
start_flag = pathlib.Path(sys.argv[2])

class Handler(http.server.SimpleHTTPRequestHandler):
    def translate_path(self, path):
        path = urllib.parse.urlparse(path).path
        if path == "/":
            path = "/scroll.html"
        return str(site / path.lstrip("/"))

    def do_GET(self):
        path = urllib.parse.urlparse(self.path).path
        if path == "/start":
            if start_flag.exists():
                self.send_response(204)
            else:
                self.send_response(425)
            self.end_headers()
            return
        if path.startswith("/scroll-y/"):
            value = path.rsplit("/", 1)[-1]
            print(f"SCROLL_Y {value}", flush=True)
            self.send_response(204)
            self.end_headers()
            return
        super().do_GET()

    def log_message(self, fmt, *args):
        print(fmt % args, flush=True)

server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
print(f"PORT={server.server_port}", flush=True)
server.serve_forever()
PY

python3 "$TMPDIR/http-server.py" "$SITE_DIR" "$START_FLAG" >"$HTTP_LOG" 2>&1 &
HTTP_PID=$!

PORT=""
for _ in $(seq 1 100); do
  if [[ -s "$HTTP_LOG" ]]; then
    PORT="$(sed -n 's/^PORT=//p' "$HTTP_LOG" | head -1)"
    [[ -n "$PORT" ]] && break
  fi
  kill -0 "$HTTP_PID" 2>/dev/null || {
    echo "ERR local scroll HTTP server exited" >&2
    cat "$HTTP_LOG" >&2 || true
    exit 1
  }
  sleep 0.05
done
[[ -n "$PORT" ]] || {
  echo "ERR local scroll HTTP server did not report a port" >&2
  cat "$HTTP_LOG" >&2 || true
  exit 1
}

URL="http://127.0.0.1:$PORT/scroll.html"

if [[ -n "${MANIFOLD_NATIVE_SCROLL_BROWSER_CMD:-}" ]]; then
  printf '#!/usr/bin/env bash\nexec env WAYLAND_DISPLAY="$WAYLAND_DISPLAY" XDG_SESSION_TYPE=wayland GDK_BACKEND=wayland bash -lc %q\n' \
    "$MANIFOLD_NATIVE_SCROLL_BROWSER_CMD \"$URL\"" >"$WRAPPER"
elif command -v flatpak >/dev/null 2>&1 && flatpak info com.brave.Browser >/dev/null 2>&1; then
  cat >"$WRAPPER" <<EOF
#!/usr/bin/env bash
exec flatpak run \\
  --env=WAYLAND_DISPLAY="\$WAYLAND_DISPLAY" \\
  --env=XDG_SESSION_TYPE=wayland \\
  --env=GDK_BACKEND=wayland \\
  com.brave.Browser \\
  --user-data-dir=$BROWSER_PROFILE \\
  --no-first-run \\
  --no-default-browser-check \\
  --disable-background-networking \\
  --disable-extensions \\
  --disable-features=Vulkan,DefaultANGLEVulkan,VulkanFromANGLE \\
  --enable-features=UseOzonePlatform \\
  --ozone-platform=wayland \\
  --new-window $URL >"$TMPDIR/browser.log" 2>&1
EOF
else
  echo "SKIP native scroll profile requires com.brave.Browser Flatpak or MANIFOLD_NATIVE_SCROLL_BROWSER_CMD" >&2
  exit 77
fi
chmod +x "$WRAPPER"

cargo build -p manifoldd --offline >/dev/null

exec 9>/tmp/manifold-native-scroll.lock
flock 9

RUST_LOG="${RUST_LOG:-manifoldd=info,smithay=warn}" \
  target/debug/manifoldd run --control-socket "$SOCK" -c "$WRAPPER" >"$LOG" 2>&1 &
PID=$!

wait_for_manifoldd_control "$SOCK" "$PID" "$LOG"

ctl() {
  target/debug/manifoldd ctl --control-socket "$SOCK" "$@"
}

READY=0
for _ in $(seq 1 300); do
  STATUS_JSON="$(ctl status-json)"
  if python3 - "$STATUS_JSON" <<'PY'
import json
import sys

data = json.loads(sys.argv[1])
if (
    data.get("presentation") == "Attached"
    and data.get("windows", 0) >= 1
    and data.get("focused_windows", 0) >= 1
    and data.get("frames", 0) >= 30
    and data.get("commits", 0) >= 1
):
    raise SystemExit(0)
raise SystemExit(1)
PY
  then
    READY=1
    break
  fi
  sleep 0.1
done

if [[ "$READY" != 1 ]]; then
  echo "ERR native scroll browser did not map and commit" >&2
  ctl status-json >&2 || true
  tail -160 "$LOG" >&2 || true
  tail -160 "$TMPDIR/browser.log" >&2 || true
  exit 1
fi

ctl inject center-motion >/dev/null

python3 - "$SOCK" "$ROOT/target/debug/manifoldd" "$SAMPLES" "$DURATION_SECONDS" <<'PY' &
import json
import subprocess
import sys
import time

sock, ctl, out, duration = sys.argv[1], sys.argv[2], sys.argv[3], float(sys.argv[4])
start = time.monotonic_ns()
last = None
with open(out, "w", encoding="utf-8") as handle:
    while (time.monotonic_ns() - start) < duration * 1_000_000_000:
        raw = subprocess.check_output(
            [ctl, "ctl", "--control-socket", sock, "status-json"],
            text=True,
        )
        now = time.monotonic_ns()
        status = json.loads(raw)
        sample = {"t_us": (now - start) // 1000, "status": status}
        if last is not None:
            dt = max(1, sample["t_us"] - last["t_us"])
            previous = last["status"]
            sample["delta"] = {
                "dt_us": dt,
                "frames": status["frames"] - previous["frames"],
                "commits": status["commits"] - previous["commits"],
                "callbacks": status["client_frame_callbacks"] - previous["client_frame_callbacks"],
                "inputs": status["input_events"] - previous["input_events"],
                "axis": status["pointer_axis_events"] - previous["pointer_axis_events"],
                "fps": (status["frames"] - previous["frames"]) * 1_000_000 / dt,
            }
        handle.write(json.dumps(sample, separators=(",", ":")) + "\n")
        handle.flush()
        last = sample
        time.sleep(0.05)
PY
SAMPLER_PID=$!

sleep 0.2
touch "$START_FLAG"

(
  for _ in $(seq 1 300); do
    ctl inject wheel 0 -120 >/dev/null || true
    sleep 0.016
  done
) &
INJECT_PID=$!

wait "$SAMPLER_PID"
wait "$INJECT_PID" || true

ctl status-json >"$FINAL_STATUS"

python3 - \
  "$SAMPLES" \
  "$FINAL_STATUS" \
  "$HTTP_LOG" \
  "$ACTIVE_START_US" \
  "$ACTIVE_END_US" \
  "$MIN_SCROLL_DELTA_Y" \
  "$MIN_FRAME_FPS" \
  "$MIN_COMMIT_FPS" \
  "$MAX_FRAME_INTERVAL_P95_US" \
  "$MAX_COMMIT_TO_FRAME_P95_US" \
  "$MAX_INPUT_TO_FRAME_P95_US" \
  "$MIN_INPUT_COMMIT_FRAME_RATIO" \
  "$TMPDIR" <<'PY' | tee "$METRICS"
import json
import re
import statistics
import sys

(
    samples_path,
    final_status_path,
    http_log_path,
    active_start_us,
    active_end_us,
    min_scroll_delta_y,
    min_frame_fps,
    min_commit_fps,
    max_frame_interval_p95_us,
    max_commit_to_frame_p95_us,
    max_input_to_frame_p95_us,
    min_input_commit_frame_ratio,
    output_dir,
) = sys.argv[1:]

active_start_us = int(active_start_us)
active_end_us = int(active_end_us)
min_scroll_delta_y = int(min_scroll_delta_y)
min_frame_fps = float(min_frame_fps)
min_commit_fps = float(min_commit_fps)
max_frame_interval_p95_us = int(max_frame_interval_p95_us)
max_commit_to_frame_p95_us = int(max_commit_to_frame_p95_us)
max_input_to_frame_p95_us = int(max_input_to_frame_p95_us)
min_input_commit_frame_ratio = float(min_input_commit_frame_ratio)

with open(samples_path, encoding="utf-8") as handle:
    samples = [json.loads(line) for line in handle if line.strip()]
with open(final_status_path, encoding="utf-8") as handle:
    final_status = json.load(handle)
with open(http_log_path, encoding="utf-8", errors="replace") as handle:
    http_log = handle.read()

scroll_y = [int(match.group(1)) for match in re.finditer(r"SCROLL_Y ([0-9]+)", http_log)]
active = [
    sample
    for sample in samples
    if "delta" in sample and active_start_us <= sample["t_us"] <= active_end_us
]

def pct(values, quantile):
    if not values:
        return None
    ordered = sorted(values)
    index = min(len(ordered) - 1, round((len(ordered) - 1) * quantile))
    return ordered[index]

def require(condition, message):
    if not condition:
        raise SystemExit(f"ERR {message}")

require(len(samples) >= 10, f"too few status samples: {len(samples)}")
require(len(active) >= 10, f"too few active scroll samples: {len(active)}")
require(scroll_y, "page did not report scrollY samples")

duration_s = (active[-1]["t_us"] - active[0]["t_us"]) / 1_000_000
frame_count = active[-1]["status"]["frames"] - active[0]["status"]["frames"]
commit_count = active[-1]["status"]["commits"] - active[0]["status"]["commits"]
callback_count = active[-1]["status"]["client_frame_callbacks"] - active[0]["status"]["client_frame_callbacks"]
input_count = active[-1]["status"]["input_events"] - active[0]["status"]["input_events"]
axis_count = active[-1]["status"]["pointer_axis_events"] - active[0]["status"]["pointer_axis_events"]
input_commit_frame_count = active[-1]["status"]["input_commit_frame_count"] - active[0]["status"]["input_commit_frame_count"]
input_without_commit_frame_count = active[-1]["status"]["input_without_commit_frame_count"] - active[0]["status"]["input_without_commit_frame_count"]
client_frame_callback_commit_count = active[-1]["status"]["client_frame_callback_commit_count"] - active[0]["status"]["client_frame_callback_commit_count"]
scroll_delta = scroll_y[-1] - scroll_y[0]
frame_fps = frame_count / duration_s
commit_fps = commit_count / duration_s
input_commit_denominator = input_commit_frame_count + input_without_commit_frame_count
input_commit_frame_ratio = (
    input_commit_frame_count / input_commit_denominator
    if input_commit_denominator
    else None
)

frame_intervals = [
    sample["status"]["last_frame_interval_us"]
    for sample in active
    if sample["status"].get("last_frame_interval_us")
]
commit_latencies = [
    sample["status"]["last_commit_to_frame_us"]
    for sample in active
    if sample["status"].get("last_commit_to_frame_us") is not None
]
input_latencies = [
    sample["status"]["last_input_to_frame_us"]
    for sample in active
    if sample["status"].get("last_input_to_frame_us") is not None
]
commit_submit_latencies = [
    sample["status"]["last_commit_to_submit_us"]
    for sample in active
    if sample["status"].get("last_commit_to_submit_us") is not None
]
input_submit_latencies = [
    sample["status"]["last_input_to_submit_us"]
    for sample in active
    if sample["status"].get("last_input_to_submit_us") is not None
]
input_commit_latencies = [
    sample["status"]["last_input_to_commit_us"]
    for sample in active
    if sample["status"].get("last_input_to_commit_us") is not None
]
input_commit_submit_latencies = [
    sample["status"]["last_input_commit_to_submit_us"]
    for sample in active
    if sample["status"].get("last_input_commit_to_submit_us") is not None
]
client_callback_commit_latencies = [
    sample["status"]["last_client_frame_callback_to_commit_us"]
    for sample in active
    if sample["status"].get("last_client_frame_callback_to_commit_us") is not None
]
render_times = [
    sample["status"]["last_frame_us"]
    for sample in active
    if sample["status"].get("last_frame_us") is not None
]

frame_interval_p95 = pct(frame_intervals, 0.95)
commit_to_frame_p95 = pct(commit_latencies, 0.95)
input_to_frame_p95 = pct(input_latencies, 0.95)
commit_to_submit_p95 = pct(commit_submit_latencies, 0.95)
input_to_submit_p95 = pct(input_submit_latencies, 0.95)
input_to_commit_p95 = pct(input_commit_latencies, 0.95)
input_commit_to_submit_p95 = pct(input_commit_submit_latencies, 0.95)
client_frame_callback_to_commit_p95 = pct(client_callback_commit_latencies, 0.95)
render_p95 = pct(render_times, 0.95)

require(scroll_delta >= min_scroll_delta_y, f"scroll delta {scroll_delta} < {min_scroll_delta_y}")
require(frame_fps >= min_frame_fps, f"active frame fps {frame_fps:.3f} < {min_frame_fps}")
require(commit_fps >= min_commit_fps, f"active commit fps {commit_fps:.3f} < {min_commit_fps}")
require(callback_count >= frame_count - 2, f"client frame callbacks did not track frames: callbacks={callback_count} frames={frame_count}")
require(axis_count >= 120, f"too few wheel events reached compositor: {axis_count}")
require(input_count >= 120, f"too few input events reached compositor: {input_count}")
require(frame_interval_p95 is not None and frame_interval_p95 <= max_frame_interval_p95_us, f"frame interval p95 {frame_interval_p95}us > {max_frame_interval_p95_us}us")
require(commit_to_frame_p95 is not None and commit_to_frame_p95 <= max_commit_to_frame_p95_us, f"commit-to-frame p95 {commit_to_frame_p95}us > {max_commit_to_frame_p95_us}us")
require(input_to_frame_p95 is not None and input_to_frame_p95 <= max_input_to_frame_p95_us, f"input-to-frame p95 {input_to_frame_p95}us > {max_input_to_frame_p95_us}us")
if min_input_commit_frame_ratio > 0:
    require(input_commit_frame_ratio is not None, "no input frames available for input-commit frame ratio")
    require(input_commit_frame_ratio >= min_input_commit_frame_ratio, f"input-commit frame ratio {input_commit_frame_ratio:.3f} < {min_input_commit_frame_ratio:.3f}")
require(final_status["presentation"] == "Attached", f"presentation not attached: {final_status['presentation']}")
require(final_status["windows"] >= 1, f"expected mapped browser window: {final_status['windows']}")
require(final_status["dmabuf"] == "v4", f"expected dmabuf v4, got {final_status['dmabuf']}")
require(final_status["explicit_sync"] == "v1", f"expected explicit sync v1, got {final_status['explicit_sync']}")
require(final_status["dmabuf_rejected"] == 0, f"dmabuf imports rejected: {final_status['dmabuf_rejected']}")
require(final_status["dmabuf_blocker_failures"] == 0, f"dmabuf blocker failures: {final_status['dmabuf_blocker_failures']}")
require(final_status["dmabuf_explicit_sync_blockers"] >= commit_count, "explicit sync blockers did not cover active commits")

print("OK native scroll profile passed")
print(f"output_dir={output_dir}")
print(f"scroll_y_first={scroll_y[0]}")
print(f"scroll_y_last={scroll_y[-1]}")
print(f"scroll_delta_y={scroll_delta}")
print(f"active_duration_s={duration_s:.3f}")
print(f"active_frames={frame_count}")
print(f"active_commits={commit_count}")
print(f"active_client_frame_callbacks={callback_count}")
print(f"active_input_events={input_count}")
print(f"active_axis_events={axis_count}")
print(f"active_input_commit_frames={input_commit_frame_count}")
print(f"active_input_without_commit_frames={input_without_commit_frame_count}")
print(f"active_input_commit_frame_ratio={input_commit_frame_ratio:.3f}" if input_commit_frame_ratio is not None else "active_input_commit_frame_ratio=none")
print(f"active_client_frame_callback_commits={client_frame_callback_commit_count}")
print(f"active_frame_fps={frame_fps:.3f}")
print(f"active_commit_fps={commit_fps:.3f}")
print(f"frame_interval_p95_us={frame_interval_p95}")
print(f"commit_to_frame_p95_us={commit_to_frame_p95}")
print(f"input_to_frame_p95_us={input_to_frame_p95}")
print(f"commit_to_submit_p95_us={commit_to_submit_p95}")
print(f"input_to_submit_p95_us={input_to_submit_p95}")
print(f"input_to_commit_p95_us={input_to_commit_p95}")
print(f"input_commit_to_submit_p95_us={input_commit_to_submit_p95}")
print(f"client_frame_callback_to_commit_p95_us={client_frame_callback_to_commit_p95}")
print(f"frame_render_p95_us={render_p95}")
print(f"dmabuf={final_status['dmabuf']}")
print(f"explicit_sync={final_status['explicit_sync']}")
print(f"dmabuf_accepted={final_status['dmabuf_accepted']}")
print(f"dmabuf_rejected={final_status['dmabuf_rejected']}")
print(f"dmabuf_explicit_sync_blockers={final_status['dmabuf_explicit_sync_blockers']}")
print(f"dmabuf_blocker_failures={final_status['dmabuf_blocker_failures']}")
PY

ctl quit >/dev/null || true
wait "$PID" || true
trap - EXIT
cleanup
