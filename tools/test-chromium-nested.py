"""Real Chromium launch, pixels and input through Brood's offscreen test CLI."""
import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import subprocess
import tempfile
import threading
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--package", type=Path, required=True)
parser.add_argument("--out-dir", type=Path, required=True)
args = parser.parse_args()
if not os.environ.get("DBUS_SESSION_BUS_ADDRESS"):
    os.execvp("dbus-run-session", ["dbus-run-session", "--", "python3", __file__, *os.sys.argv[1:]])
out = args.out_dir.resolve()
out.mkdir(parents=True, exist_ok=True)
runtime = Path(tempfile.mkdtemp(prefix="brood-chromium-runtime-"))
present = Path(tempfile.mkdtemp(prefix="brood-chromium-present-"))
binary = args.package.resolve() / "bin/brood"
client = args.package.resolve() / "bin/brood-client"
config = args.package.resolve() / "brood.toml"
env = os.environ.copy()
env.pop("brood_session", None)
env.update(XDG_RUNTIME_DIR=str(runtime), RUST_LOG="broodd=info,smithay=warn")
destination = f"org.brood.ChromiumTest.p{os.getpid()}"
processes = []
files = []
events = []
size = os.environ.get("BROOD_CHROMIUM_SIZE", "1920x1080")
width, height = map(int, size.split("x"))
page = b"""<!doctype html><title>Brood Chromium proof</title>
<style>html,body{margin:0;background:#123456;color:white;font:24px sans-serif}
main{padding:24px}input,button{font:24px sans-serif}#space{height:1600px}</style>
<main><h1>Chromium in Brood</h1><input id="entry" placeholder="Type here">
<button id="button">Verify click</button><p id="result">Ready</p>
<div id="space">Scroll this page</div><h2>End of page</h2></main>
<script>
const entry=document.getElementById('entry'), button=document.getElementById('button');
function report(type) {
  navigator.sendBeacon('/events',JSON.stringify({type,text:entry.value,scrollY,
    viewport:[innerWidth,innerHeight],entry:entry.getBoundingClientRect().toJSON(),
    button:button.getBoundingClientRect().toJSON()}));
}
entry.addEventListener('input',()=>report('input'));
button.addEventListener('click',()=>{document.getElementById('result').textContent='Click verified';report('click');});
addEventListener('scroll',()=>report('scroll'));
addEventListener('resize',()=>report('resize'));
report('ready');
</script>"""


