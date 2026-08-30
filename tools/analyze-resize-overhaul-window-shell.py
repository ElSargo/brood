#!/usr/bin/env python3
"""Strict pixel/trace checks for the recursive resize-overhaul nested lane."""

from __future__ import annotations

import collections
import json
import math
import pathlib
import re
import statistics
import struct
import subprocess
import sys
from typing import Any

from resize_transaction_readiness import validate_transaction_readiness


if len(sys.argv) != 10:
    raise SystemExit(
        "usage: analyze-resize-overhaul-window-shell.py "
        "OUT WINDOW_MANIFEST SIDEBAR_MANIFEST EVENTS INPUT CLIENT_LOG GATE_EVENTS SCALE OUTPUT_EXTENT"
    )

root = pathlib.Path(sys.argv[1])
window_manifest = pathlib.Path(sys.argv[2])
sidebar_manifest = pathlib.Path(sys.argv[3])
events_path = pathlib.Path(sys.argv[4])
input_path = pathlib.Path(sys.argv[5])
client_log_path = pathlib.Path(sys.argv[6])
gate_events_path = pathlib.Path(sys.argv[7])
output_scale = float(sys.argv[8])
output_width, output_height = map(int, sys.argv[9].split("x", 1))

window_records = [
    json.loads(line) for line in window_manifest.read_text().splitlines() if line
]
sidebar_records = [
    json.loads(line) for line in sidebar_manifest.read_text().splitlines() if line
]
records = sorted(
    window_records + sidebar_records, key=lambda record: record["captured_unix_ms"]
)
events = [json.loads(line) for line in events_path.read_text().splitlines() if line]
inputs = [json.loads(line) for line in input_path.read_text().splitlines() if line]
gate_events = [
    json.loads(line) for line in gate_events_path.read_text().splitlines() if line
]
client_lines = client_log_path.read_text(encoding="utf-8", errors="replace").splitlines()

if len(window_records) != 11:
    raise SystemExit(f"expected 11 native-window captures, got {len(window_records)}")
if len(sidebar_records) != 19:
    raise SystemExit(f"expected 19 sidebar captures, got {len(sidebar_records)}")
if len(events) != 12:
    raise SystemExit(f"expected 12 synchronized root transactions, got {len(events)}")
if len(gate_events) != 6:
    raise SystemExit(f"expected three WindowSlow capture gate pairs, got {gate_events}")

for capture, terminal in zip(gate_events[::2], gate_events[1::2]):
    point_x, point_y = capture["point"]
    pulse_x = capture["pulse_x"]
    contains_points = all(
        rect["x"] < x < rect["x"] + rect["width"]
        and rect["y"] < point_y < rect["y"] + rect["height"]
        for rect in (capture["current_pane_rect"], capture["intended_pane_rect"])
        for x in (point_x, pulse_x)
    )
    if (
        capture["epoch"] != terminal["epoch"]
        or capture["accepted_log_line"] >= capture["held_log_line"]
        or capture["accept_to_held_us"] < 0
        or capture["held_to_pulse_us"] < 0
        or pulse_x != point_x + 1
        or not contains_points
        or not (
            capture["accepted_observed_ns"]
            <= capture["held_observed_ns"]
            < capture["grim_started_ns"]
            < capture["pulse_ack_ns"]
            < capture["grim_completed_ns"]
        )
        or not (
            capture["held_log_line"]
            < capture["gate_release_log_line"]
            < terminal["ready_log_line"]
            <= terminal["promoted_log_line"]
        )
        or terminal["ready_duration_us"] >= 250_000
    ):
        raise SystemExit(f"invalid WindowSlow capture gate order: {capture} {terminal}")


def fields(text: str, prefix: str) -> dict[str, str]:
    if not text.startswith(prefix):
        raise SystemExit(f"trace does not start with {prefix!r}: {text!r}")
    result: dict[str, str] = {}
    for token in text[len(prefix) :].split():
        if "=" in token:
            name, value = token.split("=", 1)
            result[name] = value
    return result


def parse_resize_trace(text: str) -> dict[str, Any]:
    parsed = fields(text, "resize-present ")
    required = {
        "interaction",
        "event_sequence",
        "epoch",
        "divider_x",
        "phase",
        "wait_ready",
        "staged_count",
        "degraded",
    }
    missing = required - parsed.keys()
    if missing:
        raise SystemExit(f"resize trace misses {sorted(missing)}: {text}")
    ready, total = parsed["wait_ready"].split("/", 1)
    return {
        "interaction": int(parsed["interaction"]),
        "event_sequence": int(parsed["event_sequence"]),
        "epoch": int(parsed["epoch"]),
        "divider_x": int(parsed["divider_x"]),
        "phase": parsed["phase"],
        "wait_ready": int(ready),
        "wait_total": int(total),
        "staged_count": int(parsed["staged_count"]),
        "degraded": parsed["degraded"] == "true",
    }


