#!/usr/bin/env python3
"""Prove current presentation actors have promoted an exact singleton frame."""

from __future__ import annotations

import argparse
import json
import pathlib
import re
from typing import Any


ACTORS = ("terminal-ui", "session-switcher")
ACTOR_START = re.compile(
    r"^started presentation WASM actor id=(?P<actor>terminal-ui|session-switcher) "
    r"kind=[^ ]+ generation=(?P<generation>[0-9]+) pane=(?P<pane>[0-9]+) path=.+$"
)
PROMOTED = re.compile(
    r"^render scheduling actor-frame promoted actor=(?P<actor>terminal-ui|session-switcher) "
    r"generation=(?P<generation>[0-9]+) token=(?P<token>[0-9]+) "
    r"revision=(?P<revision>[0-9]+) pane_count=(?P<pane_count>[0-9]+) "
    r"panes=\[(?P<panes>[^]]*)\]$"
)


def validate(lines: list[str]) -> dict[str, Any]:
    starts: dict[str, list[tuple[int, re.Match[str]]]] = {actor: [] for actor in ACTORS}
    promotions: dict[str, list[tuple[int, re.Match[str]]]] = {actor: [] for actor in ACTORS}
    for line_number, line in enumerate(lines, 1):
        if match := ACTOR_START.match(line):
            starts[match["actor"]].append((line_number, match))
        if match := PROMOTED.match(line):
            promotions[match["actor"]].append((line_number, match))

    result = {}
    for actor in ACTORS:
        if not starts[actor]:
            raise ValueError(f"missing exact actor start actor={actor}")
        start_line, start = starts[actor][-1]
        generation = int(start["generation"])
        pane = int(start["pane"])
        if generation == 0 or pane == 0:
            raise ValueError(
                f"actor start identities must be positive actor={actor} generation={generation} pane={pane}"
            )
        matching = []
        for promotion_line, promotion in promotions[actor]:
            pane_list = [int(item) for item in promotion["panes"].split(",") if item]
            if (
                promotion_line > start_line
                and int(promotion["generation"]) == generation
                and int(promotion["pane_count"]) == 1
                and pane_list == [pane]
                and int(promotion["token"]) > 0
                and int(promotion["revision"]) > 0
            ):
                matching.append((promotion_line, promotion))
        if not matching:
            raise ValueError(
                "latest actor generation has no exact singleton promoted frame: "
                f"actor={actor} start_line={start_line} generation={generation} pane={pane}"
            )
        promotion_line, promotion = matching[-1]
        result[actor] = {
            "start_line": start_line,
            "generation": generation,
            "pane": pane,
            "promotion_line": promotion_line,
            "token": int(promotion["token"]),
            "revision": int(promotion["revision"]),
        }
    return {"verified": True, "actors": result}


def self_test() -> None:
    fixture_root = pathlib.Path(__file__).with_name("fixtures") / "actor-frame-startup"
    for name in ("positive-terminal-first.txt", "positive-session-first.txt"):
        validate((fixture_root / name).read_text(encoding="utf-8").splitlines())
    positive = (fixture_root / "positive-terminal-first.txt").read_text(encoding="utf-8")
    cases = json.loads((fixture_root / "negative-cases.json").read_text(encoding="utf-8"))
    unexpectedly_valid = []
    for name, mutation in cases.items():
        mutated = positive.replace(mutation["old"], mutation["new"], 1)
        if mutated == positive:
            raise AssertionError(f"self-test mutation did not apply: {name}")
        try:
            validate(mutated.splitlines())
        except ValueError:
            continue
        unexpectedly_valid.append(name)
    if unexpectedly_valid:
        raise SystemExit(f"invalid actor-frame startup cases passed: {unexpectedly_valid}")
    print(f"actor-frame startup self-test passed: 2 positive, {len(cases)} negative")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("log", nargs="?", type=pathlib.Path)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        if args.log is not None:
            parser.error("--self-test does not accept a log")
        self_test()
        return
    if args.log is None:
        parser.error("log is required unless --self-test is used")
    print(json.dumps(validate(args.log.read_text(encoding="utf-8", errors="replace").splitlines())))


if __name__ == "__main__":
    main()