class Fixture(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.end_headers()
        self.wfile.write(page)

    def do_POST(self):
        event = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        events.append(event)
        (out / "page-events.json").write_text(json.dumps(events, indent=2))
        self.send_response(204)
        self.end_headers()


http = ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
threading.Thread(target=http.serve_forever, daemon=True).start()


def run(argv, extra=None, binary_output=False):
    return subprocess.run([str(arg) for arg in argv], check=True, text=not binary_output,
                          capture_output=True, timeout=15,
                          env={**env, "RUST_LOG": "warn", **(extra or {})}).stdout


def start(argv, log, extra=None):
    handle = (out / log).open("w")
    files.append(handle)
    process = subprocess.Popen([str(arg) for arg in argv], env={**env, **(extra or {})},
                               stdout=handle, stderr=subprocess.STDOUT, start_new_session=True)
    processes.append(process)
    return process


def ctl(*argv):
    return run([binary, "ctl", "--dbus-destination", destination, *argv])


def wait_for(probe, description, timeout=30):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            last = probe()
            if last:
                return last
        except (subprocess.CalledProcessError, json.JSONDecodeError, FileNotFoundError):
            pass
        exited = [(p.pid, p.returncode) for p in processes if p.poll() is not None]
        if exited:
            raise AssertionError(f"process exited during {description}: {exited}; evidence={out}")
        time.sleep(0.1)
    raise AssertionError(f"timed out: {description}; last={last!r}; evidence={out}")


def capture(name):
    run(["grim", out / name], present_env)
    pixels = run(["magick", out / name, "-depth", "8", "rgb:-"], binary_output=True)
    # Actual page pixels are required: a catalogue entry or blank pane cannot pass.
    matched = [i // 3 for i in range(0, len(pixels), 3) if pixels[i:i + 3] == b"\x12\x34\x56"]
    if len(matched) < 10000:
        return None
    xs = [pixel % width for pixel in matched]
    ys = [pixel // width for pixel in matched]
    return min(xs), min(ys), max(xs) + 1, max(ys) + 1


def pointer(kind, x, y, *extra):
    run([out / "pointer", kind, round(x), round(y), *extra, width, height], present_env)


try:
    chromium = os.environ.get("BROOD_CHROMIUM") or shutil.which("chromium")
    assert chromium, "Chromium is required on PATH (or set BROOD_CHROMIUM)"
    run(["cc", "-std=c11", "-Wall", "-Wextra", "-Werror",
         Path(__file__).with_name("virtual-pointer-input.c"), "-o", out / "pointer",
         *shlex.split(run(["pkg-config", "--cflags", "--libs", "wayland-client"]))])
    run(["cc", "-std=c11", "-Wall", "-Wextra", "-Werror",
         Path(__file__).with_name("virtual-keyboard-key.c"), "-o", out / "keyboard",
         *shlex.split(run(["pkg-config", "--cflags", "--libs", "wayland-client", "xkbcommon"]))])
    start([binary, "run", "--no-session-runner", "--headless", "--config", config,
           "--dbus-destination", destination], "server.log")
    status = wait_for(lambda: json.loads(ctl("status-json")), "backend readiness")
    (out / "sway.conf").write_text(
        f"xwayland disable\noutput * mode {size}\nseat seat0 fallback true\ndefault_border none\n")
    start(["sway", "--unsupported-gpu", "--config", out / "sway.conf"], "sway.log", {
        "XDG_RUNTIME_DIR": str(present), "WLR_BACKENDS": "headless",
        "WLR_HEADLESS_OUTPUTS": "1", "WLR_LIBINPUT_NO_DEVICES": "1"})
    socket = wait_for(lambda: next((p for p in present.glob("wayland-*") if p.is_socket()), None),
                      "offscreen socket")
    present_env = {"XDG_RUNTIME_DIR": str(present), "WAYLAND_DISPLAY": socket.name}
    start([client, "--config", config, "--dbus-destination", destination], "client.log", {
        **present_env, "BROOD_FRAME_TRANSPORT": "shared-memory", "WGPU_BACKEND": "vulkan",
        "RUST_LOG": "brood_client=info,wgpu_core=warn,wgpu_hal=warn"})
    wait_for(lambda: "setup layout quiescent" in (out / "client.log").read_text(),
             "initial presentation layout", timeout=120)
    run(["grim", out / "before.png"], present_env)
    browser = start([chromium, "--user-data-dir=" + str(runtime / "profile"),
                     "--no-first-run", "--no-default-browser-check", "--disable-extensions",
                     "--disable-background-networking", "--ozone-platform=wayland",
                     f"http://127.0.0.1:{http.server_port}/"],
                    "chromium.log", {"WAYLAND_DISPLAY": status["wayland_display"], "WAYLAND_DEBUG": "1"})
    wait_for(lambda: events, "Chromium page load")
    bounds = wait_for(lambda: capture("presentation.png"), "visible Chromium content")
    before = json.loads(ctl("status-json"))
    (out / "status-before-input.json").write_text(json.dumps(before, indent=2))
    (out / "windows.json").write_text(ctl("computer-use", "windows", "1"))
    ctl("debug", "dump", str(out / "dump"), "surfaces")

    geometry = events[-1]
    sx = (bounds[2] - bounds[0]) / geometry["viewport"][0]
    sy = (bounds[3] - bounds[1]) / geometry["viewport"][1]

    def center(rect):
        return (bounds[0] + (rect["x"] + rect["width"] / 2) * sx,
                bounds[1] + (rect["y"] + rect["height"] / 2) * sy)

    pointer("click", *center(geometry["entry"]))
    typed = "brood chromium input verified"
    run([out / "keyboard", "--text", typed], present_env)
    wait_for(lambda: any(e["type"] == "input" and e["text"] == typed for e in events),
             "real keyboard input received by Chromium")
    pointer("click", *center(geometry["button"]))
    wait_for(lambda: any(e["type"] == "click" for e in events), "real click received by Chromium")
    assert capture("input.png")
    pointer("scroll", bounds[0] + 100, bounds[1] + 250, 30, 10)
    wait_for(lambda: any(e["type"] == "scroll" and e["scrollY"] > 0 for e in events),
             "real scroll received by Chromium")
    assert capture("scrolled.png")
    after = json.loads(ctl("status-json"))
    (out / "status-after-input.json").write_text(json.dumps(after, indent=2))
    counters = ["keyboard_events", "pointer_button_events", "pointer_axis_events", "commits", "frames"]
    deltas = {key: after[key] - before[key] for key in counters}
    assert all(delta > 0 for delta in deltas.values()), deltas
    assert after["dmabuf_rejected"] == 0 and after["wayland_dispatch_errors"] == 0
    client_log = (out / "client.log").read_text()
    assert "completed synchronized layout" in client_log
    if width >= 1920:
        assert "managed resize snapshot queued" in (out / "server.log").read_text()
    else:
        assert "result=Rejected { reason: SizeUnsatisfied }" in client_log
        assert "recovered_managed={1}" in client_log
    browser.terminate()
    browser.wait(timeout=10)
    # Teardown checks cannot use wait_for, which rejects exited clients.
    deadline = time.monotonic() + 10
    while json.loads(ctl("status-json"))["total_windows"] != 0:
        assert time.monotonic() < deadline, "Chromium window survived process exit"
        time.sleep(0.1)
    (out / "status-after-close.json").write_text(ctl("status-json"))
    report = {"size": size, "visible_browser_bounds": bounds, "input_deltas": deltas,
              "page_events": len(events), "typed_text": typed, "passed": True}
    (out / "report.json").write_text(json.dumps(report, indent=2))
    print(f"Chromium launch and input passed: {out}")
finally:
    for process in reversed(processes):
        # Each process owns a private test process group, including its helpers.
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=5)
    http.shutdown()
    http.server_close()
    for handle in files:
        handle.close()
    shutil.rmtree(runtime)
    shutil.rmtree(present)
