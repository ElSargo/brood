#!/usr/bin/env python3
import json
import struct
import sys
import zlib
from collections import deque
from pathlib import Path


CELL_SIZE = 5
GRID_SIZE = 9
ANCHOR_COL = 1
ANCHOR_VISUAL_ROW = 7
MARKER_SIZE = CELL_SIZE * GRID_SIZE
POSITION_TOLERANCE = 1


def usage():
    print(
        "usage: detect-surface-markers.py FRAME.png STATE.json OUT.json",
        file=sys.stderr,
    )


def paeth(a, b, c):
    p = a + b - c
    pa = abs(p - a)
    pb = abs(p - b)
    pc = abs(p - c)
    if pa <= pb and pa <= pc:
        return a
    if pb <= pc:
        return b
    return c


def read_png(path):
    data = Path(path).read_bytes()
    if not data.startswith(b"\x89PNG\r\n\x1a\n"):
        raise ValueError(f"{path} is not a PNG")

    pos = 8
    width = height = color_type = bit_depth = None
    compressed = b""
    while pos < len(data):
        length = struct.unpack(">I", data[pos : pos + 4])[0]
        chunk_type = data[pos + 4 : pos + 8]
        chunk = data[pos + 8 : pos + 8 + length]
        pos += length + 12
        if chunk_type == b"IHDR":
            width, height, bit_depth, color_type, _, _, _ = struct.unpack(">IIBBBBB", chunk)
        elif chunk_type == b"IDAT":
            compressed += chunk
        elif chunk_type == b"IEND":
            break

    if bit_depth != 8 or color_type not in (2, 6):
        raise ValueError(f"{path} must be 8-bit RGB or RGBA PNG")

    channels = 4 if color_type == 6 else 3
    raw = zlib.decompress(compressed)
    stride = width * channels
    rows = []
    prev = [0] * stride
    cursor = 0
    for _ in range(height):
        filter_type = raw[cursor]
        cursor += 1
        scan = list(raw[cursor : cursor + stride])
        cursor += stride
        row = []
        for index, value in enumerate(scan):
            left = row[index - channels] if index >= channels else 0
            up = prev[index]
            upper_left = prev[index - channels] if index >= channels else 0
            if filter_type == 0:
                decoded = value
            elif filter_type == 1:
                decoded = value + left
            elif filter_type == 2:
                decoded = value + up
            elif filter_type == 3:
                decoded = value + ((left + up) // 2)
            elif filter_type == 4:
                decoded = value + paeth(left, up, upper_left)
            else:
                raise ValueError(f"unsupported PNG filter {filter_type}")
            row.append(decoded & 0xFF)
        rows.append(row)
        prev = row

    return {
        "width": width,
        "height": height,
        "channels": channels,
        "rows": rows,
    }


def rgb(image, x, y):
    offset = x * image["channels"]
    return tuple(image["rows"][y][offset : offset + 3])


def is_magenta(pixel):
    red, green, blue = pixel
    return red >= 240 and green <= 32 and 170 <= blue <= 220


def is_dark(pixel):
    return max(pixel) <= 32


def is_light(pixel):
    return min(pixel) >= 230


def magenta_components(image):
    width = image["width"]
    height = image["height"]
    seen = set()
    components = []
    for y in range(height):
        for x in range(width):
            if (x, y) in seen or not is_magenta(rgb(image, x, y)):
                continue
            queue = deque([(x, y)])
            seen.add((x, y))
            points = []
            while queue:
                px, py = queue.popleft()
                points.append((px, py))
                for nx, ny in ((px - 1, py), (px + 1, py), (px, py - 1), (px, py + 1)):
                    if (
                        nx < 0
                        or nx >= width
                        or ny < 0
                        or ny >= height
                        or (nx, ny) in seen
                        or not is_magenta(rgb(image, nx, ny))
                    ):
                        continue
                    seen.add((nx, ny))
                    queue.append((nx, ny))
            if len(points) >= 10:
                xs = [point[0] for point in points]
                ys = [point[1] for point in points]
                components.append(
                    {
                        "x": min(xs),
                        "y": min(ys),
                        "w": max(xs) - min(xs) + 1,
                        "h": max(ys) - min(ys) + 1,
                        "pixels": len(points),
                    }
                )
    return components


def classify_cell(image, origin_x, origin_y, col, visual_row):
    counts = {"dark": 0, "magenta": 0, "light": 0, "other": 0}
    for y in range(origin_y + visual_row * CELL_SIZE, origin_y + (visual_row + 1) * CELL_SIZE):
        for x in range(origin_x + col * CELL_SIZE, origin_x + (col + 1) * CELL_SIZE):
            if x < 0 or x >= image["width"] or y < 0 or y >= image["height"]:
                counts["other"] += 1
                continue
            pixel = rgb(image, x, y)
            if is_dark(pixel):
                counts["dark"] += 1
            elif is_magenta(pixel):
                counts["magenta"] += 1
            elif is_light(pixel):
                counts["light"] += 1
            else:
                counts["other"] += 1
    return counts


def decode_marker(image, component):
    origin_x = component["x"] - ANCHOR_COL * CELL_SIZE
    origin_y = component["y"] - ANCHOR_VISUAL_ROW * CELL_SIZE
    marker_id = 0
    dark_bits = []
    for logical_row in range(2, 7):
        visual_row = GRID_SIZE - 1 - logical_row
        for col in range(2, 7):
            counts = classify_cell(image, origin_x, origin_y, col, visual_row)
            bit = (logical_row - 2) * 5 + (col - 2)
            dark = counts["dark"] >= 13
            if dark:
                marker_id |= 1 << bit
                dark_bits.append(bit)
    return {
        "marker_id": marker_id,
        "marker_rect_image": {
            "x": origin_x,
            "y": origin_y,
            "w": MARKER_SIZE,
            "h": MARKER_SIZE,
        },
        "anchor_rect_image": {
            "x": component["x"],
            "y": component["y"],
            "w": component["w"],
            "h": component["h"],
        },
        "anchor_pixels": component["pixels"],
        "dark_bits": dark_bits,
    }


def expected_windows(state, image_height):
    windows = []
    for window in state.get("windows", []):
        rect = window.get("rect")
        marker_id = window.get("marker_id")
        if rect is None or marker_id is None:
            continue
        layout_rect_image = {
            "x": rect["x"],
            "y": image_height - rect["y"] - rect["h"],
            "w": rect["w"],
            "h": rect["h"],
        }
        windows.append(
            {
                "index": window.get("index"),
                "surface_id": window.get("surface_id"),
                "marker_id": marker_id,
                "focused": window.get("focused"),
                "layout_rect": rect,
                "layout_rect_image": layout_rect_image,
                "expected_origin_marker_image": {
                    "x": rect["x"],
                    "y": image_height - rect["y"] - MARKER_SIZE,
                    "w": MARKER_SIZE,
                    "h": MARKER_SIZE,
                },
            }
        )
    return windows


def point_delta(left, right):
    return {
        "x": left["x"] - right["x"],
        "y": left["y"] - right["y"],
    }


def rect_contains(outer, inner):
    return (
        inner["x"] >= outer["x"] - POSITION_TOLERANCE
        and inner["y"] >= outer["y"] - POSITION_TOLERANCE
        and inner["x"] + inner["w"] <= outer["x"] + outer["w"] + POSITION_TOLERANCE
        and inner["y"] + inner["h"] <= outer["y"] + outer["h"] + POSITION_TOLERANCE
    )


def inferred_surface_rect_image(origin_marker, opposite_marker):
    origin = origin_marker["marker_rect_image"]
    opposite = opposite_marker["marker_rect_image"]
    return {
        "x": origin["x"],
        "y": opposite["y"],
        "w": opposite["x"] + MARKER_SIZE - origin["x"],
        "h": origin["y"] + MARKER_SIZE - opposite["y"],
    }


def compare(detected, expected):
    windows = []
    used_detected = set()
    for window in expected:
        candidates = [
            (index, marker)
            for index, marker in enumerate(detected)
            if index not in used_detected and marker["marker_id"] == window["marker_id"]
        ]
        if len(candidates) < 2:
            windows.append(
                {
                    "marker_id": window["marker_id"],
                    "status": "missing_opposite_corner_marker",
                    "expected": window,
                    "detected_markers": [marker for _, marker in candidates],
                }
            )
            used_detected.update(index for index, _ in candidates)
            continue

        origin_index, origin_marker = min(
            candidates,
            key=lambda item: abs(
                item[1]["marker_rect_image"]["x"] - window["expected_origin_marker_image"]["x"]
            )
            + abs(item[1]["marker_rect_image"]["y"] - window["expected_origin_marker_image"]["y"]),
        )
        remaining = [(index, marker) for index, marker in candidates if index != origin_index]
        opposite_index, opposite_marker = min(
            remaining,
            key=lambda item: (
                item[1]["marker_rect_image"]["y"],
                -item[1]["marker_rect_image"]["x"],
            ),
        )
        used_detected.update((origin_index, opposite_index))

        origin_delta = point_delta(
            origin_marker["marker_rect_image"],
            window["expected_origin_marker_image"],
        )
        surface_rect = inferred_surface_rect_image(origin_marker, opposite_marker)
        origin_ok = (
            abs(origin_delta["x"]) <= POSITION_TOLERANCE
            and abs(origin_delta["y"]) <= POSITION_TOLERANCE
        )
        contained = rect_contains(window["layout_rect_image"], surface_rect)
        status = "matched" if origin_ok and contained else "mismatch"
        windows.append(
            {
                "marker_id": window["marker_id"],
                "status": status,
                "origin_delta_image_px": origin_delta,
                "origin_ok": origin_ok,
                "opposite_corner_visible": True,
                "surface_inside_layout_rect": contained,
                "expected": window,
                "detected_origin_marker": origin_marker,
                "detected_opposite_marker": opposite_marker,
                "detected_surface_rect_image": surface_rect,
            }
        )
    return windows, used_detected


def main():
    if len(sys.argv) != 4:
        usage()
        return 2

    frame_path = Path(sys.argv[1])
    state_path = Path(sys.argv[2])
    out_path = Path(sys.argv[3])
    image = read_png(frame_path)
    state = json.loads(state_path.read_text())

    components = magenta_components(image)
    detected = sorted(
        (decode_marker(image, component) for component in components),
        key=lambda marker: (marker["marker_rect_image"]["y"], marker["marker_rect_image"]["x"]),
    )
    expected = expected_windows(state, image["height"])
    window_matches, used_detected = compare(detected, expected)
    unmatched = [
        marker
        for index, marker in enumerate(detected)
        if index not in used_detected
    ]
    success = (
        len(detected) >= len(expected) * 2
        and all(match["status"] == "matched" for match in window_matches)
        and not unmatched
    )

    report = {
        "success": success,
        "coordinate_spaces": {
            "image": "top-left origin, x right, y down",
            "layout_rect": "Manifold layout coordinates from state.json",
        },
        "image": {
            "path": str(frame_path),
            "width": image["width"],
            "height": image["height"],
        },
        "marker_format": {
            "grid": GRID_SIZE,
            "cell_px": CELL_SIZE,
            "marker_px": MARKER_SIZE,
            "anchor": "magenta cell at logical col=1,row=1; appears at visual row=7 in captured PNG",
        },
        "expected_windows": expected,
        "detected_markers": detected,
        "window_matches": window_matches,
        "unmatched_detected_markers": unmatched,
        "layout": state.get("layout"),
    }
    out_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")

    if not success:
        print(f"ERR marker detection mismatch; wrote {out_path}", file=sys.stderr)
        return 1
    print(f"OK marker detection {out_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
