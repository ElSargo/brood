#!/usr/bin/env python3
"""Strict trace and pixel checks for the three-level resize-region proof."""

from __future__ import annotations

import hashlib
import json
import math
import pathlib
import re
import statistics
import struct
import subprocess
import sys
from typing import Any


if len(sys.argv) != 6:
    raise SystemExit(
        "usage: analyze-resize-region-tree.py OUT MANIFEST EVIDENCE CLIENT_LOG SCALE"
    )

root = pathlib.Path(sys.argv[1])
manifest_path = pathlib.Path(sys.argv[2])
evidence = json.loads(pathlib.Path(sys.argv[3]).read_text())
log_path = pathlib.Path(sys.argv[4])
scale = float(sys.argv[5])
lines = log_path.read_text(encoding="utf-8", errors="replace").splitlines()
frames = {
    record["label"]: record
    for record in (
        json.loads(line) for line in manifest_path.read_text().splitlines() if line
    )
}


def line(index: int) -> str:
    if index <= 0 or index > len(lines):
        raise SystemExit(f"invalid evidence line {index}")
    return lines[index - 1]


def fields(text: str) -> dict[str, str]:
    result: dict[str, str] = {}
    for token in text.split():
        if "=" in token:
            name, value = token.split("=", 1)
            result[name] = value.rstrip(",")
    return result


def first_line(pattern: str, after: int = 0) -> tuple[int, str]:
    regex = re.compile(pattern)
    for index, text in enumerate(lines[after:], after + 1):
        if regex.search(text):
            return index, text
    raise SystemExit(f"missing log pattern after line {after}: {pattern}")


def all_lines(pattern: str) -> list[tuple[int, str]]:
    regex = re.compile(pattern)
    return [
        (index, text)
        for index, text in enumerate(lines, 1)
        if regex.search(text)
    ]


def parse_rects(text: str) -> dict[int, tuple[int, int, int, int]]:
    match = re.search(r"(?:^| )target_rects=\[([^]]*)\]", text)
    if match is None:
        raise SystemExit(f"accepted transaction has no target_rects: {text}")
    rects: dict[int, tuple[int, int, int, int]] = {}
    for encoded in filter(None, match.group(1).split(",")):
        pane, x, y, width, height = map(int, encoded.split(":"))
        rects[pane] = (x, y, width, height)
    return rects


def parse_json_diagnostic(text: str) -> dict[str, Any]:
    start = text.find('{"kind":')
    if start < 0:
        raise SystemExit(f"diagnostic has no JSON body: {text}")
    try:
        return json.loads(text[start:])
    except json.JSONDecodeError as error:
        raise SystemExit(f"invalid proof diagnostic: {error}: {text}") from error


def diagnostic(kind: str, epoch: int, mode: str) -> tuple[int, dict[str, Any]]:
    for index, text in enumerate(lines, 1):
        if f'"kind":"{kind}"' not in text or f'"epoch":{epoch}' not in text:
            continue
        body = parse_json_diagnostic(text)
        if body.get("mode") == mode:
            return index, body
    raise SystemExit(f"missing diagnostic kind={kind} epoch={epoch} mode={mode}")


def load_rgb(path: pathlib.Path) -> tuple[int, int, bytes]:
    dimensions = subprocess.check_output(
        ["magick", str(path), "-format", "%w %h", "info:"], text=True
    ).split()
    width, height = map(int, dimensions)
    raw = subprocess.check_output(
        ["magick", str(path), "-alpha", "off", "-depth", "8", "rgb:-"]
    )
    if len(raw) != width * height * 3:
        raise SystemExit(f"invalid RGB length for {path}")
    return width, height, raw


def pixel(raw: bytes, width: int, x: int, y: int) -> tuple[int, int, int]:
    offset = (y * width + x) * 3
    return raw[offset], raw[offset + 1], raw[offset + 2]


def near_white(sample: tuple[int, int, int]) -> bool:
    return min(sample) >= 180 and max(sample) - min(sample) <= 65