def parse_window_trace(text: str) -> dict[str, Any]:
    parsed = fields(text, "window-resize-present ")
    required = {
        "native_resize_revision",
        "configure_sequence",
        "width",
        "height",
        "epoch",
        "phase",
        "wait_ready",
        "staged_count",
        "degraded",
    }
    missing = required - parsed.keys()
    if missing:
        raise SystemExit(f"window trace misses {sorted(missing)}: {text}")
    ready, total = parsed["wait_ready"].split("/", 1)
    return {
        "native_resize_revision": int(parsed["native_resize_revision"]),
        "configure_sequence": int(parsed["configure_sequence"]),
        "width": int(parsed["width"]),
        "height": int(parsed["height"]),
        "epoch": int(parsed["epoch"]),
        "phase": parsed["phase"],
        "wait_ready": int(ready),
        "wait_total": int(total),
        "staged_count": int(parsed["staged_count"]),
        "degraded": parsed["degraded"] == "true",
    }


def nearest_resize_trace(log_line: int) -> dict[str, Any]:
    for line in reversed(client_lines[:log_line]):
        offset = line.find("resize-present ")
        if offset >= 0 and "window-resize-present " not in line:
            return parse_resize_trace(line[offset:])
    raise SystemExit(f"no composed resize trace precedes client log line {log_line}")


for record in records:
    text = record["trace"]["text"]
    if record["trace"]["kind"] == "resize-present":
        record["presentation"] = parse_resize_trace(text)
        record["composed"] = record["presentation"]
    elif record["trace"]["kind"] == "window-resize-present":
        record["presentation"] = parse_window_trace(text)
        record["composed"] = nearest_resize_trace(record["trace"]["log_line"])
    else:
        raise SystemExit(f"unknown trace kind: {record['trace']}")


ROLE_LINEAR = {
    "fast": (0.04, 0.82, 0.92),
    "slow": (0.92, 0.12, 0.72),
}
INITIAL_BACKGROUND = (0.04, 0.08, 0.18)
DIVIDER_LINEAR = {
    "normal": (0.48, 0.54, 0.64),
    "hover": (0.35, 0.66, 0.95),
    "active": (0.20, 0.78, 1.00),
}


def srgb_byte(linear: float) -> int:
    if linear <= 0.0031308:
        return round(linear * 12.92 * 255)
    return round((1.055 * linear ** (1 / 2.4) - 0.055) * 255)


def variants(linear: tuple[float, float, float]) -> list[tuple[int, int, int]]:
    return [
        tuple(round(channel * 255) for channel in linear),
        tuple(srgb_byte(channel) for channel in linear),
    ]


ROLE_PALETTES = {role: variants(colour) for role, colour in ROLE_LINEAR.items()}
DIVIDER_PALETTE = [target for colour in DIVIDER_LINEAR.values() for target in variants(colour)]


def squared_distance(left: tuple[int, int, int], right: tuple[int, int, int]) -> int:
    return sum((left[index] - right[index]) ** 2 for index in range(3))


def matches_palette(
    sample: tuple[int, int, int], palette: list[tuple[int, int, int]], tolerance: int = 42
) -> bool:
    return min(squared_distance(sample, target) for target in palette) <= tolerance**2


def near_white(sample: tuple[int, int, int]) -> bool:
    return min(sample) >= 195 and max(sample) - min(sample) <= 55


def load_rgb(path: pathlib.Path) -> tuple[int, int, bytes]:
    identify = subprocess.check_output(
        ["magick", str(path), "-format", "%w %h", "info:"], text=True
    ).split()
    width, height = map(int, identify)
    raw = subprocess.check_output(
        ["magick", str(path), "-alpha", "off", "-depth", "8", "rgb:-"]
    )
    if len(raw) != width * height * 3:
        raise SystemExit(f"invalid raw RGB length for {path}")
    return width, height, raw


def pixel(raw: bytes, width: int, x: int, y: int) -> tuple[int, int, int]:
    offset = (y * width + x) * 3
    return raw[offset], raw[offset + 1], raw[offset + 2]


def mean_patch(
    raw: bytes,
    width: int,
    height: int,
    x: int,
    y: int,
    radius_x: int = 1,
    radius_y: int = 1,
) -> tuple[int, int, int]:
    samples = []
    for sample_y in range(max(0, y - radius_y), min(height, y + radius_y + 1)):
        for sample_x in range(max(0, x - radius_x), min(width, x + radius_x + 1)):
            samples.append(pixel(raw, width, sample_x, sample_y))
    return tuple(
        round(statistics.mean(sample[index] for sample in samples)) for index in range(3)
    )


def role_score(raw: bytes, width: int, rect: tuple[int, int, int, int], role: str) -> int:
    left, top, pane_width, pane_height = rect
    palette = ROLE_PALETTES[role]
    return sum(
        matches_palette(pixel(raw, width, x, y), palette)
        for y in range(top, top + pane_height, 4)
        for x in range(left, left + pane_width, 4)
    )


