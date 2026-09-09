#!/usr/bin/env python3
"""Real continuous divider drags in the offscreen production brood-client.

Called by test-session-switcher-plugin-nested.sh with BROOD_PLUGIN_DRAG_TEST=1.
No wait for Brood promotion is inserted between pointer motions. All cases run
before the aggregate verdict, retaining screenshots and failure evidence.
"""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import time
import threading

parser = argparse.ArgumentParser(__doc__)
parser.add_argument("--out", type=Path, required=True)
parser.add_argument("--client-pid", type=int, required=True)
args = parser.parse_args()
out = args.out
log = out / "client.log"
results = []
frames = out / "drag-frames"
frames.mkdir(exist_ok=True)


def ctl_status_json():
    command = [
        os.environ["BROOD_DRAG_CTL_BIN"],
        "ctl",
        "--dbus-destination",
        os.environ["BROOD_DRAG_DBUS_DESTINATION"],
        "--dbus-address",
        os.environ["BROOD_DRAG_DBUS_ADDRESS"],
        "status-json",
    ]
    completed = subprocess.run(command, check=True, capture_output=True, text=True)
    return json.loads(completed.stdout)


def terminal_actor_ids(status):
    return sorted(
        actor[0]
        for actor in status.get("plugin_actors", [])
        if isinstance(actor, list)
        and actor
        and "terminal-session" in str(actor[0])
    )


def write_status(name, status):
    path = out / f"many-actors-{name}-status.json"
    path.write_text(json.dumps(status, indent=2) + "\n")
    return str(path)


def wait_for_terminal_actor_count(minimum=9, seconds=30):
    until = time.monotonic() + seconds
    latest = None
    while time.monotonic() < until:
        latest = ctl_status_json()
        if len(terminal_actor_ids(latest)) >= minimum:
            return latest
        time.sleep(.1)
    raise RuntimeError(
        f"timed out waiting for {minimum} terminal-session actors; "
        f"found {terminal_actor_ids(latest or {})}"
    )


def idle_callback_check(name, before, seconds=1.0, maximum_per_second=240):
    time.sleep(seconds)
    after = ctl_status_json()
    before_count = int(before.get("smithay_callback_count", 0))
    after_count = int(after.get("smithay_callback_count", 0))
    delta = max(0, after_count - before_count)
    result = {
        "seconds": seconds,
        "before": before_count,
        "after": after_count,
        "delta": delta,
        "rate_per_second": delta / seconds,
        "maximum_per_second": maximum_per_second,
        "before_file": write_status(f"{name}-before", before),
        "after_file": write_status(f"{name}-after", after),
    }
    if result["rate_per_second"] > maximum_per_second:
        raise RuntimeError(
            f"smithay callback idle rate {result['rate_per_second']:.1f}/s "
            f"exceeds {maximum_per_second}/s"
        )
    return result


def text():
    return log.read_text(errors="replace")


def wait_for(predicate, seconds=12):
    until = time.monotonic() + seconds
    while time.monotonic() < until:
        os.kill(args.client_pid, 0)
        if predicate():
            return
        time.sleep(.05)
    raise RuntimeError("timed out waiting for visible plugin")


def key(code, modifiers=None):
    command = [str(out / "virtual-keyboard-key")]
    if modifiers:
        command.append(modifiers)
    subprocess.run(command + [str(code)], check=True)
    time.sleep(.5)


def actor_panes():
    return dict(re.findall(r"started presentation WASM actor id=(\S+) .*? pane=(\d+) ", text()))


def projection():
    found = re.findall(r"render-content-projection .*?details=\[([^\]]*)\]", text())
    if not found:
        return {}
    return {v[0]: [int(float(x)) for x in v[1:]] for item in found[-1].split(",")
            if len(v := item.split(":")) == 6}


