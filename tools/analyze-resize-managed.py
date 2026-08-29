#!/usr/bin/env python3
"""Strict trace and pixel checks for managed synchronized resize proof."""

from __future__ import annotations

import json
import pathlib
import re
import subprocess
import sys
import tempfile
from typing import Any


def fields(text: str) -> dict[str, str]:
    return {
        key: value.rstrip(",")
        for key, value in re.findall(r"(?:^| )([A-Za-z0-9_]+)=([^ ]+)", text)
    }


def parse_ids(text: str, name: str) -> list[int]:
    match = re.search(
        rf"(?:^| ){re.escape(name)}=(?:\[([^]]*)\]|\{{([^}}]*)\}})", text
    )
    if match is None or not match.group(1):
        if match is None or not match.group(2):
            return []
    return [int(value) for value in re.findall(r"\d+", match.group(1) or match.group(2))]


def parse_rects(text: str) -> dict[int, tuple[int, int, int, int]]:
    match = re.search(r"(?:^| )target_rects=\[([^]]*)\]", text)
    if match is None:
        return {}
    result = {}
    for encoded in filter(None, match.group(1).split(",")):
        pane, x, y, width, height = map(int, encoded.split(":"))
        result[pane] = (x, y, width, height)
    return result


def matching(
    lines: list[str], pattern: str, after: int = 0, before: int | None = None
) -> list[tuple[int, str]]:
    regex = re.compile(pattern)
    end = len(lines) if before is None else before - 1
    return [
        (index, text)
        for index, text in enumerate(lines[after:end], after + 1)
        if regex.search(text)
    ]


def first(
    lines: list[str], pattern: str, after: int = 0, before: int | None = None
) -> tuple[int, str] | None:
    found = matching(lines, pattern, after, before)
    return found[0] if found else None


