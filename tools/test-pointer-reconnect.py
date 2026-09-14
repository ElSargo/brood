"""Real shell clicks across session changes and frontend replacement.

Launched by `brood test pointer-reconnect` inside the isolated shell harness.
Each held click leaves time for asynchronous layout replies to arrive between
press and release; success requires the intended plugin action exactly once.
"""

import argparse
import json
import os
from pathlib import Path
import re
import select
import signal
import subprocess
import time


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--out", type=Path, required=True)
parser.add_argument("--binary", type=Path, required=True)
parser.add_argument("--config", type=Path, required=True)
parser.add_argument("--destination", required=True)
parser.add_argument("--dbus-address", required=True)
args = parser.parse_args()
out = args.out.resolve()
log = out / "client.log"
replacements = []
handles = []
checks = []
pointer = None


def run(argv):
    return subprocess.run([str(arg) for arg in argv], check=True,
                          capture_output=True, text=True, timeout=15).stdout


def ctl(*argv):
    return run([args.binary, "ctl", "--dbus-destination", args.destination,
                "--dbus-address", args.dbus_address, *argv])


def contents():
    return log.read_text(errors="replace")


def wait_for(probe, description, timeout=30):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = probe()
        if result:
            return result
        assert all(p.poll() is None for p in replacements), "replacement frontend exited"
        time.sleep(0.05)
    raise AssertionError(f"timed out: {description}; evidence={out}")


def actor_pane(actor):
    matches = re.findall(rf"started presentation WASM actor id={actor} .*? pane=(\d+)", contents())
    return int(matches[-1]) if matches else None


def settled():
    text = contents()
    starts = [m.start() for m in re.finditer(r"^accepted synchronized layout ", text, re.M)]
    ends = [m.start() for m in re.finditer(r"^(?:completed|degraded|aborted) synchronized layout ", text, re.M)]
    return not starts or (ends and ends[-1] > starts[-1])


def capture(name):
    run(["grim", out / f"{name}.png"])


def ready():
    wait_for(lambda: actor_pane("session-switcher") and actor_pane("picker")
             and re.search(r"^(?:completed|degraded) synchronized layout actor=session-switcher ",
                           contents(), re.M)
             and "setup layout quiescent" in contents() and settled(), "shell layout", 120)
    # Let the final ready composition reach the offscreen output.
    time.sleep(0.25)


def pointer_command(command):
    pointer.stdin.write(command + "\n")
    pointer.stdin.flush()
    assert select.select([pointer.stdout], [], [], 5)[0], "pointer acknowledgement timed out"
    reply = pointer.stdout.readline()
    assert reply.startswith("ack "), reply


def key(*argv):
    run([out / "virtual-keyboard-key", *argv])


def held_click(actor, x, y, name):
    pane = actor_pane(actor)
    start = len(contents())
    pointer_command(f"motion {round(x)} {round(y)}")
    pointer_command("press")
    try:
        wait_for(lambda: f"pane: {pane}," in contents()[start:]
                 and "pressed=true" in contents()[start:], f"{name} press delivery")
        time.sleep(0.35)
        capture(name + "-held")
        interval = contents()[start:]
        assert f"focused away from plugin pane={pane}" not in interval, \
            f"{name}: layout stole {actor} focus during the click"
    finally:
        pointer_command("release")
    return start


def picker_visibility_since(start, visible):
    return re.search(r'"type":"picker-pane","pane":\d+,"visible":'
                     + str(visible).lower(), contents()[start:])


def verify_new_button(label):
    start = held_click("session-switcher", 1244, 26, label + "-new")
    wait_for(lambda: picker_visibility_since(start, True), "New button opened picker")
    wait_for(settled, "creation picker layout")
    time.sleep(0.2)
    capture(label + "-new-picker")
    opened = contents()[start:]
    assert len(re.findall(r'"type":"picker-created"', opened)) == 1, "duplicate New activation"
    closing = len(contents())
    key("--plain", 1)
    wait_for(lambda: picker_visibility_since(closing, False), "creation picker cancelled")
    wait_for(settled, "picker close layout")
    checks.append({"stage": label, "action": "new-button", "passed": True})


def wait_session_rendered(start, session):
    completion = wait_for(
        lambda: re.search(r"presentation session command completed actor=session-switcher .*"
                          rf"operation=Activate success=true session=Some\({session}\)",
                          contents()[start:]),
        "session command completed in the client")
    completed_at = start + completion.end()
    wait_for(lambda: "facet native pair committed" in contents()[completed_at:]
             and re.search(r"facet native pair committed .*actor=session-switcher ",
                           contents()[completed_at:]) and settled(),
             "session switch completion rendered")
    # A backend catalogue update can enqueue its workspace layout just after
    # command completion. Allow that publication to reach layout admission.
    time.sleep(0.25)
    wait_for(settled, "post-switch workspace layout")


