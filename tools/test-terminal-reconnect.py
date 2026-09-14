"""Real PTYs and keyboard input across complete presentation replacements.

Run through `brood test terminal-reconnect`, which owns the isolated compositor.
"""

import argparse
import base64
import json
import os
from pathlib import Path
import re
import signal
import shlex
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
process = None
handles = []
evidence = []


def run(argv):
    result = subprocess.run([str(arg) for arg in argv], capture_output=True, text=True, timeout=20)
    assert result.returncode == 0, f"command failed: {argv}: {result.stderr}"
    return result.stdout


def ctl(*argv):
    return run([args.binary, "ctl", "--dbus-destination", args.destination,
                "--dbus-address", args.dbus_address, *argv])


def wait_for(probe, description, timeout=60):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = probe()
        if value:
            return value
        assert process is None or process.poll() is None, "replacement frontend exited"
        time.sleep(0.05)
    raise AssertionError(f"timed out: {description}; evidence={out}")


def contents():
    return log.read_text(errors="replace")


def terminals():
    return json.loads(ctl("terminal", "list-json"))


def key(code):
    run([out / "virtual-keyboard-key", code])


def ready(empty=False):
    def shell_settled():
        text = contents()
        assert "visual facet failed" not in text, "presentation facet failed during startup"
        claimed = text.find("granted claimed root layout lease actor=session-switcher")
        if empty:
            return claimed >= 0 and text.rfind("setup layout quiescent") > claimed
        # Startup may commit an ordinary tree or a synchronized transaction.
        # Require the shell, workspace delegation and a settled presentation,
        # without depending on which transaction wins the first-frame race.
        delegated = text.find("granted rest layout lease parent=1 child=2 actor=workspace", claimed)
        return claimed >= 0 and delegated > claimed and text.rfind("setup layout quiescent") > delegated
    wait_for(shell_settled, "settled presentation shell", 120)


def activate(session):
    ctl("surface-session", "switch", session)
    wait_for(lambda: json.loads(ctl("status-json"))["active_surface_session"] == session,
             "active session")
    time.sleep(0.4)


def focus(session, ordinal):
    activate(session)
    start = len(contents())
    ctl("terminal", "focus", session, ordinal)
    reply = wait_for(lambda: re.search(r'sender=brood.client-init recipient=(terminal-ui[^ ]*) body=.*"kind":"focus"',
                                      contents()[start:]), "terminal focus request")
    actor = reply.group(1)
    pane = re.findall(rf"started presentation WASM actor id={actor} .*? pane=(\d+)", contents())[-1]
    wait_for(lambda: re.search(rf"focused local plugin pane={pane}\b", contents()[start:])
             or re.search(rf"pane focus pane={pane} focused=Some\({pane}\).*deferred=false", contents()[start:]),
             "terminal focus committed")
    time.sleep(0.2)


def shell_probe(session, ordinal, generation):
    focus(session, ordinal)
    proof = out / f"shell-{session}-{ordinal}-{generation}.txt"
    # The variable and PID must survive frontend replacement, and input must
    # actually traverse the new client keyboard path to the old shell.
    line = f'BROOD_REATTACH_TOKEN=${{BROOD_REATTACH_TOKEN:-token-{session}-{ordinal}}}; printf "%s %s\\n" "$BROOD_REATTACH_TOKEN" "$$" | tee {shlex.quote(str(proof))}'
    # Source a short script in the real shell. The retained variable and $$
    # belong to that shell, while short keystrokes avoid coupling this lifecycle
    # proof to high-volume text-input queue stress.
    (out / "project" / ".r").write_text(line + "\n")
    run([out / "virtual-keyboard-key", "--text", ". ./.r"])
    run([out / "virtual-keyboard-key", "--plain", 28])
    wait_for(lambda: proof.exists() and proof.stat().st_size, "shell keyboard response")
    value = proof.read_text().strip()
    if generation:
        assert value == (out / f"shell-{session}-{ordinal}-0.txt").read_text().strip(), \
            "shell process or state was replaced"
    return {"session": session, "ordinal": ordinal, "generation": generation, "shell": value}


def capture(name):
    time.sleep(0.3)
    run(["grim", out / f"{name}.png"])


def assert_present(expected):
    # Assert actual presentation actors and rendered frames, not just retained
    # server PTYs. Both sessions' default UIs plus every dynamic UI must exist.
    wait_for(lambda: len(re.findall(r"started presentation WASM actor id=terminal-ui[^ ]* ",
                                    contents())) == len(expected), "all terminal UI actors")
    actors = re.findall(r"started presentation WASM actor id=(terminal-ui[^ ]*) ", contents())
    assert len(actors) == len(set(actors)), "duplicate presentation actor starts"
    for terminal in expected:
        if terminal["ordinal"] > 1:
            actor = f'terminal-ui-b1-s{terminal["session"]}-{terminal["ordinal"]}'
            assert actor in actors, f"missing {actor}"
    return actors


def detach():
    global process
    ctl("close-frontend")
    if process is not None:
        assert process.wait(timeout=15) == 0
        process = None
    wait_for(lambda: json.loads(ctl("status-json"))["presentation"] == "Detached", "detached")