def exact_round(pixel: int, scale_120: int) -> int:
    return (pixel * 120 + scale_120 // 2) // scale_120


def real_pointer_trace_is_valid(records: list[dict[str, Any]]) -> bool:
    pointer = [record for record in records if record.get("action", "").startswith("pointer-")]
    expected = [
        ("pointer-hover", "motion"),
        ("pointer-press", "press"),
        ("pointer-motion-a", "motion"),
        ("pointer-motion-b", "motion"),
        ("pointer-release", "release"),
    ]
    if [record.get("action") for record in pointer] != [item[0] for item in expected]:
        return False
    position: tuple[int, int] | None = None
    for sequence, (record, (_action, helper_action)) in enumerate(
        zip(pointer, expected, strict=True), 1
    ):
        command = str(record.get("command", ""))
        if helper_action == "motion":
            match = re.fullmatch(r"motion (-?\d+) (-?\d+)", command)
            if match is None:
                return False
            position = (int(match.group(1)), int(match.group(2)))
        elif command != helper_action or position is None:
            return False
        ack = re.fullmatch(
            r"ack sequence=(\d+) action=(motion|press|release) "
            r"x=(-?\d+) y=(-?\d+) roundtrip_ms=(\d+)",
            str(record.get("ack", "")),
        )
        if (
            ack is None
            or int(ack.group(1)) != sequence
            or ack.group(2) != helper_action
            or (int(ack.group(3)), int(ack.group(4))) != position
        ):
            return False
    return True


def sway_content_geometry(tree: dict[str, Any], pid: int) -> tuple[int, int, int, int]:
    pending = [tree]
    while pending:
        node = pending.pop()
        if node.get("pid") == pid:
            rect = node["rect"]
            window = node.get("window_rect") or {
                "x": 0,
                "y": 0,
                "width": rect["width"],
                "height": rect["height"],
            }
            return (
                int(rect["x"]) + int(window["x"]),
                int(rect["y"]) + int(window["y"]),
                int(window["width"]),
                int(window["height"]),
            )
        pending.extend(node.get("nodes", []))
        pending.extend(node.get("floating_nodes", []))
    raise SystemExit(f"presentation client pid {pid} is absent from Sway tree")


def image_strip(path: pathlib.Path, rect: tuple[int, int, int, int]) -> list[int]:
    x, y, width, height = rect
    if min(width, height) <= 12:
        raise SystemExit(f"pane rectangle is too small for fiducial: {rect}")
    command = [
        "magick",
        str(path),
        "-crop",
        f"{width}x1+{x}+{y + 6}",
        "+repage",
        "-resize",
        "64x1!",
        "-alpha",
        "off",
        "-colorspace",
        "gray",
        "-depth",
        "8",
        "txt:-",
    ]
    output = subprocess.run(command, check=True, text=True, capture_output=True).stdout
    samples = []
    for text in output.splitlines():
        match = re.search(r"gray\((\d+)\)", text)
        if match:
            samples.append(int(match.group(1)))
    if len(samples) != 64:
        raise SystemExit(f"could not decode 64-bit pane fiducial from {path}: {len(samples)}")
    return samples


def decode_content(path: pathlib.Path, rect: tuple[int, int, int, int]) -> int:
    result = 0
    for bit, value in enumerate(image_strip(path, rect)):
        if value >= 128:
            result |= 1 << bit
    return result


def load_rgb(path: pathlib.Path) -> tuple[int, int, bytes]:
    width, height = map(
        int,
        subprocess.check_output(
            ["magick", str(path), "-format", "%w %h", "info:"], text=True
        ).split(),
    )
    raw = subprocess.check_output(["magick", str(path), "-depth", "8", "rgb:-"])
    if len(raw) != width * height * 3:
        raise SystemExit(f"unexpected RGB byte count for {path}")
    return width, height, raw


def one_pixel_divider(path: pathlib.Path, expected_x: int) -> bool:
    width, height, raw = load_rgb(path)
    linear_colours = ((0.48, 0.54, 0.64), (0.35, 0.66, 0.95), (0.20, 0.78, 1.0))

    def srgb(value: float) -> int:
        if value <= 0.0031308:
            return round(value * 12.92 * 255)
        return round((1.055 * value ** (1 / 2.4) - 0.055) * 255)

    palette = [
        encoded
        for colour in linear_colours
        for encoded in (
            tuple(round(channel * 255) for channel in colour),
            tuple(srgb(channel) for channel in colour),
        )
    ]

    def match(x: int, y: int) -> bool:
        offset = (y * width + x) * 3
        sample = raw[offset : offset + 3]
        return min(
            sum((sample[index] - target[index]) ** 2 for index in range(3))
            for target in palette
        ) <= 32**2

    start, end = round(height * 0.08), round(height * 0.92)
    candidates = []
    for x in range(max(0, expected_x - 3), min(width, expected_x + 4)):
        coverage = sum(match(x, y) for y in range(start, end)) / max(end - start, 1)
        if coverage >= 0.55:
            candidates.append(x)
    return candidates == [expected_x]


def fixture_commit_for_serial(lines: list[str], serial: int) -> tuple[int, str] | None:
    candidates = [
        entry
        for entry in matching(lines, rf"event=commit .*serial={serial}(?: |$)")
        if "kind=exact" in entry[1]
    ]
    return candidates[-1] if candidates else None


def configured_status_tuple(
    text: str,
) -> tuple[tuple[int, int], tuple[int, int], int] | None:
    match = re.search(
        r"result=Configured \{ logical_size: \[(\d+), (\d+)\], "
        r"pixel_size: \[(\d+), (\d+)\], scale_120: (\d+) \}",
        text,
    )
    if match is None:
        return None
    values = tuple(map(int, match.groups()))
    return ((values[0], values[1]), (values[2], values[3]), values[4])


def self_test() -> int:
    sample = (
        "accepted synchronized layout epoch=9 wait_for_panes=[4,7,8] "
        "target_rects=[7:0:0:300:200,8:301:0:299:200]"
    )
    assert parse_ids(sample, "wait_for_panes") == [4, 7, 8]
    assert parse_rects(sample)[8] == (301, 0, 299, 200)
    assert exact_round(451, 180) == 301
    assert fields("x epoch=9 degraded=false")["epoch"] == "9"
    assert configured_status_tuple(
        "managed resize status backend=3 connection_generation=4 pane=5 epoch=6 "
        "result=Configured { logical_size: [207, 229], "
        "pixel_size: [310, 343], scale_120: 180 }"
    ) == ((207, 229), (310, 343), 180)
    assert configured_status_tuple("result=Rejected { reason: PaneUnavailable }") is None
    pointer_records = [
        {
            "action": "pointer-hover",
            "command": "motion 40 20",
            "ack": "ack sequence=1 action=motion x=40 y=20 roundtrip_ms=0",
        },
        {
            "action": "pointer-press",
            "command": "press",
            "ack": "ack sequence=2 action=press x=40 y=20 roundtrip_ms=0",
        },
        {
            "action": "pointer-motion-a",
            "command": "motion 38 20",
            "ack": "ack sequence=3 action=motion x=38 y=20 roundtrip_ms=1",
        },
        {
            "action": "pointer-motion-b",
            "command": "motion 36 20",
            "ack": "ack sequence=4 action=motion x=36 y=20 roundtrip_ms=0",
        },
        {
            "action": "pointer-release",
            "command": "release",
            "ack": "ack sequence=5 action=release x=36 y=20 roundtrip_ms=0",
        },
        {"action": "native-old-buffer-growth"},
    ]
    assert real_pointer_trace_is_valid(pointer_records)
    pointer_records[3]["ack"] = "ack sequence=4 action=motion x=35 y=20 roundtrip_ms=0"
    assert not real_pointer_trace_is_valid(pointer_records)
    tree = {
        "nodes": [],
        "floating_nodes": [
            {
                "pid": 77,
                "rect": {"x": 120, "y": 80, "width": 804, "height": 524},
                "window_rect": {"x": 2, "y": 22, "width": 800, "height": 500},
            }
        ],
    }
    assert sway_content_geometry(tree, 77) == (122, 102, 800, 500)
    with tempfile.TemporaryDirectory(prefix="brood-managed-analyzer-") as directory:
        single = pathlib.Path(directory) / "single.png"
        double = pathlib.Path(directory) / "double.png"
        subprocess.run(
            ["magick", "-size", "32x32", "xc:#101010", "-fill", "#b8c2d1", "-draw", "line 17,0 17,31", str(single)],
            check=True,
        )
        subprocess.run(
            ["magick", "-size", "32x32", "xc:#101010", "-fill", "#b8c2d1", "-draw", "rectangle 17,0 18,31", str(double)],
            check=True,
        )
        assert one_pixel_divider(single, 17)
        assert not one_pixel_divider(double, 17)
        alpha_bits = pathlib.Path(directory) / "alpha-bits.png"
        subprocess.run(
            [
                "magick",
                "-size",
                "320x32",
                "xc:none",
                "-fill",
                "#121212ff",
                "-draw",
                "rectangle 0,0 319,31",
                "-fill",
                "#eaeaeaff",
                "-draw",
                "rectangle 0,0 4,10 rectangle 315,0 319,10",
                str(alpha_bits),
            ],
            check=True,
        )
        assert "a" in subprocess.check_output(
            ["magick", str(alpha_bits), "-format", "%[channels]", "info:"], text=True
        )
        assert decode_content(alpha_bits, (0, 0, 320, 32)) == (1 | (1 << 63))
    print("managed_resize_analyzer_self_test=passed")
    return 0


def analyze(root: pathlib.Path, evidence_path: pathlib.Path) -> dict[str, Any]:
    evidence = json.loads(evidence_path.read_text())
    client_lines = pathlib.Path(evidence["client_log"]).read_text(
        encoding="utf-8", errors="replace"
    ).splitlines()
    server_lines = pathlib.Path(evidence["server_log"]).read_text(
        encoding="utf-8", errors="replace"
    ).splitlines()
    fixture_lines = [
        pathlib.Path(path).read_text(encoding="utf-8", errors="replace").splitlines()
        for path in evidence["fixtures"]
    ]
    inputs = [
        json.loads(line)
        for line in pathlib.Path(evidence["inputs"]).read_text().splitlines()
        if line
    ]
    frames = {
        record["label"]: record
        for record in (
            json.loads(line)
            for line in pathlib.Path(evidence["manifest"]).read_text().splitlines()
            if line
        )
    }
    panes = {name: int(value) for name, value in evidence["panes"].items()}
    managed = {panes["managed_a"], panes["managed_b"]}
    transactions = evidence["transactions"]

    def component_actor_pane(actor: str) -> int | None:
        started = first(
            client_lines,
            rf"started presentation WASM actor id={re.escape(actor)} kind=wasm-component ",
        )
        if started is None:
            return None
        pane = fields(started[1]).get("pane")
        return int(pane) if pane is not None else None

    expected_plugin_panes = {
        pane
        for pane in (
            component_actor_pane("resize-proof-slow"),
            component_actor_pane("terminal-ui"),
        )
        if pane is not None
    }

    def client_line(index: int) -> str:
        if index <= 0 or index > len(client_lines):
            raise SystemExit(f"invalid client evidence line: {index}")
        return client_lines[index - 1]

    def tx(name: str) -> tuple[dict[str, int], str]:
        record = transactions[name]
        text = client_line(record["accepted"])
        if fields(text).get("epoch") != str(record["epoch"]):
            raise SystemExit(f"{name} accepted line does not match its epoch")
        return record, text

    tx_data = {name: tx(name) for name in transactions}
    accepted_waits = {
        name: set(parse_ids(text, "wait_for_panes"))
        for name, (_record, text) in tx_data.items()
    }
    accepted_rects = {
        name: parse_rects(text) for name, (_record, text) in tx_data.items()
    }

    def admissions_for(name: str) -> list[dict[str, str]]:
        epoch = transactions[name]["epoch"]
        return [
            fields(text)
            for _index, text in matching(
                client_lines,
                rf"admitted managed resize expectation .*epoch={epoch}(?: |$)",
            )
            if fields(text).get("epoch") == str(epoch)
        ]

    admissions_by_transaction = {
        name: admissions_for(name)
        for name in ("pointer", "release", "native", "wrong", "timeout", "abort")
    }

    def epoch_matches(pattern: str, epoch: int, after: int = 0) -> list[tuple[int, str]]:
        return matching(client_lines, rf"{pattern}.*(?:epoch|resize_epoch)={epoch}(?: |$)", after)

    def exact_path(label: str) -> pathlib.Path:
        frame = frames[label]
        return root / frame["file"]

    pointer = transactions["pointer"]
    pointer_old = transactions["pointer_old"]
    pointer_fields = fields(tx_data["pointer"][1])
    old_fields = fields(tx_data["pointer_old"][1])
    pointer_stages = epoch_matches("staged managed frame", pointer["epoch"], pointer["accepted"])
    pointer_stage_panes = {int(fields(text)["pane"]) for _index, text in pointer_stages}
    pointer_plugin = epoch_matches("staged plugin draw", pointer["epoch"], pointer["accepted"])
    pointer_branch_panes: dict[int, set[int]] = {}
    for _index, text in epoch_matches(
        "layout-region pane-notified", pointer["epoch"], pointer["accepted"]
    ):
        values = fields(text)
        pointer_branch_panes.setdefault(int(values["owner_lease"]), set()).add(
            int(values["pane"])
        )
    pointer_ready = first(
        client_lines,
        rf"synchronized layout ready .*epoch={pointer['epoch']} .*degraded=false",
        pointer["accepted"],
    )
    pointer_complete = first(
        client_lines,
        rf"completed synchronized layout .*epoch={pointer['epoch']}(?: |$)",
        pointer["accepted"],
    )
    pointer_held_text = client_line(pointer_old["held"])
    pointer_gate_release_text = client_line(pointer["gate_release"])
    pointer_drop_text = client_line(pointer["old_drop"])

    admission_fields = admissions_by_transaction["pointer"]
    all_admission_fields = [
        item for admissions in admissions_by_transaction.values() for item in admissions
    ]
    scale_contract = all(
        int(item["scale_120"]) == 180
        and int(item["logical_size"].split("x")[0])
        == exact_round(int(item["pixel_size"].split("x")[0]), 180)
        and int(item["logical_size"].split("x")[1])
        == exact_round(int(item["pixel_size"].split("x")[1]), 180)
        for item in all_admission_fields
    )

    def server_event(message: str, epoch: int, backend_pane: int) -> tuple[int, str] | None:
        for index, text in enumerate(server_lines, 1):
            values = fields(text)
            if (
                message in text
                and values.get("epoch") == str(epoch)
                and values.get("pane") == str(backend_pane)
            ):
                return index, text
        return None

    def server_snapshot(epoch: int, backend_pane: int) -> tuple[int, str] | None:
        return server_event("managed resize snapshot queued", epoch, backend_pane)

    def route_stages_match(name: str) -> bool:
        epoch = transactions[name]["epoch"]
        stage_values = [
            fields(text)
            for _index, text in epoch_matches(
                "staged managed frame", epoch, transactions[name]["accepted"]
            )
        ]
        for route in admissions_by_transaction[name]:
            if not any(
                stage.get("backend") == route.get("backend")
                and stage.get("connection_generation")
                == route.get("connection_generation")
                and stage.get("pane") == route.get("pane")
                and stage.get("backend_pane") == route.get("backend_pane")
                and stage.get("epoch") == route.get("epoch")
                for stage in stage_values
            ):
                return False
        return len(stage_values) >= len(admissions_by_transaction[name])

    def route_statuses_match(name: str) -> bool:
        epoch = transactions[name]["epoch"]
        status_values = [
            fields(text)
            for _index, text in epoch_matches(
                "managed resize status", epoch, transactions[name]["accepted"]
            )
            if " result=Configured" in text
        ]
        return all(
            any(
                status.get("backend") == route.get("backend")
                and status.get("connection_generation")
                == route.get("connection_generation")
                and status.get("pane") == route.get("pane")
                and status.get("epoch") == route.get("epoch")
                for status in status_values
            )
            for route in admissions_by_transaction[name]
        )

    def exact_managed_correlation(name: str) -> tuple[bool, list[dict[str, Any]]]:
        transaction = transactions[name]
        ready = first(
            client_lines,
            rf"synchronized layout ready .*epoch={transaction['epoch']}(?: |$)",
            transaction["accepted"],
            transaction["promoted"] + 1,
        )
        details: list[dict[str, Any]] = []
        valid = ready is not None
        for route in admissions_by_transaction[name]:
            epoch = int(route["epoch"])
            backend_pane = int(route["backend_pane"])
            logical = tuple(map(int, route["logical_size"].split("x")))
            pixel = tuple(map(int, route["pixel_size"].split("x")))
            scale_120 = int(route["scale_120"])
            configure = server_event("managed resize configure issued", epoch, backend_pane)
            snapshot = server_event("managed resize snapshot queued", epoch, backend_pane)
            published = server_event("managed resize scene published", epoch, backend_pane)
            presented = server_event("managed resize scene presented", epoch, backend_pane)
            status = next(
                (
                    (index, text)
                    for index, text in epoch_matches(
                        "managed resize status",
                        epoch,
                        transaction["accepted"],
                    )
                    if fields(text).get("backend") == route.get("backend")
                    and fields(text).get("connection_generation")
                    == route.get("connection_generation")
                    and fields(text).get("pane") == route.get("pane")
                    and configured_status_tuple(text) is not None
                ),
                None,
            )
            stage = next(
                (
                    (index, text)
                    for index, text in epoch_matches(
                        "staged managed frame",
                        epoch,
                        transaction["accepted"],
                    )
                    if fields(text).get("backend") == route.get("backend")
                    and fields(text).get("connection_generation")
                    == route.get("connection_generation")
                    and fields(text).get("pane") == route.get("pane")
                    and fields(text).get("backend_pane") == route.get("backend_pane")
                ),
                None,
            )
            chain_valid = all(
                entry is not None
                for entry in (configure, snapshot, published, presented, status, stage, ready)
            )
            values: dict[str, dict[str, str]] = {}
            if chain_valid:
                values = {
                    "configure": fields(configure[1]),
                    "snapshot": fields(snapshot[1]),
                    "published": fields(published[1]),
                    "presented": fields(presented[1]),
                    "stage": fields(stage[1]),
                }
                configured = configured_status_tuple(status[1])
                expected_tuple = (logical, pixel, scale_120)
                chain_valid = (
                    configure[0] < snapshot[0] < published[0] < presented[0]
                    and transaction["accepted"] < status[0] < ready[0]
                    and transaction["accepted"] < stage[0] < ready[0]
                    and ready[0] < transaction["promoted"]
                    and configured == expected_tuple
                    and (
                        int(values["configure"]["logical_width"]),
                        int(values["configure"]["logical_height"]),
                    )
                    == logical
                    and (
                        int(values["configure"]["pixel_width"]),
                        int(values["configure"]["pixel_height"]),
                    )
                    == pixel
                    and int(values["configure"]["scale_120"]) == scale_120
                    and (
                        int(values["snapshot"]["logical_width"]),
                        int(values["snapshot"]["logical_height"]),
                    )
                    == logical
                    and (
                        int(values["snapshot"]["pixel_width"]),
                        int(values["snapshot"]["pixel_height"]),
                    )
                    == pixel
                    and values["configure"].get("serial")
                    == values["snapshot"].get("serial")
                    and values["snapshot"].get("surface_commit")
                    == values["published"].get("surface_commit")
                    == values["presented"].get("surface_commit")
                    and values["published"].get("scene_sequence")
                    == values["presented"].get("scene_sequence")
                    == values["stage"].get("sequence")
                    and values["published"].get("transport_resource")
                    == values["presented"].get("transport_resource")
                    == values["stage"].get("resource")
                    and values["published"].get("transport_resource") is not None
                    and values["stage"].get("source") == route.get("pixel_size")
                    and values["stage"].get("resource_extent")
                    == route.get("pixel_size")
                )
            details.append(
                {
                    "backend": int(route["backend"]),
                    "connection_generation": int(route["connection_generation"]),
                    "pane": int(route["pane"]),
                    "backend_pane": backend_pane,
                    "epoch": epoch,
                    "logical_size": list(logical),
                    "pixel_size": list(pixel),
                    "scale_120": scale_120,
                    "serial": int(values.get("configure", {}).get("serial", 0)),
                    "scene_sequence": int(
                        values.get("published", {}).get("scene_sequence", 0)
                    ),
                    "transport_resource": int(
                        values.get("published", {}).get("transport_resource", 0)
                    ),
                    "verified": chain_valid,
                }
            )
            valid &= chain_valid
        return valid and bool(details), details

    # tracing-subscriber field order is not a wire contract. Parse each line,
    # then join server pane/epoch/serial to the real fixture ACK and commit.
    expected_content: dict[tuple[str, int], int] = {}
    for name in ("pointer", "native", "wrong"):
        epoch = transactions[name]["epoch"]
        epoch_admissions = admissions_by_transaction[name]
        for fixture_index, backend_pane in enumerate(
            (panes["managed_a"], panes["managed_b"])
        ):
            admission = next(
                (
                    item
                    for item in epoch_admissions
                    if int(item.get("pane", -1)) == backend_pane
                    and int(item.get("epoch", -1)) == epoch
                ),
                None,
            )
            backend_id = int(admission["backend_pane"]) if admission else -1
            snapshot = server_snapshot(epoch, backend_id)
            if snapshot:
                serial = int(fields(snapshot[1])["serial"])
                commit = fixture_commit_for_serial(fixture_lines[fixture_index], serial)
                if commit:
                    expected_content[(name, backend_pane)] = int(fields(commit[1])["content"])

    visual: dict[str, dict[str, Any]] = {}
    for name, label in {
        "pointer": "pointer-promoted",
        "native": "native-promoted",
        "timeout": "timeout-degraded",
    }.items():
        rects = accepted_rects[name]
        decoded: dict[int, int] = {}
        for pane in managed:
            if pane not in rects:
                continue
            decoded[pane] = decode_content(exact_path(label), rects[pane])
        expected_name = "wrong" if name == "timeout" else name
        visual[name] = {
            "decoded": decoded,
            "expected": {
                pane: expected_content.get((expected_name, pane))
                for pane in sorted(managed)
            },
        }

    native = transactions["native"]
    native_stages = epoch_matches("staged managed frame", native["epoch"], native["accepted"])
    native_stage_panes = {int(fields(text)["pane"]) for _index, text in native_stages}
    native_unsuitable = server_lines[native["unsuitable_server"] - 1]
    native_b_old = first(fixture_lines[1], r"event=commit .*kind=old-after-ack")
    native_growth = [
        record for record in inputs if record.get("action") == "native-old-buffer-growth"
    ]
    native_promoted_fields = fields(client_line(native["promoted"]))

    wrong = transactions["wrong"]
    wrong_unsuitable = server_lines[wrong["unsuitable_server"] - 1]
    wrong_stage = epoch_matches("staged managed frame", wrong["epoch"], wrong["accepted"])
    wrong_route_b = next(
        item
        for item in admissions_by_transaction["wrong"]
        if int(item["pane"]) == panes["managed_b"]
    )
    backend_b = int(
        wrong_route_b["backend_pane"]
    )
    wrong_snapshot = server_snapshot(wrong["epoch"], backend_b)
    wrong_configure = server_event("managed resize configure issued", wrong["epoch"], backend_b)
    wrong_unsuitable_fields = fields(wrong_unsuitable)
    wrong_exact_command = (
        fixture_commit_for_serial(
            fixture_lines[1], int(fields(wrong_configure[1])["serial"])
        )
        if wrong_configure is not None
        else None
    )

    timeout = transactions["timeout"]
    timeout_ready_text = client_line(timeout["ready"])
    timeout_ready_fields = fields(timeout_ready_text)
    timeout_promoted = first(
        client_lines,
        rf"window-resize-present .*epoch={timeout['epoch']} .*phase=degraded .*degraded=true",
        timeout["ready"],
    )
    timeout_restart = first(
        client_lines,
        r"requested managed window resize .*resize_epoch=None",
        timeout["ready"],
    )
    timeout_server_restart = first(
        server_lines,
        rf"accepted and armed ordinary managed recovery .*pane={backend_b} .*recover_through_epoch={timeout['epoch']} .*recovery_floor=\d+",
    )
    timeout_ordinary = first(fixture_lines[1], r"event=commit .*kind=ordinary-command")
    timeout_ordinary_client = client_line(timeout["ordinary"])

    abort = transactions["abort"]
    abort_text = client_line(abort["terminal"])
    abort_current_match = re.search(r"current \[([^]]*)\]", abort_text)
    abort_current_inventory = (
        {int(value) for value in re.findall(r"\d+", abort_current_match.group(1))}
        if abort_current_match is not None
        else set()
    )
    abort_forbidden_after_terminal = [
        entry
        for entry in epoch_matches("", abort["epoch"], abort["terminal"])
        if any(
            marker in entry[1]
            for marker in (
                "synchronized layout ready",
                "completed synchronized layout",
                "degraded synchronized layout",
                "phase=promoted",
                "phase=degraded",
            )
        )
    ]
    abort_stage_after_terminal = epoch_matches(
        "staged managed frame", abort["epoch"], abort["terminal"]
    )
    abort_cleared_trace = first(
        client_lines,
        r"(?:^| )resize-present .*managed_expectations=0 "
        r"managed_staged_resources=0 ",
        abort["terminal"],
    )
    abort_inventory_publication = first(
        client_lines,
        r"queued layout-state revision=\d+ lease=2 actor=workspace(?: |$)",
        abort["terminal"],
    )
    abort_inventory_revision = (
        fields(abort_inventory_publication[1]).get("revision")
        if abort_inventory_publication is not None
        else None
    )
    abort_replacement_layout = (
        first(
            client_lines,
            rf"accepted leased actor layout items=\d+ panes={len(accepted_waits['abort']) - 1} "
            rf"revision={abort_inventory_revision} ",
            abort_inventory_publication[0],
        )
        if abort_inventory_publication is not None
        else None
    )
    abort_presented_revision = (
        fields(abort_replacement_layout[1]).get("pending_presented_layout_revision")
        if abort_replacement_layout is not None
        else None
    )
    abort_replacement_presented = (
        first(
            client_lines,
            rf"presented committed layout revision={abort_presented_revision}(?: |$)",
            abort_replacement_layout[0],
        )
        if abort_replacement_layout is not None
        else None
    )

    release = transactions["release"]
    capture_release = first(
        client_lines,
        rf"pointer-policy capture released interaction={pointer_fields.get('interaction')} ",
        pointer["promoted"],
        release["accepted"] + 1,
    )
    release_complete = first(
        client_lines,
        rf"completed synchronized layout .*epoch={release['epoch']}(?: |$)",
        release["promoted"],
    )

    resize_present = matching(client_lines, r"(?:^| )resize-present ") + matching(
        client_lines, r"(?:^| )window-resize-present "
    )
    staged_counts = [
        int(fields(text)["staged_count"])
        for _index, text in resize_present
        if "staged_count" in fields(text)
    ]
    managed_counts = [
        (
            int(fields(text)["managed_expectations"]),
            int(fields(text)["managed_staged_resources"]),
        )
        for _index, text in resize_present
        if "managed_expectations" in fields(text)
        and "managed_staged_resources" in fields(text)
    ]

    pointer_promoted_values = visual["pointer"]["decoded"]
    pointer_expected_values = visual["pointer"]["expected"]
    native_promoted_values = visual["native"]["decoded"]
    native_expected_values = visual["native"]["expected"]
    timeout_degraded_values = visual["timeout"]["decoded"]
    timeout_expected_values = visual["timeout"]["expected"]
    divider_x = int(fields(client_line(pointer["promoted"])).get("divider_x", "-1"))
    managed_chrome_aligned = divider_x >= 0 and one_pixel_divider(
        exact_path("pointer-promoted"), divider_x
    )
    capture_geometry_matches = True
    for frame in frames.values():
        trace_fields = fields(frame["trace"]["text"])
        if "composition_size" in trace_fields:
            expected = tuple(map(int, trace_fields["composition_size"].split("x")))
        else:
            expected = (int(trace_fields["width"]), int(trace_fields["height"]))
        logical = frame["capture_geometry"]["logical_size"]
        physical_from_crop = tuple((int(value) * 180 + 60) // 120 for value in logical)
        capture_geometry_matches &= tuple(frame["image_size"]) == expected
        capture_geometry_matches &= tuple(frame["image_size"]) == physical_from_crop

    pointer_correlation_valid, pointer_correlations = exact_managed_correlation(
        "pointer"
    )
    native_correlation_valid, native_correlations = exact_managed_correlation("native")

    assertions = {
        "real_pointer_edges_and_motion": real_pointer_trace_is_valid(inputs),
        "pointer_same_interaction_supersedes": (
            old_fields.get("interaction") == pointer_fields.get("interaction")
            and int(pointer_fields.get("event_sequence", "0"))
            > int(old_fields.get("event_sequence", "0"))
            and pointer_old["epoch"] != pointer["epoch"]
            and bool(
                first(
                    client_lines,
                    rf"synchronized interaction superseded interaction={pointer_fields.get('interaction')}",
                    pointer_old["accepted"],
                    pointer["accepted"] + 2,
                )
            )
        ),
        "late_old_managed_tag_dropped": (
            pointer_old["accepted"] < pointer_old["held"] < pointer["accepted"]
            < pointer["gate_release"] < pointer["old_drop"] < pointer["promoted"]
            and f"epoch={pointer_old['epoch']}" in pointer_held_text
            and f"held_epoch={pointer_old['epoch']}" in pointer_gate_release_text
            and f"active_epoch={pointer['epoch']}" in pointer_gate_release_text
            and f"resize_epoch=Some({pointer_old['epoch']})" in pointer_drop_text
        ),
        "mixed_wait_set_names_exact_managed_and_plugin_child_topology": (
            len(expected_plugin_panes) == 2
            and any(
                branch == managed | expected_plugin_panes
                for branch in pointer_branch_panes.values()
            )
            and all(
                (managed | expected_plugin_panes) <= accepted_waits[name]
                for name in ("pointer", "native")
            )
        ),
        "exact_scale_1_5_chain": (
            evidence["scale"] == 1.5
            and len(all_admission_fields) >= 2
            and scale_contract
        ),
        "pointer_mixed_dependencies_stage_before_ready": (
            pointer_stage_panes == managed
            and route_stages_match("pointer")
            and route_statuses_match("pointer")
            and bool(pointer_plugin)
            and pointer_ready is not None
            and pointer_complete is not None
            and max(index for index, _text in pointer_stages + pointer_plugin)
            < pointer_ready[0]
            < pointer["promoted"]
            < pointer_complete[0]
        ),
        "pointer_exact_server_managed_correlation": pointer_correlation_valid,
        "pointer_promotes_two_exact_managed_frames": (
            set(pointer_promoted_values) == managed
            and all(pointer_expected_values[pane] is not None for pane in managed)
            and all(
                pointer_promoted_values[pane] == pointer_expected_values[pane]
                for pane in managed
            )
        ),
        "final_release_after_capture_converges": (
            capture_release is not None
            and capture_release[0] < release["accepted"] < release["promoted"]
            and release_complete is not None
            and release["promoted"] < release_complete[0]
            and fields(tx_data["release"][1]).get("interaction")
            == pointer_fields.get("interaction")
        ),
        "native_old_buffer_cannot_satisfy": (
            "managed resize commit is not suitable" in native_unsuitable
            and native_b_old is not None
            and native_stage_panes == managed
            and route_stages_match("native")
            and route_statuses_match("native")
            and all(native_expected_values[pane] is not None for pane in managed)
        ),
        "native_growth_guard_matches_accepted_target": (
            len(native_growth) == 1
            and int(native_growth[0].get("current_composition", 0))
            < int(native_growth[0].get("expected_composition", 0))
            and int(native_growth[0].get("current_b_width", 0))
            < int(native_growth[0].get("expected_b_width", 0))
            and int(native_growth[0].get("expected_composition", 0))
            == int(native_promoted_fields.get("width", 0))
            and int(native_growth[0].get("expected_b_width", 0))
            == accepted_rects["native"][panes["managed_b"]][2]
        ),
        "native_promotes_two_exact_managed_frames": (
            set(native_promoted_values) == managed
            and all(
                native_promoted_values[pane] == native_expected_values[pane]
                for pane in managed
            )
        ),
        "native_exact_server_managed_correlation": native_correlation_valid,
        "wrong_pixel_buffer_cannot_satisfy": (
            "managed resize commit is not suitable" in wrong_unsuitable
            and wrong_exact_command is not None
            and len(wrong_stage) >= 2
            and wrong_snapshot is not None
            and wrong_configure is not None
            and wrong["unsuitable_server"] < wrong_snapshot[0]
            and fields(wrong_configure[1]).get("serial")
            == wrong_unsuitable_fields.get("serial")
            == fields(wrong_snapshot[1]).get("serial")
            and route_stages_match("wrong")
            and route_statuses_match("wrong")
        ),
        "timeout_degrades_complete_wait_set": (
            timeout_ready_fields.get("degraded") == "true"
            and int(timeout_ready_fields.get("duration_us", "0")) >= 250_000
            and int(timeout_ready_fields.get("duration_us", "0")) < 750_000
            and panes["managed_b"] in parse_ids(timeout_ready_text, "recovered_managed")
            and (
                "old-projected" in timeout_ready_text
                or "placeholder" in timeout_ready_text
            )
        ),
        "timeout_image_keeps_complete_old_managed_set": (
            set(timeout_degraded_values) == managed
            and all(timeout_expected_values[pane] is not None for pane in managed)
            and all(
                timeout_degraded_values[pane] == timeout_expected_values[pane]
                for pane in managed
            )
        ),
        "timeout_clears_stage_and_restarts_ordinary": (
            timeout_promoted is not None
            and fields(timeout_promoted[1]).get("staged_count") == "0"
            and timeout_restart is not None
            and timeout_server_restart is not None
            and timeout_ordinary is not None
            and "accepted ordinary managed frame" in timeout_ordinary_client
            and fields(timeout_ordinary_client).get("pane")
            == str(panes["managed_b"])
        ),
        "removed_target_aborts_without_promotion": (
            "unavailable_panes=" in abort_text
            and panes["managed_b"] in parse_ids(abort_text, "unavailable_panes")
            and bool(abort_current_inventory)
            and panes["managed_b"] not in abort_current_inventory
            and not abort_forbidden_after_terminal
            and not abort_stage_after_terminal
            and abort_cleared_trace is not None
            and abort_inventory_publication is not None
            and abort_replacement_layout is not None
            and abort_replacement_presented is not None
            and abort_inventory_publication[0] < abort_replacement_layout[0]
            < abort_replacement_presented[0]
        ),
        "managed_staging_is_bounded": bool(staged_counts) and max(staged_counts) <= 1,
        "managed_expectations_and_resources_are_bounded": (
            bool(managed_counts)
            and all(
                0 <= staged <= expectations <= 128
                and staged <= len(managed)
                for expectations, staged in managed_counts
            )
            and all(
                len({
                    (item["backend"], item["connection_generation"], item["pane"], item["epoch"])
                    for item in admissions
                })
                == len(admissions)
                for admissions in admissions_by_transaction.values()
            )
        ),
        "capture_epochs_match_manifest": all(
            str(transactions[label.split("-")[0]]["epoch"])
            in frame["trace"]["text"]
            for label, frame in frames.items()
            if label.startswith(("pointer-", "native-"))
        ),
        "managed_frames_keep_one_pixel_native_chrome": managed_chrome_aligned,
        "cropped_content_geometry_matches_composition": capture_geometry_matches,
    }
    return {
        "verified": all(assertions.values()),
        "assertions": assertions,
        "panes": panes,
        "transactions": transactions,
        "visual": visual,
        "correlations": {
            "pointer": pointer_correlations,
            "native": native_correlations,
        },
        "bounds": {
            "maximum_staged_layout_count": max(staged_counts, default=0),
            "maximum_managed_expectations": max(
                (expectations for expectations, _staged in managed_counts), default=0
            ),
            "maximum_managed_staged_resources": max(
                (staged for _expectations, staged in managed_counts), default=0
            ),
        },
        "artifacts": {
            "evidence": evidence_path.name,
            "manifest": pathlib.Path(evidence["manifest"]).name,
            "client_log": pathlib.Path(evidence["client_log"]).name,
            "server_log": pathlib.Path(evidence["server_log"]).name,
        },
    }


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        raise SystemExit(self_test())
    if len(sys.argv) == 3 and sys.argv[1] == "--sway-content-geometry":
        print(*sway_content_geometry(json.load(sys.stdin), int(sys.argv[2])))
        raise SystemExit(0)
    if len(sys.argv) != 3:
        raise SystemExit("usage: analyze-resize-managed.py OUT EVIDENCE")
    print(json.dumps(analyze(pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])), indent=2, sort_keys=True))
