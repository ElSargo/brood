#!/usr/bin/env python3
"""Recognize the causally complete actor-frame startup resize proof."""

from __future__ import annotations

import argparse
import json
import re
from dataclasses import dataclass
from pathlib import Path


MESSAGE = re.compile(
    r"presentation local actor message sender=(?P<sender>\S+) "
    r"recipient=(?P<recipient>\S+) body=(?P<body>\{.*\})$"
)


def fields(line: str) -> dict[str, str]:
    result: dict[str, str] = {}
    for word in line.split():
        if "=" in word:
            name, value = word.split("=", 1)
            result[name] = value
    return result


def exact_tagged_message(
    line: str,
    *,
    actor: str,
    mode: str,
    epoch: int,
    width: int,
    height: int,
) -> int | None:
    match = MESSAGE.search(line)
    if match is None or match["sender"] != actor or match["recipient"] != actor:
        return None
    try:
        body = json.loads(match["body"])
    except json.JSONDecodeError:
        return None
    if not (
        body.get("kind") == "resize-proof-tagged"
        and body.get("mode") == mode
        and body.get("epoch") == epoch
        and body.get("width") == float(width)
        and body.get("height") == float(height)
        and body.get("success") is True
        and isinstance(body.get("pane"), int)
    ):
        return None
    return body["pane"]


@dataclass(frozen=True)
class AcceptedLayout:
    request: int
    native: int
    epoch: int
    target_revision: int
    wait_panes: frozenset[int]


def accepted_layout(line: str) -> AcceptedLayout | None:
    if not (
        "accepted synchronized layout actor=workspace " in line
        and re.search(r" target_rects=\[[^]]*\]$", line)
    ):
        return None
    values = fields(line)
    try:
        wait_panes = frozenset(
            int(pane)
            for pane in values["wait_for_panes"]
            .removeprefix("[")
            .removesuffix("]")
            .split(",")
        )
        accepted = AcceptedLayout(
            request=int(values["request"]),
            native=int(values["native_resize_revision"]),
            epoch=int(values["epoch"]),
            target_revision=int(values["target_revision"]),
            wait_panes=wait_panes,
        )
    except (KeyError, ValueError):
        return None
    if (
        accepted.native <= 0
        or values.get("wait") != "2"
        or values.get("affected") != "2"
        or values.get("pane_ready") != "0/2"
        or len(accepted.wait_panes) != 2
    ):
        return None
    return accepted


def startup_completion_line(
    lines: list[str],
    *,
    width: int,
    height: int,
    first_actor: str,
    first_mode: str,
    second_actor: str,
    second_mode: str,
) -> int | None:
    accepted: AcceptedLayout | None = None
    panes: dict[str, int] = {}
    ready = common_promoted = completed = window_promoted = False

    for line_number, line in enumerate(lines, start=1):
        next_accepted = accepted_layout(line)
        if next_accepted is not None:
            accepted = next_accepted
            panes.clear()
            ready = common_promoted = completed = window_promoted = False
            continue
        if accepted is None:
            continue

        for actor, mode in ((first_actor, first_mode), (second_actor, second_mode)):
            pane = exact_tagged_message(
                line,
                actor=actor,
                mode=mode,
                epoch=accepted.epoch,
                width=width,
                height=height,
            )
            if pane is not None:
                panes[actor] = pane

        values = fields(line)
        both_tagged = (
            set(panes) == {first_actor, second_actor}
            and len(set(panes.values())) == 2
            and set(panes.values()) == accepted.wait_panes
        )
        if (
            both_tagged
            and not ready
            and "synchronized layout ready actor=workspace " in line
            and values.get("request") == str(accepted.request)
            and values.get("epoch") == str(accepted.epoch)
            and values.get("target_revision") == str(accepted.target_revision)
            and values.get("pane_ready") == "2/2"
            and values.get("missing") == "[]"
            and values.get("degraded") == "false"
        ):
            ready = True
            continue
        if (
            ready
            and not common_promoted
            and "resize-present " in line
            and not line.startswith("window-resize-present ")
            and values.get("epoch") == str(accepted.epoch)
            and values.get("phase") == "promoted"
            and values.get("wait_ready") == "2/2"
            and values.get("staged_count") == "0"
            and values.get("degraded") == "false"
        ):
            common_promoted = True
            continue
        if (
            common_promoted
            and not completed
            and "completed synchronized layout actor=workspace " in line
            and values.get("request") == str(accepted.request)
            and values.get("native_resize_revision") == str(accepted.native)
            and values.get("epoch") == str(accepted.epoch)
            and values.get("target_revision") == str(accepted.target_revision)
            and values.get("pane_ready") == "2/2"
            and values.get("missing") == "[]"
        ):
            completed = True
            continue
        if (
            completed
            and not window_promoted
            and line.startswith("window-resize-present ")
            and values.get("native_resize_revision") == str(accepted.native)
            and values.get("epoch") == str(accepted.epoch)
            and values.get("phase") == "promoted"
            and values.get("wait_ready") == "2/2"
            and values.get("staged_count") == "0"
            and values.get("degraded") == "false"
        ):
            window_promoted = True
            continue
        if (
            window_promoted
            and "accepted unchanged leased actor layout " in line
            and values.get("presented_layout_revision")
            == str(accepted.target_revision)
        ):
            return line_number
    return None


def check_file(path: Path, args: argparse.Namespace) -> int | None:
    return startup_completion_line(
        path.read_text(encoding="utf-8").splitlines(),
        width=args.width,
        height=args.height,
        first_actor=args.first_actor,
        first_mode=args.first_mode,
        second_actor=args.second_actor,
        second_mode=args.second_mode,
    )


def self_test(root: Path) -> None:
    fixtures = root / "fixtures" / "resize-startup"
    options = argparse.Namespace(
        width=512,
        height=576,
        first_actor="resize-proof-fast",
        first_mode="fast",
        second_actor="resize-proof-slow",
        second_mode="slow",
    )
    positive = check_file(fixtures / "positive.log", options)
    if positive is None:
        raise SystemExit("positive actor-frame startup fixture was rejected")
    for name in (
        "missing-actor",
        "wrong-extent",
        "missing-ready",
        "duplicate-pane",
        "wrong-pane",
        "successes-without-ready",
        "reordered-completion",
    ):
        if check_file(fixtures / f"{name}.log", options) is not None:
            raise SystemExit(f"negative actor-frame startup fixture was accepted: {name}")
    print("resize_startup_parser_self_test=verified")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("log", nargs="?", type=Path)
    parser.add_argument("--width", type=int, default=512)
    parser.add_argument("--height", type=int, default=576)
    parser.add_argument("--first-actor", default="resize-proof-fast")
    parser.add_argument("--first-mode", default="fast")
    parser.add_argument("--second-actor", default="resize-proof-slow")
    parser.add_argument("--second-mode", default="slow")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test(Path(__file__).resolve().parent)
        return
    if args.log is None:
        parser.error("log is required unless --self-test is used")
    result = check_file(args.log, args)
    if result is not None:
        print(result)


if __name__ == "__main__":
    main()