def verify_session_picker(label, session_name):
    start = len(contents())
    key(25)  # Ctrl+Alt+P, the session picker.
    wait_for(lambda: picker_visibility_since(start, True), "session picker opened")
    wait_for(settled, "session picker layout")
    time.sleep(0.2)
    requests = re.findall(r"sender=session-switcher recipient=picker body=(.*)", contents()[start:])
    request = next(json.loads(value) for value in requests
                   if json.loads(value).get("title") == "Switch session")
    row, item = next((index, item) for index, item in enumerate(request["items"])
                     if item["value"]["label"] == session_name)
    session = item["value"]["value"]["session"]
    capture(label + "-session-picker")
    clicked = held_click("picker", 500, 120 + 52 + 15 + row * 30, label + "-select")
    wait_for(lambda: picker_visibility_since(clicked, False), "picker row activated")
    wait_for(lambda: json.loads(ctl("status-json"))["active_surface_session"] == session,
             f"selected session {session_name}")
    wait_session_rendered(clicked, session)
    selected = contents()[clicked:]
    assert len(re.findall(r'"type":"picker-selected"', selected)) == 1, "duplicate picker activation"
    checks.append({"stage": label, "action": "session-picker", "session": session, "passed": True})


def verify_sidebar_session(label, session_name):
    row = ("Beta", "Gamma").index(session_name)
    # The fixture has one expanded project row followed by Beta and Gamma.
    start = held_click("session-switcher", 1050, 52 + 34 + 15 + row * 30, label + "-sidebar")
    wait_for(lambda: json.loads(ctl("status-json"))["active_surface_session_name"] == session_name,
             f"sidebar selected {session_name}")
    wait_session_rendered(start, json.loads(ctl("status-json"))["active_surface_session"])
    checks.append({"stage": label, "action": "sidebar-session", "session": session_name, "passed": True})


try:
    ready()
    pointer = subprocess.Popen([out / "virtual-pointer-input", "sequence", "1280", "720"],
                               stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    assert select.select([pointer.stdout], [], [], 5)[0], "pointer startup timed out"
    assert pointer.stdout.readline().startswith("ready ")
    before = json.loads(ctl("status-json"))
    (out / "pointer-server-before.json").write_text(json.dumps(before, indent=2))
    for generation in range(3):
        label = f"generation-{generation}"
        ready()
        capture(label + "-ready")
        for repeat in range(2):
            stage = f"{label}-round-{repeat}"
            verify_new_button(stage)
            verify_sidebar_session(stage, "Beta")
            verify_session_picker(stage, "Gamma")
        (out / f"{label}-server.json").write_text(ctl("status-json"))
        if generation == 2:
            break
        ctl("close-frontend")
        for process in replacements:
            assert process.wait(timeout=15) == 0
        replacements.clear()
        wait_for(lambda: json.loads(ctl("status-json"))["presentation"] == "Detached",
                 "frontend detached")
        log = out / f"client-reconnected-{generation + 1}.log"
        handle = log.open("w")
        handles.append(handle)
        process = subprocess.Popen([args.binary, "client", "--config", args.config,
                                    "--dbus-destination", args.destination,
                                    "--dbus-address", args.dbus_address],
                                   stdout=handle, stderr=subprocess.STDOUT, start_new_session=True)
        replacements.append(process)
    capture("pointer-final")
    after = json.loads(ctl("status-json"))
    assert after["total_windows"] == before["total_windows"] == 2
    assert after["smithay_input_drops"] == before["smithay_input_drops"] == 0
    client_logs = [out / "client.log", *(out / f"client-reconnected-{i}.log" for i in (1, 2))]
    client_metrics = {}
    for path in client_logs:
        text = path.read_text(errors="replace")
        client_metrics[path.name] = {
            "plugin_button_presses": len(re.findall(r"presentation pointer button target=Plugin .* pressed=true", text)),
            "plugin_button_releases": len(re.findall(r"presentation pointer button target=Plugin .* pressed=false", text)),
            "facet_inputs_delivered": len(re.findall(r"facet input delivered actor=(?:picker|session-switcher) ", text)),
        }
        assert client_metrics[path.name]["plugin_button_presses"] == 6
        assert client_metrics[path.name]["plugin_button_releases"] == 6
    (out / "pointer-report.json").write_text(json.dumps({
        "passed": True, "presentation_reconnects": 2, "checks": checks,
        "held_clicks": len(checks), "session_switches": 12,
        "client_metrics": client_metrics, "server_input_drops": after["smithay_input_drops"],
    }, indent=2))
    print(f"Plugin pointer reconnect proof passed: {out}")
finally:
    if pointer is not None:
        pointer.terminate()
        pointer.wait(timeout=5)
    for process in replacements:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=5)
    for handle in handles:
        handle.close()