def reconnect(generation):
    global log, process
    log = out / f"terminal-client-reconnected-{generation}.log"
    handle = log.open("w")
    handles.append(handle)
    process = subprocess.Popen([args.binary, "client", "--config", args.config,
                                "--dbus-destination", args.destination,
                                "--dbus-address", args.dbus_address],
                               stdout=handle, stderr=subprocess.STDOUT, start_new_session=True)
    ready()


try:
    ready(empty=True)
    assert not terminals(), "fixture must start with no sessions or terminals"
    for count, name in enumerate(("Beta", "Gamma"), 1):
        ctl("surface-session", "new", name)
        wait_for(lambda: len(terminals()) == count, "new session's default terminal")
        assert_present(terminals())
    ready()
    sessions = sorted({terminal["session"] for terminal in terminals()})
    assert len(sessions) == 2, sessions
    for session in sessions:
        activate(session)
        for expected_count in (2, 3):
            key(20)  # Ctrl+Alt+T, through the real presentation.
            wait_for(lambda: len([t for t in terminals() if t["session"] == session]) == expected_count,
                     "shortcut-created terminal")
            time.sleep(0.4)
    expected = terminals()
    assert len(expected) == 6
    assert_present(expected)
    for terminal in expected:
        evidence.append(shell_probe(terminal["session"], terminal["ordinal"], 0))
    capture("terminal-before-disconnect")
    baseline = json.loads(ctl("status-json"))
    (out / "terminal-server-before.json").write_text(json.dumps(baseline, indent=2))

    for generation in (1, 2):
        detach()
        detached = json.loads(ctl("status-json"))
        assert terminals() == expected, "disconnect removed terminal resources"
        assert detached["hosted_runtime_io_children"] == baseline["hosted_runtime_io_children"]
        reconnect(generation)
        assert_present(expected)
        for terminal in expected:
            evidence.append(shell_probe(terminal["session"], terminal["ordinal"], generation))
        capture(f"terminal-reconnected-{generation}")
        (out / f"terminal-server-{generation}.json").write_text(ctl("status-json"))

    # Close with the real shortcut, then create while no client can receive
    # the creation event. The next client must get exactly the retained set.
    victim = expected[-1]
    focus(victim["session"], victim["ordinal"])
    key(16)  # Ctrl+Alt+Q
    wait_for(lambda: victim not in terminals(), "explicit pane close stopped its shell")
    expected.remove(victim)
    detach()
    ctl("terminal", "new", sessions[0], base64.b64encode(b"printf 'CREATED WHILE DETACHED\n'").decode())
    wait_for(lambda: len(terminals()) == 6, "terminal created while detached")
    expected = terminals()
    reconnect(3)
    actors = assert_present(expected)
    assert victim not in terminals(), "closed terminal resurrected"
    for terminal in expected:
        previous = out / f'shell-{terminal["session"]}-{terminal["ordinal"]}-0.txt'
        evidence.append(shell_probe(terminal["session"], terminal["ordinal"], 3 if previous.exists() else 0))
    capture("terminal-detached-create-restored")
    final = json.loads(ctl("status-json"))
    assert final["hosted_runtime_io_children"] == baseline["hosted_runtime_io_children"]
    assert final["smithay_input_drops"] == 0
    (out / "terminal-server-after.json").write_text(json.dumps(final, indent=2))
    client_metrics = {}
    for path in [out / "client.log", *(out / f"terminal-client-reconnected-{g}.log" for g in (1, 2, 3))]:
        text = path.read_text(errors="replace")
        rendered = {
            actor: len(re.findall(rf"facet native pair committed .*actor={re.escape(actor)} ", text))
            for actor in re.findall(r"started presentation WASM actor id=(terminal-ui[^ ]*) ", text)
        }
        client_metrics[path.name] = {
            "terminal_ui_starts": len(re.findall(r"started presentation WASM actor id=terminal-ui[^ ]* ", text)),
            "terminal_frames": len(re.findall(r"facet native pair committed .*actor=terminal-ui[^ ]* ", text)),
            "keyboard_events": len(re.findall(r"plugin keyboard input ", text)),
            "text_events": len(re.findall(r"plugin text input ", text)),
            "frames_by_actor": rendered,
        }
        assert rendered and all(rendered.values()), f"unrendered terminal pane: {path}: {rendered}"
        assert client_metrics[path.name]["text_events"] > 0
    (out / "terminal-reconnect-report.json").write_text(json.dumps({
        "passed": True, "presentation_reconnects": 3, "sessions": sessions,
        "terminal_actors": expected, "presentation_actors": actors,
        "shell_probes": evidence, "explicit_close": victim, "client_metrics": client_metrics,
        "created_while_detached": True, "server_input_drops": final["smithay_input_drops"],
    }, indent=2))
    print(f"Terminal reconnect proof passed: {out}")
finally:
    if process is not None and process.poll() is None:
        os.killpg(process.pid, signal.SIGTERM)
        process.wait(timeout=15)
    for handle in handles:
        handle.close()