def divider(kind="tile"):
    if kind == "window":
        return int(re.findall(r"resize-present .*?composition_size=(\d+)x", text())[-1])
    if kind == "shell":
        panes = actor_panes()
        sizes = projection()
        sidebar = sizes[panes["session-switcher"]][3]
        inbox = sizes.get(panes.get("inbox"), [0, 0, 0, 0, 0])[3]
        return 1280 - sidebar - inbox
    if kind == "inbox":
        pane = actor_panes().get("inbox")
        return 1280 - projection()[pane][3]
    found = re.findall(r"resize-present .*?divider_x=(\d+)", text())
    if not found:
        raise RuntimeError("missing presented divider trace")
    return int(found[-1])


def capture(name):
    path = frames / (name + ".png")
    subprocess.run(["grim", "-l", "0", str(path)], check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    return str(path)


def visible_actor(prefix):
    panes = actor_panes()
    rendered = projection()
    return any(name.startswith(prefix) and pane in rendered for name, pane in panes.items())


def drag_case(name, expected, delta=-180, boundary="tile", fixed=()):
    wait_for(lambda: all(visible_actor(prefix) for prefix in expected))
    time.sleep(.3)
    panes = actor_panes()
    before = projection()
    start = divider(boundary)
    target = start + delta
    before_file = capture(name + "-before")
    offset = len(text())
    started = time.monotonic()
    modifier = None
    if boundary == "window":
        height = int(re.findall(r"resize-present .*?composition_size=\d+x(\d+)", text())[-1])
        modifier = subprocess.Popen(["wtype", "-M", "logo", "-s", "2000", "-m", "logo"])
        time.sleep(.1)
        process = subprocess.Popen([str(out / "virtual-pointer-input"), "sequence", "1280", "720"],
                                   stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, text=True)
        def gesture():
            x, y = start - 30, height - 30
            scale = float(os.environ.get("BROOD_PLUGIN_DRAG_OUTPUT_SCALE", "1"))
            # Respect the client's logical minimum height at fractional scale;
            # restoring must also keep pointer coordinates inside the output.
            dy = -min(240, max(0, height - int(320 * scale))) if delta < 0 else min(240, 720 - height)
            process.stdin.write(f"motion {x} {y}\npress-right\n")
            process.stdin.flush()
            time.sleep(.02)
            for step in range(1, 91):
                process.stdin.write(f"motion {x + delta * step // 90} {y + dy * step // 90}\n")
                process.stdin.flush()
                time.sleep(.016)
            process.stdin.write("release-right\nquit\n")
            process.stdin.flush()
            process.stdin.close()
        threading.Thread(target=gesture).start()
    else:
        process = subprocess.Popen([str(out / "virtual-pointer-input"), "drag",
                                str(start), "330", str(target), "330",
                                "90", "1280", "720"])
    samples = []
    while process.poll() is None:
        sample_started = time.monotonic()
        screenshot = capture(f"{name}-{len(samples):03d}")
        samples.append({"seconds": time.monotonic() - started,
                        "divider": divider(boundary), "projection": projection(),
                        "file": screenshot})
        time.sleep(max(0, .08 - (time.monotonic() - sample_started)))
    process.wait()
    released = time.monotonic()
    # Keep the failure evidence even if final convergence is late or absent.
    while time.monotonic() - released < 3:
        if abs(divider(boundary) - target) <= 3:
            break
        time.sleep(.02)
    final_delay = time.monotonic() - released
    time.sleep(.25)
    final = divider(boundary)
    after = projection()
    after_file = capture(name + "-after")
    if modifier:
        modifier.wait()
    trace = text()[offset:]
    unique = sorted(set(s["divider"] for s in samples))
    affected = {}
    for actor, pane in panes.items():
        if pane not in before or pane not in after:
            continue
        sizes = {tuple(before[pane][3:]), tuple(after[pane][3:])}
        sizes.update(tuple(s["projection"][pane][3:])
                     for s in samples if pane in s["projection"])
        affected[actor] = {"pane": pane, "target_sizes": sorted(sizes),
                           "resized": len(sizes) > 1}
    errors = []
    if "pointer-policy capture cancelled" in trace:
        errors.append("Brood cancelled pointer capture before release")
    if process.returncode:
        errors.append("pointer gesture failed")
    if abs(final - target) > 3:
        errors.append(f"final divider {final}, requested {target}")
    # 90 independent motions over ~1.5 s must not collapse into a few jumps.
    if len(unique) < 10:
        errors.append(f"only {len(unique)} visible divider positions in {len(samples)} captures")
    if final_delay > .15:
        errors.append(f"post-gesture convergence took {final_delay:.3f}s")
    if any(v[1:3] != v[3:5] for v in after.values()):
        errors.append("final content extent differs from allocation")
    for prefix in expected:
        matching = [v for a, v in affected.items() if a.startswith(prefix)]
        if not matching:
            errors.append(f"{prefix} missing from final visible composition")
        elif prefix not in fixed and not any(v["resized"] for v in matching):
            errors.append(f"{prefix} never changed its visible extent")
    result = {"case": name, "expected_actors": expected, "fixed_actors": fixed, "boundary": boundary,
              "start": start, "target": target, "final": final,
              "duration_seconds": released - started,
              "post_gesture_convergence_seconds": final_delay,
              "visible_positions": unique, "samples": samples, "actors": affected,
              "synchronized_submissions": len(re.findall(r"^accepted synchronized layout ", trace, re.MULTILINE)),
              "synchronized_completions": len(re.findall(r"^completed synchronized layout ", trace, re.MULTILINE)),
              "synchronized_supersessions": trace.count("synchronized interaction superseded"),
              "facet_starts": trace.count("facet render started "),
              "capture_cancellations": re.findall(r"pointer-policy capture cancelled[^\n]*", trace),
              "layout_rejections": re.findall(r"rejected synchronized layout[^\n]*", trace),
              "pane_ready_records": re.findall(r"resize-host pane-ready[^\n]*", trace),
              "before": before_file, "after": after_file, "log_start_character": offset,
              "log_end_character": offset + len(trace), "errors": errors}
    results.append(result)
    (out / "drag-results.json").write_text(json.dumps(results, indent=2))
    print(f"drag_case={name} positions={len(unique)} final={final} errors={errors}", flush=True)
    return result


def many_actors_mode():
    close_terminals = os.environ.get("BROOD_PLUGIN_DRAG_CLOSE_TERMINALS") == "1"
    initial_status = ctl_status_json()
    write_status("initial", initial_status)
    for _ in range(9):
        key(20)
    populated_status = wait_for_terminal_actor_count()
    write_status("populated", populated_status)
    terminal_ids = terminal_actor_ids(populated_status)
    if len(terminal_ids) <= 8:
        raise RuntimeError(f"expected more than 8 terminal actors, found {terminal_ids}")

    if close_terminals:
        before_close = len(terminal_ids)
        layout_before_close = len(re.findall(r"^accepted synchronized layout ", text(), re.MULTILINE))
        for _ in range(9):
            key(16)
        until = time.monotonic() + 30
        closed_status = populated_status
        while time.monotonic() < until:
            closed_status = ctl_status_json()
            if len(terminal_actor_ids(closed_status)) < before_close:
                break
            time.sleep(.1)
        if len(terminal_actor_ids(closed_status)) >= before_close:
            raise RuntimeError("terminal actors did not begin closing")
        write_status("closed", closed_status)
        wait_for(
            lambda: len(re.findall(r"^accepted synchronized layout ", text(), re.MULTILINE))
            > layout_before_close
        )

    started = len(text())
    key(32)
    wait_for(lambda: bool(re.findall(r"started presentation WASM actor id=diff-viewer-\S+ ", text()[started:])))
    diff_match = re.findall(
        r"started presentation WASM actor id=(diff-viewer-\S+) ", text()[started:]
    )
    diff_actor = diff_match[-1]
    wait_for(lambda: f"presentation actor worker finished actor={diff_actor}" in text()[started:], 30)
    expect_failure = os.environ.get("BROOD_PLUGIN_DRAG_EXPECT_FACET_FAILURE") == "1"
    if expect_failure:
        wait_for(lambda: f"retiring failed visual actor={diff_actor} " in text()[started:])
        wait_for(lambda: not visible_actor("diff-viewer"))
        # Replace the failed pane with another real terminal so both the root
        # and workspace dividers exercise topology after generation retirement.
        key(20)
        wait_for(lambda: sum(name.startswith("terminal-ui") and pane in projection()
                            for name, pane in actor_panes().items()) >= 2)
    def diff_presented_after_worker():
        tail = text()[started:].split(f"presentation actor worker finished actor={diff_actor}", 1)
        return len(tail) == 2 and bool(re.search(
            rf"facet native pair committed .*actor={re.escape(diff_actor)} ", tail[1]))
    if not expect_failure:
        wait_for(diff_presented_after_worker)
        if os.environ.get("BROOD_PLUGIN_DRAG_LARGE_DIFF") == "1":
            # End/Home exercise navigation far beyond the materialized rows.
            key(107, "--none")
            capture("large-diff-end")
            key(102, "--none")
            capture("large-diff-home")

    expected = ["terminal-ui"] if expect_failure else ["diff-viewer", "terminal-ui"]

    root = drag_case(
        "many-actors-root",
        expected,
        -100,
        boundary="shell",
    )
    root_status = ctl_status_json()
    write_status("root", root_status)
    try:
        root["idle_callbacks"] = idle_callback_check("root-idle", root_status)
    except Exception as error:
        root["idle_callback_error"] = str(error)
        root["errors"].append(str(error))
        (out / "drag-results.json").write_text(json.dumps(results, indent=2))

    tile = drag_case(
        "many-actors-tile",
        expected,
        100,
        boundary="tile",
    )
    tile_status = ctl_status_json()
    write_status("tile", tile_status)
    try:
        tile["idle_callbacks"] = idle_callback_check("tile-idle", tile_status)
    except Exception as error:
        tile["idle_callback_error"] = str(error)
        tile["errors"].append(str(error))
    if not close_terminals and len(terminal_actor_ids(tile_status)) <= 8:
        raise RuntimeError("terminal-session actor count dropped below 9 after drags")

    floating_window()
    drag_case("many-actors-window-narrow", expected, -400, boundary="window")
    drag_case("many-actors-window-restore", expected, 400, boundary="window")
    if not expect_failure and f"visual facet failed actor={diff_actor}" in text()[started:]:
        results[-1]["errors"].append("diff facet failed during navigation or resizing")
    (out / "drag-results.json").write_text(json.dumps(results, indent=2))


def floating_window():
    socket = next(Path(os.environ["XDG_RUNTIME_DIR"]).glob("sway-ipc.*.sock"))
    scale = float(os.environ.get("BROOD_PLUGIN_DRAG_OUTPUT_SCALE", "1"))
    width, height = round(1280 / scale), round(720 / scale)
    subprocess.run(["swaymsg", "-s", str(socket),
                    f'[pid={args.client_pid}] floating enable, border none, resize set {width} {height}, move position 0 0'],
                   check=True, stdout=subprocess.DEVNULL)
    subprocess.run(["swaymsg", "-s", str(socket), "floating_modifier Mod4"],
                   check=True, stdout=subprocess.DEVNULL)
    time.sleep(.5)


try:
    def shell_ready():
        panes, sizes = actor_panes(), projection()
        records = re.findall(r"resize-present .*?composition_size=(\d+)x(\d+)", text())
        if not records:
            return False
        width, height = map(int, records[-1])
        return (visible_actor("terminal-ui")
                and abs(width - 1280) <= 2 and abs(height - 720) <= 2
                and sizes.get(panes.get("session-switcher"), [])[1:] == [320, height - 34, 320, height - 34]
                and sizes.get(panes.get("status-bar"), [])[1:] == [width, 34, width, 34])
    wait_for(shell_ready)
    time.sleep(.5)
    if os.environ.get("BROOD_PLUGIN_DRAG_MANY_ACTORS") == "1":
        many_actors_mode()
    elif os.environ.get("BROOD_PLUGIN_DRAG_PROOF_ONLY") == "1":
        drag_case("resize-proof-narrow", ["resize-proof-fast"])
        drag_case("resize-proof-wide", ["resize-proof-fast"], 180)
    else:
        drag_case("shell-narrow", ["session-switcher", "terminal-ui", "status-bar"],
                  boundary="shell", fixed=("status-bar",))
        # Return towards the original width even when a failed drag never moved.
        drag_case("shell-return", ["session-switcher", "terminal-ui"], 80, boundary="shell")
        key(32)
        wait_for(lambda: visible_actor("diff-viewer"))
        time.sleep(1)
        drag_case("diff-narrow", ["diff-viewer", "terminal-ui"])
        drag_case("diff-wide", ["diff-viewer", "terminal-ui"], 180)
        key(18)
        wait_for(lambda: visible_actor("neovim-ui"))
        key(1, "--none")
        editor_lines = "\n".join(f"Resize line {number:02d}: verify text clipping across changing pane widths." for number in range(24))
        subprocess.run([str(out / "virtual-keyboard-key"), "--text", "i" + editor_lines], check=True)
        key(1, "--none")
        time.sleep(.7)
        drag_case("editor-narrow", ["neovim-ui"])
        drag_case("editor-wide", ["neovim-ui"], 180)
        key(30)
        key(28, "--none")
        wait_for(lambda: visible_actor("coding-agent"))
        # A draft exercises wrapping without starting an external model job.
        subprocess.run(["wtype", "Resize draft: verify wrapping and controls while dragging the pane boundary. " * 4], check=True)
        drag_case("agent-narrow", ["coding-agent"])
        drag_case("agent-wide", ["coding-agent"], 180)
        key(23)
        drag_case("inbox-narrow", ["inbox", "session-switcher"], -100, "inbox")
        drag_case("inbox-wide", ["inbox", "session-switcher"], 100, "inbox")
        # These actors have no user-resizable in-client divider. Exercise real
        # window-resize drags from inside the client so their viewport changes.
        floating_window()
        drag_case("status-window-narrow", ["status-bar"], -700, "window")
        drag_case("status-window-wide", ["status-bar"], 700, "window")
        key(25)
        drag_case("picker-window-narrow", ["picker"], -700, "window")
        drag_case("picker-window-wide", ["picker"], 700, "window")
        key(1, "--none")
        key(53, "--ctrl-shift-only")
        subprocess.run(["wtype", "toggle session switcher", "-k", "Return"], check=True)
        drag_case("keybindings-window-narrow", ["keybindings"], -700, "window")
        drag_case("keybindings-window-wide", ["keybindings"], 700, "window")
except Exception as error:
    results.append({"case": "setup-or-runtime-failure", "errors": [str(error)]})
    try:
        results[-1]["failure_frame"] = capture("failure")
    except Exception as capture_error:
        results[-1]["failure_capture_error"] = str(capture_error)
finally:
    (out / "drag-results.json").write_text(json.dumps(results, indent=2))
    summary = ["# Real plugin divider drag results", "",
               "90 real Wayland motions per drag, without waiting for Brood promotion.",
               "Intermediate captures are retained in drag-frames/.", ""]
    for result in results:
        summary.append(f"- {result['case']}: " +
                       ("FAIL: " + "; ".join(result["errors"]) if result["errors"] else "PASS"))
    (out / "drag-report.md").write_text("\n".join(summary) + "\n")
raise SystemExit(1 if any(r["errors"] for r in results) else 0)