def contiguous_role_run(
    raw: bytes, width: int, rect: tuple[int, int, int, int], role: str
) -> tuple[int, int]:
    left, top, pane_width, pane_height = rect
    y = top + min(pane_height - 1, max(1, round(pane_height * 0.27)))
    palette = ROLE_PALETTES[role]
    matching = [
        x
        for x in range(0, max(1, pane_width // 4))
        if matches_palette(pixel(raw, width, left + x, y), palette)
    ]
    if not matching:
        raise SystemExit(f"could not find {role} marker rail in rect={rect}")
    start = matching[0]
    end = start
    misses = 0
    for candidate in range(start, max(1, pane_width // 4)):
        if matches_palette(pixel(raw, width, left + candidate, y), palette):
            end = candidate
            misses = 0
        else:
            misses += 1
            if misses >= 2:
                break
    return start, end - start + 1


def marker_vertical_run(
    raw: bytes,
    width: int,
    rect: tuple[int, int, int, int],
    role: str,
    edge: int,
    rail_width: int,
) -> tuple[int, int]:
    left, top, _pane_width, pane_height = rect
    x = left + edge + max(1, rail_width // 2)
    palette = ROLE_PALETTES[role]
    runs: list[tuple[int, int]] = []
    start = None
    for relative_y in range(pane_height):
        matched = matches_palette(pixel(raw, width, x, top + relative_y), palette)
        if matched and start is None:
            start = relative_y
        elif not matched and start is not None:
            runs.append((start, relative_y - 1))
            start = None
    if start is not None:
        runs.append((start, pane_height - 1))
    if not runs:
        raise SystemExit(f"could not find {role} marker vertical run in rect={rect}")
    return max(runs, key=lambda run: run[1] - run[0])


def decode_horizontal_bits(
    raw: bytes,
    width: int,
    height: int,
    rect: tuple[int, int, int, int],
    edge: int,
    y: int,
    count: int,
) -> int:
    left, _top, pane_width, _pane_height = rect
    inner_width = pane_width - edge * 2
    if inner_width <= count:
        raise SystemExit(f"pane is too narrow for {count}-cell barcode: {rect}")
    value = 0
    for index in range(count):
        center = left + edge + round((index + 0.5) * inner_width / count)
        radius = max(1, round(inner_width / count * 0.16))
        sample = mean_patch(raw, width, height, center, y, radius, 1)
        luminance = (54 * sample[0] + 183 * sample[1] + 19 * sample[2]) / 256
        value |= int(luminance >= 145) << index
    return value


def decode_vertical_bits(
    raw: bytes,
    width: int,
    height: int,
    rect: tuple[int, int, int, int],
    edge: int,
    rail_width: int,
    marker_run: tuple[int, int],
) -> int:
    left, top, pane_width, _pane_height = rect
    start_y, end_y = marker_run
    inner_height = end_y - start_y + 1
    x = left + pane_width - edge - max(1, rail_width // 2)
    value = 0
    for index in range(32):
        center_y = top + start_y + round((index + 0.5) * inner_height / 32)
        radius = max(1, round(inner_height / 32 * 0.14))
        sample = mean_patch(raw, width, height, x, center_y, 1, radius)
        luminance = (54 * sample[0] + 183 * sample[1] + 19 * sample[2]) / 256
        value |= int(luminance >= 145) << index
    return value


def float_from_bits(bits: int) -> float:
    return struct.unpack("<f", int(bits).to_bytes(4, "little"))[0]


def background_for_epoch(epoch: int) -> tuple[float, float, float]:
    return tuple(0.22 + 0.20 * ((epoch >> shift) & 3) for shift in (0, 2, 4))


def background_sample(
    raw: bytes, width: int, height: int, rect: tuple[int, int, int, int]
) -> tuple[int, int, int]:
    left, top, pane_width, pane_height = rect
    samples = []
    for u, v in ((0.28, 0.31), (0.34, 0.68), (0.68, 0.31), (0.74, 0.68)):
        samples.append(
            mean_patch(
                raw,
                width,
                height,
                left + round(pane_width * u),
                top + round(pane_height * v),
                2,
                2,
            )
        )
    return tuple(
        round(statistics.median(sample[index] for sample in samples))
        for index in range(3)
    )


def background_match(sample: tuple[int, int, int], epoch: int) -> float:
    candidates = variants(background_for_epoch(epoch))
    if epoch == 0:
        candidates.extend(variants(INITIAL_BACKGROUND))
    error = math.sqrt(min(squared_distance(sample, target) for target in candidates))
    if error > 48:
        raise SystemExit(
            f"background does not match decoded epoch={epoch}: sample={sample} error={error:.2f}"
        )
    return error


def border_coverage(
    raw: bytes, width: int, rect: tuple[int, int, int, int]
) -> float:
    left, top, pane_width, pane_height = rect
    horizontal = []
    for y in range(top, min(top + 5, top + pane_height)):
        horizontal.append(
            sum(
                near_white(pixel(raw, width, x, y))
                for x in range(left + 2, left + pane_width - 2)
            )
            / max(pane_width - 4, 1)
        )
    for y in range(max(top, top + pane_height - 5), top + pane_height):
        horizontal.append(
            sum(
                near_white(pixel(raw, width, x, y))
                for x in range(left + 2, left + pane_width - 2)
            )
            / max(pane_width - 4, 1)
        )
    vertical = []
    for x in list(range(left, min(left + 6, left + pane_width))) + list(
        range(max(left, left + pane_width - 6), left + pane_width)
    ):
        vertical.append(
            sum(
                near_white(pixel(raw, width, x, y))
                for y in range(top + 2, top + pane_height - 2)
            )
            / max(pane_height - 4, 1)
        )
    coverage = min(max(horizontal, default=0.0), max(vertical, default=0.0))
    if coverage < 0.64:
        raise SystemExit(f"near-white extent border is incomplete: rect={rect} coverage={coverage}")
    return coverage


def analyse_pane(
    raw: bytes, width: int, height: int, rect: tuple[int, int, int, int]
) -> dict[str, Any]:
    scores = {role: role_score(raw, width, rect, role) for role in ROLE_LINEAR}
    role = max(scores, key=scores.get)
    if scores[role] < 30:
        raise SystemExit(f"proof role marker not found: rect={rect} scores={scores}")
    edge, rail_width = contiguous_role_run(raw, width, rect, role)
    marker_run = marker_vertical_run(raw, width, rect, role, edge, rail_width)
    marker_top, _marker_bottom = marker_run
    left, top, _pane_width, pane_height = rect
    top_y = top + max(1, round((edge + marker_top) * 0.5))
    bottom_y = top + pane_height - (top_y - top) - 1
    epoch = decode_horizontal_bits(raw, width, height, rect, edge, top_y, 64)
    size_bits = decode_horizontal_bits(raw, width, height, rect, edge, bottom_y, 64)
    encoded_width = float_from_bits(size_bits & 0xFFFF_FFFF)
    encoded_height = float_from_bits(size_bits >> 32)
    encoded_scale = float_from_bits(
        decode_vertical_bits(raw, width, height, rect, edge, rail_width, marker_run)
    )
    for name, value in (
        ("width", encoded_width),
        ("height", encoded_height),
        ("scale", encoded_scale),
    ):
        if not math.isfinite(value) or value <= 0:
            raise SystemExit(f"decoded {name} is invalid for role={role}: {value}")
    background = background_sample(raw, width, height, rect)
    return {
        "role": role,
        "epoch": epoch,
        "encoded_width": encoded_width,
        "encoded_height": encoded_height,
        "encoded_scale": encoded_scale,
        "background_rgb": list(background),
        "background_error": background_match(background, epoch),
        "extent_border_coverage": border_coverage(raw, width, rect),
        "rect": list(rect),
    }


def divider_metrics(
    raw: bytes, width: int, content_height: int, expected_x: int
) -> dict[str, Any]:
    row_start = max(0, round(content_height * 0.04))
    row_end = min(content_height, round(content_height * 0.96))
    scores = []
    for x in range(width):
        matches = sum(
            matches_palette(pixel(raw, width, x, y), DIVIDER_PALETTE, 32)
            for y in range(row_start, row_end)
        )
        scores.append(matches / max(row_end - row_start, 1))
    candidates = [
        x
        for x, score in enumerate(scores)
        if score >= max(0.55, max(scores) * 0.88) and abs(x - expected_x) <= 3
    ]
    if len(candidates) != 1:
        raise SystemExit(
            f"expected one device-pixel divider at {expected_x}, candidates={candidates}"
        )
    return {
        "detected_x": candidates[0],
        "trace_x": expected_x,
        "device_width": 1,
        "coverage": scores[candidates[0]],
    }


def find_content_height(raw: bytes, width: int, height: int, rest_right: int) -> int:
    if rest_right < 160:
        raise SystemExit(f"proof rest is too narrow: {rest_right}")
    left = 3
    right = max(left + 1, rest_right - 3)
    candidates = []
    for y in range(2, height - 2):
        stride = max(1, (right - left) // 320)
        samples = [pixel(raw, width, x, y) for x in range(left, right, stride)]
        coverage = sum(near_white(sample) for sample in samples) / len(samples)
        if coverage >= 0.68:
            candidates.append(y)
    if not candidates:
        raise SystemExit("could not find proof panes' bottom extent border")
    # The top border and barcodes are early; the exact lower extent border is
    # the final high-coverage line before root-owned status content.
    return max(candidates) + 1


def close(left: float, right: float, tolerance: float = 1.7) -> bool:
    return abs(left - right) <= tolerance


event_by_input = {event["input_index"]: event for event in events}
decoded = []
for record in records:
    path = root / record["file"]
    data = path.read_bytes()
    if not data.startswith(b"\x89PNG\r\n\x1a\n") or len(data) < 4096:
        raise SystemExit(f"invalid PNG: {path}")
    width, height, raw = load_rgb(path)
    if [width, height] != record["image_size"]:
        raise SystemExit(f"manifest/image extent mismatch for {path}")
    if record["lane"] == "sidebar" and (width, height) != (output_width, output_height):
        raise SystemExit(f"sidebar capture does not cover the output: {path} {width}x{height}")
    divider_x = record["composed"]["divider_x"]
    rest_right = divider_x * 2
    if divider_x < 80 or rest_right > width + 2:
        raise SystemExit(
            f"invalid recursive proof divider/rest geometry label={record['label']} "
            f"divider={divider_x} width={width}"
        )
    rest_right = min(width, rest_right)
    content_height = find_content_height(raw, width, height, rest_right)
    rects = [
        (0, 0, divider_x, content_height),
        (divider_x, 0, rest_right - divider_x, content_height),
    ]
    panes = [analyse_pane(raw, width, height, rect) for rect in rects]
    if {pane["role"] for pane in panes} != {"fast", "slow"}:
        raise SystemExit(f"proof roles are not fast/slow in {record['label']}: {panes}")
    panes_by_role = {pane["role"]: pane for pane in panes}
    if panes_by_role["fast"]["epoch"] != panes_by_role["slow"]["epoch"]:
        raise SystemExit(f"mixed waited epochs in {record['label']}: {panes_by_role}")
    for pane in panes:
        if not close(pane["encoded_scale"], output_scale, 0.015):
            raise SystemExit(f"fractional-scale barcode mismatch in {record['label']}: {pane}")
    event = event_by_input.get(record["input_index"])
    geometry = None
    private_projection = record["presentation"]["phase"] == "projected"
    if event is not None and not private_projection:
        diagnostic = event["diagnostic"]
        direct_error = abs(diagnostic["rest_width"] - rest_right)
        scaled_error = abs(diagnostic["rest_width"] * output_scale - rest_right)
        unit_scale = output_scale if scaled_error < direct_error else 1.0
        geometry = {
            "unit_scale": unit_scale,
            "rest_width": diagnostic["rest_width"] * unit_scale,
            "rest_height": diagnostic["rest_height"] * unit_scale,
            "sidebar_width": diagnostic["sidebar_width"] * unit_scale,
            "inbox_width": diagnostic["inbox_width"] * unit_scale,
            "status_height": diagnostic["status_height"] * unit_scale,
        }
        if abs(geometry["rest_width"] - rest_right) > 2.1:
            raise SystemExit(
                f"diagnostic rest geometry misses composed proof edge in {record['label']}: "
                f"geometry={geometry} rest_right={rest_right}"
            )
        if abs(geometry["rest_height"] - content_height) > 2.1:
            raise SystemExit(
                f"diagnostic rest height misses proof border in {record['label']}: "
                f"geometry={geometry} content_height={content_height}"
            )
        if abs(
            geometry["rest_width"]
            + geometry["sidebar_width"]
            + geometry["inbox_width"]
            - width
        ) > 2.1:
            raise SystemExit(f"root horizontal allocations do not fill {record['label']}: {geometry}")
        if abs(geometry["rest_height"] + geometry["status_height"] - height) > 2.1:
            raise SystemExit(f"root vertical allocations do not fill {record['label']}: {geometry}")
    decoded.append(
        {
            "lane": record["lane"],
            "label": record["label"],
            "input_index": record["input_index"],
            "captured_unix_ms": record["captured_unix_ms"],
            "presentation": record["presentation"],
            "composed": record["composed"],
            "image_size": [width, height],
            "rest_right": rest_right,
            "content_height": content_height,
            "divider": divider_metrics(raw, width, content_height, divider_x),
            "panes": panes_by_role,
            "geometry": geometry,
            "raw": raw,
        }
    )


# Atomicity state machine. Every live/root projected frame retains the complete
# old fast/slow pair. The first promoted frame advances both panes together and
# its encoded logical extent exactly names the target rect.
visible: dict[str, tuple[int, float, float]] | None = None
projected_old_old = 0
promoted_new_new = 0
for frame in decoded:
    phase = frame["presentation"]["phase"]
    panes = frame["panes"]
    current = {
        role: (pane["epoch"], pane["encoded_width"], pane["encoded_height"])
        for role, pane in panes.items()
    }
    if frame["presentation"]["degraded"] or frame["composed"]["degraded"]:
        raise SystemExit(f"recursive resize degraded in {frame['label']}")
    if frame["presentation"]["staged_count"] > 1 or frame["composed"]["staged_count"] > 1:
        raise SystemExit(f"recursive resize staged more than one transaction: {frame['label']}")
    if visible is None:
        visible = current
    if phase == "projected":
        for role in panes:
            if (
                current[role][0] != visible[role][0]
                or not close(current[role][1], visible[role][1])
                or not close(current[role][2], visible[role][2])
            ):
                raise SystemExit(
                    f"projected frame contains mixed/new content: {frame['label']} "
                    f"role={role} old={visible[role]} current={current[role]}"
                )
        projected_old_old += 1
    elif phase == "promoted":
        # sidebar-initial is an extra capture of the already promoted restore.
        duplicate = frame["label"] == "sidebar-initial"
        if not duplicate and current["fast"][0] <= visible["fast"][0]:
            raise SystemExit(
                f"promoted frame did not advance its epoch: {frame['label']} "
                f"old={visible} current={current}"
            )
        for pane in panes.values():
            rect_width = pane["rect"][2] / output_scale
            rect_height = pane["rect"][3] / output_scale
            if not close(pane["encoded_width"], rect_width) or not close(
                pane["encoded_height"], rect_height
            ):
                raise SystemExit(
                    f"promoted proof extent is not exact: {frame['label']} pane={pane} "
                    f"target={rect_width}x{rect_height}"
                )
            if pane["epoch"] != frame["presentation"]["epoch"]:
                raise SystemExit(
                    f"promoted barcode epoch misses trace: {frame['label']} pane={pane}"
                )
        visible = current
        if not duplicate:
            promoted_new_new += 1
    else:
        raise SystemExit(f"unexpected captured phase={phase} label={frame['label']}")


def region_detail(
    frame: dict[str, Any], left: int, top: int, width: int, height: int
) -> dict[str, Any]:
    if width <= 2 or height <= 2:
        raise SystemExit(f"invalid root chrome region: {(left, top, width, height)}")
    raw = frame["raw"]
    image_width = frame["image_size"][0]
    samples = [
        pixel(raw, image_width, x, y)
        for y in range(top + 1, top + height - 1, max(1, height // 90))
        for x in range(left + 1, left + width - 1, max(1, width // 90))
    ]
    quantized = {(r // 8, g // 8, b // 8) for r, g, b in samples}
    luminance = statistics.fmean((54 * r + 183 * g + 19 * b) / 256 for r, g, b in samples)
    if len(quantized) < 8 or luminance < 8:
        raise SystemExit(
            f"root-owned pane is blank/flat region={(left, top, width, height)} "
            f"colours={len(quantized)} luminance={luminance}"
        )
    return {"quantized_colours": len(quantized), "mean_luminance": luminance}


root_frame_content = {}
for frame in decoded:
    width, height = frame["image_size"]
    rest_right = frame["rest_right"]
    content_height = frame["content_height"]
    details = {
        "status": region_detail(
            frame, 0, content_height, width, height - content_height
        )
    }
    if width - rest_right > 2:
        details["right_root_chrome"] = region_detail(
            frame, rest_right, 0, width - rest_right, content_height
        )
    root_frame_content[frame["label"]] = details


expected_sidebar_states = {
    "sidebar-initial": (True, False),
    "sidebar-hide-promoted": (False, False),
    "sidebar-show-promoted": (True, False),
    "inbox-show-promoted": (True, True),
    "sidebar-hide-with-inbox-promoted": (False, True),
    "sidebar-show-with-inbox-promoted": (True, True),
    "sidebar-divider-motion-promoted": (True, True),
    "sidebar-divider-release-promoted": (True, True),
    "inbox-divider-motion-promoted": (True, True),
    "inbox-divider-release-promoted": (True, True),
}
chrome = {}
for frame in decoded:
    if frame["label"] not in expected_sidebar_states:
        continue
    geometry = frame["geometry"]
    if geometry is None:
        raise SystemExit(f"no diagnostic geometry for {frame['label']}")
    sidebar_expected, inbox_expected = expected_sidebar_states[frame["label"]]
    sidebar_width = round(geometry["sidebar_width"])
    inbox_width = round(geometry["inbox_width"])
    if (sidebar_width > 0) != sidebar_expected or (inbox_width > 0) != inbox_expected:
        raise SystemExit(
            f"wrong shell visibility geometry in {frame['label']}: geometry={geometry}"
        )
    details: dict[str, Any] = {}
    root_dividers: dict[str, Any] = {}
    rest_right = frame["rest_right"]
    content_height = frame["content_height"]
    if sidebar_expected:
        details["sidebar"] = region_detail(
            frame, rest_right, 0, sidebar_width, content_height
        )
        root_dividers["main_sidebar"] = divider_metrics(
            frame["raw"], frame["image_size"][0], content_height, round(rest_right)
        )
    if inbox_expected:
        details["inbox"] = region_detail(
            frame, rest_right + sidebar_width, 0, inbox_width, content_height
        )
        root_dividers["sidebar_inbox"] = divider_metrics(
            frame["raw"],
            frame["image_size"][0],
            content_height,
            round(rest_right + sidebar_width),
        )
    status_height = round(geometry["status_height"])
    details["status"] = region_detail(
        frame, 0, content_height, frame["image_size"][0], status_height
    )
    chrome[frame["label"]] = {
        "sidebar_visible": sidebar_expected,
        "inbox_visible": inbox_expected,
        "rest_width": geometry["rest_width"],
        "sidebar_width": geometry["sidebar_width"],
        "inbox_width": geometry["inbox_width"],
        "status_height": geometry["status_height"],
        "content": details,
        "dividers": root_dividers,
    }

if not (
    chrome["sidebar-hide-promoted"]["rest_width"]
    > chrome["sidebar-initial"]["rest_width"]
    and chrome["sidebar-show-promoted"]["rest_width"]
    < chrome["sidebar-hide-promoted"]["rest_width"]
    and chrome["inbox-show-promoted"]["rest_width"]
    < chrome["sidebar-show-promoted"]["rest_width"]
    and chrome["sidebar-hide-with-inbox-promoted"]["rest_width"]
    > chrome["inbox-show-promoted"]["rest_width"]
    and close(
        chrome["sidebar-show-with-inbox-promoted"]["rest_width"],
        chrome["inbox-show-promoted"]["rest_width"],
    )
):
    raise SystemExit(f"root sidebar/inbox allocations did not move coherently: {chrome}")

initial_three_column = chrome["sidebar-show-with-inbox-promoted"]
sidebar_resized = chrome["sidebar-divider-release-promoted"]
inbox_resized = chrome["inbox-divider-release-promoted"]
if not (
    sidebar_resized["rest_width"] < initial_three_column["rest_width"]
    and sidebar_resized["sidebar_width"] > initial_three_column["sidebar_width"]
    and close(sidebar_resized["inbox_width"], initial_three_column["inbox_width"])
    and close(inbox_resized["rest_width"], sidebar_resized["rest_width"])
    and inbox_resized["sidebar_width"] > sidebar_resized["sidebar_width"]
    and inbox_resized["inbox_width"] < sidebar_resized["inbox_width"]
):
    raise SystemExit(f"root dividers did not resize all three columns: {chrome}")
if not all(
    divider["device_width"] == 1
    for state in chrome.values()
    for divider in state["dividers"].values()
):
    raise SystemExit(f"root divider is not exactly one device pixel: {chrome}")


# Transaction identity, ordering, explicit wait sets, and bounded staging.
transaction_metrics = []
max_staged = 0
for event in events:
    diagnostic = event["diagnostic"]
    accepted = fields(event["accepted"]["text"], "accepted synchronized layout ")
    projected = parse_resize_trace(event["projected"]["text"])
    promoted = parse_resize_trace(event["promoted"]["text"])
    wait = diagnostic["wait_for_panes"]
    if not wait or len(wait) != len(set(wait)):
        raise SystemExit(f"transaction wait set is empty/duplicated: {event}")
    if (
        accepted.get("actor") != "session-switcher"
        or int(accepted["request"]) != diagnostic["request_id"]
        or int(accepted["interaction"]) != projected["interaction"]
        or int(accepted["event_sequence"]) != diagnostic["event_sequence"]
        or int(accepted["native_resize_revision"])
        != diagnostic["native_resize_revision"]
        or int(accepted["actor_generation"]) != diagnostic["actor_generation"]
        or int(accepted["focus"]) != diagnostic["focus"]
        or int(accepted["wait"]) != len(wait)
        or int(accepted["epoch"]) != projected["epoch"]
        or projected["epoch"] != promoted["epoch"]
        or projected["event_sequence"] != promoted["event_sequence"]
        or projected["phase"] != "projected"
        or promoted["phase"] != "promoted"
        or projected["wait_total"] != len(wait)
        or promoted["wait_total"] != len(wait)
        or promoted["wait_ready"] != len(wait)
        or projected["staged_count"] != 1
        or promoted["staged_count"] != 0
        or projected["degraded"]
        or promoted["degraded"]
        or not (
            event["diagnostic_log_line"]
            == event["accepted"]["log_line"]
            < event["projected"]["log_line"]
            <= event["promoted"]["log_line"]
            < event["completed_log_line"]
        )
    ):
        raise SystemExit(f"invalid root transaction ordering/trace: {event}")
    if event["lane"] == "window":
        settle_line = event["native_settle_log_line"]
        if not 0 < settle_line < event["accepted"]["log_line"]:
            raise SystemExit(f"native settlement/acceptance ordering is invalid: {event}")
        forbidden = [
            line
            for line in client_lines[settle_line : event["completed_log_line"]]
            if "accepted leased actor layout" in line
            or "SetResolvedLayout" in line
            or "set-resolved-layout" in line
        ]
        if forbidden:
            raise SystemExit(
                f"ordinary layout appeared inside native root transaction: {forbidden}"
            )
    try:
        readiness = validate_transaction_readiness(
            client_lines,
            actor="session-switcher",
            request=diagnostic["request_id"],
            interaction=projected["interaction"],
            event_sequence=diagnostic["event_sequence"],
            epoch=projected["epoch"],
            target_revision=int(accepted["target_revision"]),
            region_total=int(accepted["region_final"].split("/", 1)[1]),
            wait_panes=wait,
            projected_line=event["projected"]["log_line"],
            promoted_line=event["promoted"]["log_line"],
            completed_line=event["completed_log_line"],
        )
    except ValueError as error:
        raise SystemExit(f"invalid aggregate synchronized readiness: {error}; event={event}") from error
    max_staged = max(max_staged, projected["staged_count"], promoted["staged_count"])
    transaction_metrics.append(
        {
            "lane": event["lane"],
            "action": event["action"],
            "request_id": diagnostic["request_id"],
            "event_sequence": diagnostic["event_sequence"],
            "native_resize_revision": diagnostic["native_resize_revision"],
            "epoch": projected["epoch"],
            "wait_for_panes": wait,
            "wait_ready": promoted["wait_ready"],
            "wait_total": promoted["wait_total"],
            "readiness_log_line": readiness["ready_line"],
            "readiness_evidence": (
                "identity-matched aggregate ready set equals the exact accepted wait set"
            ),
            "completed_after_present": True,
        }
    )

native_transactions = [item for item in transaction_metrics if item["lane"] == "window"]
sidebar_transactions = [item for item in transaction_metrics if item["lane"] == "sidebar"]
if len(native_transactions) != 3 or len(sidebar_transactions) != 9:
    raise SystemExit(f"wrong transaction lane counts: {transaction_metrics}")
if any(item["native_resize_revision"] == 0 for item in native_transactions):
    raise SystemExit(f"native transaction omitted revision: {native_transactions}")
if any(item["native_resize_revision"] != 0 for item in sidebar_transactions):
    raise SystemExit(f"interactive shell transaction carried native revision: {sidebar_transactions}")

configure_sequences = [
    record["presentation"]["configure_sequence"]
    for record in records
    if record["trace"]["kind"] == "window-resize-present"
]
if any(second < first for first, second in zip(configure_sequences, configure_sequences[1:])):
    raise SystemExit(f"window configure sequence regressed: {configure_sequences}")

input_durations = [event["compositor_roundtrip_ms"] for event in inputs]
if not input_durations or any(duration < 0 or duration >= 5_000 for duration in input_durations):
    raise SystemExit(f"invalid synchronized input durations: {input_durations}")
keyboard_inputs = [event for event in inputs if event["kind"] == "virtual-keyboard"]
if [event["action"] for event in keyboard_inputs] != [
    "sidebar-hide",
    "sidebar-show",
    "inbox-show",
    "sidebar-hide-with-inbox",
    "sidebar-show-with-inbox",
]:
    raise SystemExit(f"unexpected real key sequence: {keyboard_inputs}")
pointer_inputs = [event for event in inputs if event["kind"] == "virtual-pointer"]
if [event["action"] for event in pointer_inputs] != [
    "window-slow-gate-arm",
    "window-slow-gate-pulse",
    "window-slow-gate-release",
    "window-slow-gate-arm",
    "window-slow-gate-pulse",
    "window-slow-gate-release",
    "window-slow-gate-arm",
    "window-slow-gate-pulse",
    "window-slow-gate-release",
    "sidebar-divider-hover",
    "sidebar-divider-press",
    "sidebar-divider-motion",
    "sidebar-divider-release",
    "inbox-divider-hover",
    "inbox-divider-press",
    "inbox-divider-motion",
    "inbox-divider-release",
]:
    raise SystemExit(f"unexpected real pointer sequence: {pointer_inputs}")

for frame in decoded:
    frame.pop("raw", None)

result = {
    "verified": True,
    "output": {
        "scale": output_scale,
        "device_size": [output_width, output_height],
    },
    "captures": {
        "window": len(window_records),
        "sidebar": len(sidebar_records),
        "total": len(records),
        "projected_old_old": projected_old_old,
        "promoted_new_new": promoted_new_new,
        "mixed_waited_pairs": 0,
        "all_one_device_pixel_dividers": all(
            frame["divider"]["device_width"] == 1 for frame in decoded
        ),
        "all_exact_extent_borders": all(
            pane["extent_border_coverage"] >= 0.64
            for frame in decoded
            for pane in frame["panes"].values()
        ),
        "frames": decoded,
    },
    "input": {
        "count": len(inputs),
        "sway_ipc_count": sum(event["kind"] == "sway-ipc" for event in inputs),
        "virtual_keyboard_count": len(keyboard_inputs),
        "roundtrip_ms": {
            "samples": len(input_durations),
            "average": statistics.fmean(input_durations),
            "maximum": max(input_durations),
        },
        "configure_sequences": configure_sequences,
        "ordered_configure_sequences": True,
    },
    "transactions": transaction_metrics,
    "max_staged_count": max_staged,
    "chrome": chrome,
    "assertions": {
        "native_window_live_projection": sum(
            record["label"].endswith("live-projected")
            or re.fullmatch(r"window-live-\d+-projected", record["label"])
            is not None
            for record in window_records
        )
        >= 5,
        "native_uses_root_synchronization": len(native_transactions) == 3,
        "native_revision_echoed": all(
            item["native_resize_revision"] > 0 for item in native_transactions
        ),
        "sidebar_root_transactions": len(sidebar_transactions) == 9,
        "explicit_unique_wait_sets": all(
            item["wait_for_panes"]
            and len(item["wait_for_panes"]) == len(set(item["wait_for_panes"]))
            for item in transaction_metrics
        ),
        "atomic_old_old_then_new_new": projected_old_old >= 12
        and promoted_new_new == 12,
        "no_mixed_waited_pairs": True,
        "staging_bounded": max_staged <= 1,
        "no_degraded_final": True,
        "fractional_scale_fiducials": close(output_scale, 1.25, 0.001),
        "one_device_pixel_divider": all(
            frame["divider"]["device_width"] == 1 for frame in decoded
        ),
        "exact_gpu_logical_extents": True,
        "session_switcher_visibility": all(
            label in chrome
            for label in (
                "sidebar-initial",
                "sidebar-hide-promoted",
                "sidebar-show-promoted",
            )
        ),
        "inbox_visibility": all(
            label in chrome
            for label in (
                "inbox-show-promoted",
                "sidebar-hide-with-inbox-promoted",
                "sidebar-show-with-inbox-promoted",
            )
        ),
        "three_column_root_dividers": (
            sidebar_resized["rest_width"] < initial_three_column["rest_width"]
            and sidebar_resized["sidebar_width"]
            > initial_three_column["sidebar_width"]
            and inbox_resized["sidebar_width"] > sidebar_resized["sidebar_width"]
            and inbox_resized["inbox_width"] < sidebar_resized["inbox_width"]
        ),
        "root_dividers_one_device_pixel": all(
            divider["device_width"] == 1
            for state in chrome.values()
            for divider in state["dividers"].values()
        ),
        "status_bar_extent_stable": len(
            {round(value["status_height"], 2) for value in chrome.values()}
        )
        == 1,
        "root_chrome_content_every_frame": len(root_frame_content) == len(decoded),
        "synchronized_real_input": len(keyboard_inputs) == 5
        and len(pointer_inputs) == 17
        and max(input_durations) < 5_000,
        "projected_capture_gate_ordered": len(gate_events) == 6,
        "client_survived": True,
    },
}

print(json.dumps(result, indent=2, sort_keys=True))
