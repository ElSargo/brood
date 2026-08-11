#!/usr/bin/env python3
"""Choose a pointer pulse inside current and intended WindowSlow geometry."""

from __future__ import annotations

import json
import math
import re
import sys
from typing import Any


def compute(
    window: dict[str, int],
    line: str,
    pane_id: str,
    scale: float,
    intended_width: int,
    intended_height: int,
    fullscreen: bool,
) -> tuple[int, int, int, dict[str, float], dict[str, float], dict[str, int]]:
    pane = re.escape(pane_id)
    match = re.search(
        rf"(?:\[|,)({pane}):(-?\d+):(-?\d+):(\d+):(\d+)(?:,|\])", line
    )
    if match is None:
        raise ValueError("WindowSlow pane geometry is absent from the accepted target")
    _pane, x, y, width, height = map(int, match.groups())
    current_physical_width = round(window["width"] * scale)
    current_physical_height = round(window["height"] * scale)
    fixed_trailing_width = current_physical_width - 2 * width
    fixed_bottom_height = current_physical_height - height
    if fixed_trailing_width < 0 or fixed_bottom_height < 0:
        raise ValueError("current WindowSlow geometry has invalid fixed chrome")
    if fullscreen:
        intended_window = {
            "x": 0,
            "y": 0,
            "width": round(intended_width / scale),
            "height": round(intended_height / scale),
        }
    else:
        intended_logical_width = round(intended_width / scale)
        intended_logical_height = round(intended_height / scale)
        intended_window = {
            "x": round(window["x"] + (window["width"] - intended_logical_width) / 2),
            "y": round(window["y"] + (window["height"] - intended_logical_height) / 2),
            "width": intended_logical_width,
            "height": intended_logical_height,
        }
    current = {
        "x": window["x"] + x / scale,
        "y": window["y"] + y / scale,
        "width": width / scale,
        "height": height / scale,
    }
    intended = {
        "x": intended_window["x"],
        "y": intended_window["y"],
        "width": (intended_width - fixed_trailing_width) / 2 / scale,
        "height": (intended_height - fixed_bottom_height) / scale,
    }
    left = max(current["x"], intended["x"])
    top = max(current["y"], intended["y"])
    right = min(current["x"] + current["width"], intended["x"] + intended["width"])
    bottom = min(current["y"] + current["height"], intended["y"] + intended["height"])
    point_x = math.floor((left + right) / 2)
    point_y = math.floor((top + bottom) / 2)
    pulse_x = point_x + 1

    def contains(rect: dict[str, float], px: int, py: int) -> bool:
        return (
            rect["x"] < px < rect["x"] + rect["width"]
            and rect["y"] < py < rect["y"] + rect["height"]
        )

    if not all(
        contains(rect, px, point_y)
        for rect in (current, intended)
        for px in (point_x, pulse_x)
    ):
        raise ValueError("no conservative two-pixel WindowSlow intersection")
    return point_x, point_y, pulse_x, current, intended, intended_window


def emit(result: tuple[int, int, int, dict[str, Any], dict[str, Any], dict[str, Any]]) -> None:
    point_x, point_y, pulse_x, current, intended, intended_window = result
    print(point_x, point_y, pulse_x)
    for rect in (current, intended, intended_window):
        print(json.dumps(rect, separators=(",", ":")))


def self_test() -> None:
    line = "accepted target_rects=[9:0:0:402:616,10:402:0:402:616]"
    result = compute(
        {"x": 162, "y": 76, "width": 900, "height": 520},
        line,
        "9",
        1.25,
        1020,
        600,
        False,
    )
    assert result[:3] == (343, 322, 344)
    assert result[5] == {"x": 204, "y": 96, "width": 816, "height": 480}
    fullscreen = compute(
        {"x": 232, "y": 108, "width": 760, "height": 456},
        "accepted target_rects=[9:0:0:315:536]",
        "9",
        1.25,
        1280,
        720,
        True,
    )
    assert fullscreen[5] == {"x": 0, "y": 0, "width": 1024, "height": 576}
    try:
        compute(
            {"x": 900, "y": 0, "width": 100, "height": 100},
            "accepted target_rects=[9:0:0:40:100]",
            "9",
            1.0,
            100,
            100,
            True,
        )
    except ValueError as error:
        assert "intersection" in str(error)
    else:
        raise AssertionError("disjoint panes must fail")


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        self_test()
    elif len(sys.argv) == 8:
        emit(
            compute(
                json.loads(sys.argv[1]),
                sys.argv[2],
                sys.argv[3],
                float(sys.argv[4]),
                int(sys.argv[5]),
                int(sys.argv[6]),
                sys.argv[7] == "true",
            )
        )
    else:
        raise SystemExit(
            "usage: resize-window-gate-geometry.py WINDOW LINE PANE SCALE WIDTH HEIGHT FULLSCREEN"
        )