ROLE_LINEAR = {
    "middle-layout": (0.04, 0.82, 0.92),
    "leaf-layout": (0.92, 0.12, 0.72),
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


def distance(left: tuple[int, int, int], right: tuple[int, int, int]) -> int:
    return sum((left[index] - right[index]) ** 2 for index in range(3))


def matches_role(sample: tuple[int, int, int], role: str) -> bool:
    return min(distance(sample, target) for target in ROLE_PALETTES[role]) <= 42**2


def mean_patch(
    raw: bytes, width: int, height: int, x: int, y: int, rx: int, ry: int
) -> tuple[int, int, int]:
    samples = [
        pixel(raw, width, sx, sy)
        for sy in range(max(0, y - ry), min(height, y + ry + 1))
        for sx in range(max(0, x - rx), min(width, x + rx + 1))
    ]
    return tuple(
        round(statistics.mean(sample[index] for sample in samples)) for index in range(3)
    )


def marker_geometry(
    raw: bytes,
    image_width: int,
    rect: tuple[int, int, int, int],
    role: str,
) -> tuple[int, int, tuple[int, int]]:
    left, top, width, height = rect
    y = top + min(height - 1, max(1, round(height * 0.27)))
    matching = [matches_role(pixel(raw, image_width, left + x, y), role) for x in range(width)]
    runs: list[tuple[int, int]] = []
    start = None
    for x, match in enumerate(matching):
        if match and start is None:
            start = x
        elif not match and start is not None:
            runs.append((start, x - 1))
            start = None
    if start is not None:
        runs.append((start, width - 1))
    if not runs:
        raise SystemExit(f"no {role} fiducial in rect={rect}")
    edge_run = min(runs, key=lambda run: run[0])
    edge = edge_run[0]
    rail = edge_run[1] - edge_run[0] + 1
    x = left + edge + max(1, rail // 2)
    vertical = [matches_role(pixel(raw, image_width, x, top + y), role) for y in range(height)]
    vertical_runs: list[tuple[int, int]] = []
    start = None
    for y, match in enumerate(vertical):
        if match and start is None:
            start = y
        elif not match and start is not None:
            vertical_runs.append((start, y - 1))
            start = None
    if start is not None:
        vertical_runs.append((start, height - 1))
    if not vertical_runs:
        raise SystemExit(f"no vertical {role} fiducial in rect={rect}")
    return edge, rail, max(vertical_runs, key=lambda run: run[1] - run[0])


def decode_horizontal(
    raw: bytes,
    image_width: int,
    image_height: int,
    rect: tuple[int, int, int, int],
    edge: int,
    y: int,
) -> int:
    left, _top, width, _height = rect
    inner = width - edge * 2
    if inner <= 64:
        raise SystemExit(f"pane is too narrow for epoch barcode: {rect}")
    value = 0
    for bit in range(64):
        x = left + edge + round((bit + 0.5) * inner / 64)
        radius = max(1, round(inner / 64 * 0.16))
        sample = mean_patch(raw, image_width, image_height, x, y, radius, 1)
        luminance = (54 * sample[0] + 183 * sample[1] + 19 * sample[2]) / 256
        value |= int(luminance >= 145) << bit
    return value


def decode_vertical(
    raw: bytes,
    image_width: int,
    image_height: int,
    rect: tuple[int, int, int, int],
    edge: int,
    rail: int,
    marker_run: tuple[int, int],
) -> int:
    left, top, width, _height = rect
    start, end = marker_run
    inner = end - start + 1
    x = left + width - edge - max(1, rail // 2)
    value = 0
    for bit in range(32):
        y = top + start + round((bit + 0.5) * inner / 32)
        radius = max(1, round(inner / 32 * 0.14))
        sample = mean_patch(raw, image_width, image_height, x, y, 1, radius)
        luminance = (54 * sample[0] + 183 * sample[1] + 19 * sample[2]) / 256
        value |= int(luminance >= 145) << bit
    return value


def float_bits(value: int) -> float:
    return struct.unpack("<f", int(value).to_bytes(4, "little"))[0]


def analyse_pane(
    raw: bytes,
    image_width: int,
    image_height: int,
    rect: tuple[int, int, int, int],
    role: str,
) -> dict[str, Any]:
    edge, rail, marker_run = marker_geometry(raw, image_width, rect, role)
    _left, top, _width, height = rect
    top_y = top + max(1, round((edge + marker_run[0]) * 0.5))
    bottom_y = top + height - (top_y - top) - 1
    epoch = decode_horizontal(raw, image_width, image_height, rect, edge, top_y)
    size_bits = decode_horizontal(raw, image_width, image_height, rect, edge, bottom_y)
    encoded_width = float_bits(size_bits & 0xFFFF_FFFF)
    encoded_height = float_bits(size_bits >> 32)
    encoded_scale = float_bits(
        decode_vertical(raw, image_width, image_height, rect, edge, rail, marker_run)
    )
    if not all(
        math.isfinite(value) and value > 0
        for value in (encoded_width, encoded_height, encoded_scale)
    ):
        raise SystemExit(
            f"invalid {role} fiducial values epoch={epoch} "
            f"size={encoded_width}x{encoded_height} scale={encoded_scale}"
        )
    return {
        "epoch": epoch,
        "width": encoded_width,
        "height": encoded_height,
        "scale": encoded_scale,
    }


def frame_epochs(
    label: str, rects: dict[int, tuple[int, int, int, int]], panes: list[int]
) -> tuple[dict[int, dict[str, Any]], str]:
    record = frames.get(label)
    if record is None:
        raise SystemExit(f"missing frame {label}")
    path = root / record["file"]
    width, height, raw = load_rgb(path)
    roles = {panes[0]: "middle-layout", panes[1]: "leaf-layout"}
    decoded = {
        pane: analyse_pane(raw, width, height, rects[pane], roles[pane]) for pane in panes
    }
    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    return decoded, digest


topology = evidence["topology"]
supersession = evidence["supersession"]
release = evidence["release"]
timeout = evidence["timeout"]
final_release = evidence["final_release"]
root_pane = int(topology["panes"]["root"])
middle_pane = int(topology["panes"]["middle"])
leaf_pane = int(topology["panes"]["leaf"])

root_grant = line(topology["root_grant"])
middle_grant = line(topology["middle_grant"])
leaf_grant = line(topology["leaf_grant"])
root_lease = int(re.search(r"lease_generation=(\d+)", root_grant).group(1))
middle_match = re.search(r"parent=(\d+) child=(\d+)", middle_grant)
leaf_match = re.search(r"parent=(\d+) child=(\d+)", leaf_grant)
if middle_match is None or leaf_match is None:
    raise SystemExit("could not decode three-level lease grants")
root_lease_id, middle_lease_id = map(int, middle_match.groups())
middle_parent_id, leaf_lease_id = map(int, leaf_match.groups())

first = supersession["first"]
second = supersession["second"]
second_epoch = int(second["epoch"])
accepted_text = line(second["accepted"])
accepted_fields = fields(accepted_text)
target_rects = parse_rects(accepted_text)
for pane in (root_pane, middle_pane, leaf_pane):
    if pane not in target_rects:
        raise SystemExit(f"normal target has no pane {pane}: {accepted_text}")

root_notify_line, root_notify_text = first_line(
    rf"layout-region pane-notified epoch={second_epoch} pane={root_pane} .*owner_lease={root_lease_id}"
)
middle_accept_line, middle_accept_text = first_line(
    rf"layout-region response-accepted epoch={second_epoch} .*actor=workspace .*kind=fragment"
)
middle_notify_line, middle_notify_text = first_line(
    rf"layout-region pane-notified epoch={second_epoch} pane={middle_pane} .*owner_lease={middle_lease_id}"
)
leaf_accept_line, leaf_accept_text = first_line(
    rf"layout-region response-accepted epoch={second_epoch} .*actor=resize-proof-leaf-layout .*kind=fragment"
)
leaf_notify_line, leaf_notify_text = first_line(
    rf"layout-region pane-notified epoch={second_epoch} pane={leaf_pane} .*owner_lease={leaf_lease_id}"
)

middle_issue_line, middle_issue_text = first_line(
    rf"layout-region issued epoch={second_epoch} lease={middle_lease_id} "
)
leaf_issue_line, leaf_issue_text = first_line(
    rf"layout-region issued epoch={second_epoch} lease={leaf_lease_id} "
)
middle_received_line, middle_received = diagnostic(
    "resize-proof-region-received", second_epoch, "middle-layout"
)
leaf_received_line, leaf_received = diagnostic(
    "resize-proof-region-received", second_epoch, "leaf-layout"
)

def issued_target(text: str) -> tuple[int, int, int, int]:
    found = re.search(r"target=(-?\d+):(-?\d+):(\d+):(\d+)", text)
    if found is None:
        raise SystemExit(f"region issue has no target: {text}")
    return tuple(map(int, found.groups()))


middle_target = issued_target(middle_issue_text)
leaf_target = issued_target(leaf_issue_text)
middle_rect = target_rects[middle_pane]
leaf_rect = target_rects[leaf_pane]

projected_epochs, projected_sha = frame_epochs(
    "normal-projected", target_rects, [middle_pane, leaf_pane]
)
promoted_epochs, promoted_sha = frame_epochs(
    "normal-promoted", target_rects, [middle_pane, leaf_pane]
)

timeout_epoch = int(timeout["epoch"])
timeout_text = line(timeout["accepted"])
timeout_rects = parse_rects(timeout_text)
timeout_projected_epochs, timeout_projected_sha = frame_epochs(
    "timeout-projected", timeout_rects, [middle_pane, leaf_pane]
)
timeout_degraded_epochs, timeout_degraded_sha = frame_epochs(
    "timeout-degraded", timeout_rects, [middle_pane, leaf_pane]
)

middle_response_us = int(fields(middle_accept_text)["response_us"])
leaf_response_us = int(fields(leaf_accept_text)["response_us"])
ready_line, ready_text = first_line(
    rf"synchronized layout ready actor=session-switcher .*epoch={second_epoch} "
)
ready_fields = fields(ready_text)
ready_duration_us = int(ready_fields["duration_us"])

normal_durable_middle = all_lines(
    rf'"kind":"resize-proof-layout-state".*"mode":"middle-layout".*"presented_layout_revision":{accepted_fields["target_revision"]}'
)
normal_durable_leaf = all_lines(
    rf'"kind":"resize-proof-layout-state".*"mode":"leaf-layout".*"presented_layout_revision":{accepted_fields["target_revision"]}'
)

region_resize_staging = []
for index, text in enumerate(lines[topology["armed"] :], topology["armed"] + 1):
    if "resize-present " not in text or "window-resize-present " in text:
        continue
    staged = re.search(r"(?:^| )staged_count=(\d+)", text)
    if staged is not None:
        region_resize_staging.append((index, int(staged.group(1))))

timeout_recovery_text = line(timeout["recovery"])
recovered_match = re.search(r"recovered_leases=\[([^]]+)\]", timeout_recovery_text)
recovered_leases = (
    [] if recovered_match is None else [int(value) for value in recovered_match.group(1).split(",")]
)
timeout_ready_line, timeout_ready_text = first_line(
    rf"synchronized layout ready actor=session-switcher .*epoch={timeout_epoch} "
)
timeout_ready = fields(timeout_ready_text)
timeout_middle_issues = [
    (index, text)
    for index, text in all_lines(
        rf"layout-region issued epoch={timeout_epoch} lease={middle_lease_id} "
    )
    if index < timeout["recovery"]
]
timeout_leaf_issues = [
    (index, text)
    for index, text in all_lines(
        rf"layout-region issued epoch={timeout_epoch} lease={leaf_lease_id} "
    )
    if index < timeout["terminal"]
]

stale_text = line(supersession["stale_reply"])
old_epoch = int(first["epoch"])
old_epoch_mutations = [
    (index, text)
    for index, text in enumerate(lines[second["accepted"] :], second["accepted"] + 1)
    if (
        f"layout-region response-accepted epoch={old_epoch} " in text
        or f"layout-region pane-notified epoch={old_epoch} " in text
        or (f"resize-present " in text and f"epoch={old_epoch} " in text and "phase=promoted" in text)
    )
]

foreign_sync = [
    text
    for text in lines
    if "synchronized layout actor=" in text
    and "actor=session-switcher " not in text
]
region_totals = []
for text in lines:
    if "synchronized layout actor=session-switcher " not in text:
        continue
    found = re.search(r"region_final=(\d+)/(\d+)", text)
    if found:
        region_totals.append(tuple(map(int, found.groups())))

assertions = {
    "production_root_and_three_level_authority": (
        topology["root_grant"] < topology["middle_grant"] < topology["leaf_grant"]
        and middle_lease_id == middle_parent_id
        and len({root_lease_id, middle_lease_id, leaf_lease_id}) == 3
    ),
    "real_pointer_interaction_supersedes": (
        int(first["interaction"]) > 0
        and first["interaction"] == second["interaction"]
        and first["sequence"] < second["sequence"]
        and first["accepted"] < second["accepted"]
    ),
    "root_middle_leaf_notification_order": (
        middle_issue_line
        < root_notify_line
        < middle_accept_line
        < middle_notify_line
        < leaf_accept_line
        < leaf_notify_line
        < ready_line
        and second["accepted"] < middle_accept_line
        and all(
            fields(text).get("epoch") == str(second_epoch)
            for text in (
                middle_issue_text,
                root_notify_text,
                middle_notify_text,
                leaf_issue_text,
                leaf_notify_text,
            )
        )
    ),
    "sequential_delays_fit_deadline": (
        middle_response_us >= 130_000
        and leaf_response_us >= 20_000
        and ready_duration_us < 250_000
    ),
    "same_epoch_exact_region_extents": (
        middle_issue_line < middle_received_line < middle_accept_line
        and leaf_issue_line < leaf_received_line < leaf_accept_line
        and (middle_received["width"], middle_received["height"])
        == (middle_target[2], middle_target[3])
        and (leaf_received["width"], leaf_received["height"])
        == (leaf_target[2], leaf_target[3])
        and abs(float(middle_received["scale"]) - scale) < 0.01
        and abs(float(leaf_received["scale"]) - scale) < 0.01
        and middle_rect[0] == middle_target[0]
        and middle_rect[1] == middle_target[1]
        and leaf_rect == leaf_target
    ),
    "projected_old_child_not_durable": (
        projected_epochs[middle_pane]["epoch"] != second_epoch
        and projected_epochs[leaf_pane]["epoch"] != second_epoch
        and promoted_epochs[middle_pane]["epoch"] == second_epoch
        and promoted_epochs[leaf_pane]["epoch"] == second_epoch
        and projected_sha != promoted_sha
        and second["projected"] < middle_accept_line < second["promoted"]
    ),
    "promoted_fiducials_match_final_extents": all(
        abs(promoted_epochs[pane]["width"] * promoted_epochs[pane]["scale"] - target_rects[pane][2]) <= 2.0
        and abs(promoted_epochs[pane]["height"] * promoted_epochs[pane]["scale"] - target_rects[pane][3]) <= 2.0
        and abs(promoted_epochs[pane]["scale"] - scale) < 0.02
        for pane in (middle_pane, leaf_pane)
    ),
    "normal_promotion_has_durable_layout_state": (
        any(index > second["promoted"] for index, _text in normal_durable_middle)
        and any(index > second["promoted"] for index, _text in normal_durable_leaf)
        and second["promoted"] < second["completed"]
        and "degraded=false" in line(second["promoted"])
    ),
    "stale_superseded_reply_fails_closed": (
        supersession["stale_reply"] > second["accepted"]
        and f"epoch={old_epoch}" in stale_text
        and "reason=StaleEpoch" in stale_text
        and not old_epoch_mutations
    ),
    "bounded_single_region_state": (
        bool(region_totals)
        and max(total for _final, total in region_totals) <= 2
        and all(final <= total for final, total in region_totals)
        and bool(region_resize_staging)
        and max(staged for _index, staged in region_resize_staging) <= 1
        and not foreign_sync
    ),
    "normal_final_release_authenticates_after_capture": (
        release["sequence"] > second["sequence"]
        and release["capture_released"] < release["accepted"]
        and release["accepted"] < release["promoted"] < release["completed"]
        and "phase=ButtonRelease" in line(release["capture_released"])
    ),
    "timeout_recovers_issued_and_unissued_descendants": (
        set(recovered_leases) == {middle_lease_id, leaf_lease_id}
        and len(timeout_middle_issues) == 1
        and not timeout_leaf_issues
        and timeout["accepted"] < timeout["projected"] < timeout["recovery"]
        and timeout["recovery"] < timeout["degraded"] < timeout["terminal"]
        and timeout_ready.get("region_final") == "2/2"
        and timeout_ready.get("recovered_count") == "2"
        and timeout_ready.get("degraded") == "true"
    ),
    "timeout_promotes_only_explicit_recovery": (
        timeout_projected_epochs[middle_pane]["epoch"] != timeout_epoch
        and timeout_projected_epochs[leaf_pane]["epoch"] != timeout_epoch
        and timeout_degraded_epochs[middle_pane]["epoch"] != timeout_epoch
        and timeout_degraded_epochs[leaf_pane]["epoch"] != timeout_epoch
        and timeout_projected_epochs == timeout_degraded_epochs
    ),
    "post_timeout_release_converges": (
        final_release["sequence"] > timeout["sequence"]
        and final_release["capture_released"] < final_release["accepted"]
        and final_release["accepted"]
        < final_release["promoted"]
        < final_release["completed"]
        and "degraded=false" in line(final_release["promoted"])
    ),
}

metrics = {
    "verified": all(assertions.values()),
    "assertions": assertions,
    "topology": {
        "root_lease": root_lease_id,
        "root_generation": root_lease,
        "middle_lease": middle_lease_id,
        "leaf_lease": leaf_lease_id,
        "panes": topology["panes"],
    },
    "normal": {
        "first_epoch": old_epoch,
        "epoch": second_epoch,
        "middle_target": middle_target,
        "leaf_target": leaf_target,
        "middle_response_us": middle_response_us,
        "leaf_response_us": leaf_response_us,
        "ready_duration_us": ready_duration_us,
        "projected_epochs": projected_epochs,
        "promoted_epochs": promoted_epochs,
    },
    "timeout": {
        "epoch": timeout_epoch,
        "recovered_leases": recovered_leases,
        "projected_epochs": timeout_projected_epochs,
        "degraded_epochs": timeout_degraded_epochs,
    },
}
print(json.dumps(metrics, indent=2, sort_keys=True))
