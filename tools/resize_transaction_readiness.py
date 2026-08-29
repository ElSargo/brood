#!/usr/bin/env python3
"""Validate aggregate synchronized-resize readiness evidence."""

from __future__ import annotations

import pathlib
import sys
from typing import Any


READY_PREFIX = "synchronized layout ready "
COMPLETED_PREFIX = "completed synchronized layout "
ACCEPTED_PREFIX = "accepted synchronized layout "
PROJECTED_PREFIX = "resize-present "


def fields(text: str, prefix: str) -> dict[str, str]:
    if not text.startswith(prefix):
        raise ValueError(f"trace does not start with {prefix!r}: {text!r}")
    result: dict[str, str] = {}
    for token in text[len(prefix) :].split():
        if "=" in token:
            name, value = token.split("=", 1)
            result[name] = value
    return result


def _integer_pair(value: str, name: str) -> tuple[int, int]:
    try:
        left, right = value.split("/", 1)
        return int(left), int(right)
    except (AttributeError, TypeError, ValueError) as error:
        raise ValueError(f"invalid {name}={value!r}") from error


def _require_exact_record(
    record: dict[str, str],
    *,
    actor: str,
    request: int,
    interaction: int,
    event_sequence: int,
    epoch: int,
    target_revision: int,
    region_total: int,
    wait_total: int,
    require_degraded: bool,
) -> None:
    expected = {
        "actor": actor,
        "request": str(request),
        "interaction": str(interaction),
        "event_sequence": str(event_sequence),
        "epoch": str(epoch),
        "target_revision": str(target_revision),
        "missing": "[]",
    }
    if require_degraded:
        expected["degraded"] = "false"
    mismatches = {
        name: (record.get(name), value)
        for name, value in expected.items()
        if record.get(name) != value
    }
    if mismatches:
        raise ValueError(f"synchronized readiness identity mismatch: {mismatches}")
    if _integer_pair(record.get("region_final", ""), "region_final") != (
        region_total,
        region_total,
    ):
        raise ValueError(
            "synchronized readiness did not finalize the exact region set: "
            f"region_final={record.get('region_final')} expected={region_total}/{region_total}"
        )
    if _integer_pair(record.get("pane_ready", ""), "pane_ready") != (
        wait_total,
        wait_total,
    ):
        raise ValueError(
            "synchronized readiness did not cover the exact wait set: "
            f"pane_ready={record.get('pane_ready')} expected={wait_total}/{wait_total}"
        )


def validate_transaction_readiness(
    lines: list[str],
    *,
    actor: str,
    request: int,
    interaction: int,
    event_sequence: int,
    epoch: int,
    target_revision: int,
    region_total: int,
    wait_panes: list[int],
    projected_line: int,
    promoted_line: int,
    completed_line: int,
) -> dict[str, Any]:
    """Return exact aggregate readiness evidence or reject the transaction."""
    if not wait_panes or len(wait_panes) != len(set(wait_panes)):
        raise ValueError(f"transaction wait set is empty/duplicated: {wait_panes}")
    if not 0 < projected_line < promoted_line < completed_line <= len(lines):
        raise ValueError(
            "invalid synchronized transaction order: "
            f"projected={projected_line} promoted={promoted_line} completed={completed_line}"
        )

    exact_ready: list[tuple[int, dict[str, str]]] = []
    for line_number in range(projected_line + 1, promoted_line):
        line = lines[line_number - 1]
        offset = line.find(READY_PREFIX)
        if offset < 0:
            continue
        record = fields(line[offset:], READY_PREFIX)
        if (
            record.get("actor") == actor
            and record.get("request") == str(request)
            and record.get("event_sequence") == str(event_sequence)
            and record.get("epoch") == str(epoch)
            and record.get("target_revision") == str(target_revision)
        ):
            exact_ready.append((line_number, record))
    if len(exact_ready) != 1:
        raise ValueError(
            "transaction requires exactly one identity-matched synchronized readiness record: "
            f"matches={exact_ready}"
        )
    ready_line, ready = exact_ready[0]
    if not projected_line < ready_line < promoted_line:
        raise ValueError(
            "synchronized readiness is not between projection and promotion: "
            f"projected={projected_line} ready={ready_line} promoted={promoted_line}"
        )
    _require_exact_record(
        ready,
        actor=actor,
        request=request,
        interaction=interaction,
        event_sequence=event_sequence,
        epoch=epoch,
        target_revision=target_revision,
        region_total=region_total,
        wait_total=len(wait_panes),
        require_degraded=True,
    )

    completed_text = lines[completed_line - 1]
    completed_offset = completed_text.find(COMPLETED_PREFIX)
    if completed_offset < 0:
        raise ValueError(
            f"transaction terminal is not nondegraded completion: {completed_text!r}"
        )
    completed = fields(completed_text[completed_offset:], COMPLETED_PREFIX)
    _require_exact_record(
        completed,
        actor=actor,
        request=request,
        interaction=interaction,
        event_sequence=event_sequence,
        epoch=epoch,
        target_revision=target_revision,
        region_total=region_total,
        wait_total=len(wait_panes),
        require_degraded=False,
    )
    return {
        "ready_line": ready_line,
        "ready": ready,
        "completed": completed,
    }


def _fixture_transaction(path: pathlib.Path) -> dict[str, Any]:
    lines = path.read_text(encoding="utf-8").splitlines()
    accepted_line, accepted = next(
        (line_number, fields(line, ACCEPTED_PREFIX))
        for line_number, line in enumerate(lines, 1)
        if line.startswith(ACCEPTED_PREFIX)
    )
    del accepted_line
    projected_line, projected = next(
        (line_number, fields(line, PROJECTED_PREFIX))
        for line_number, line in enumerate(lines, 1)
        if line.startswith(PROJECTED_PREFIX) and "phase=projected" in line
    )
    promoted_line, _promoted = next(
        (line_number, fields(line, PROJECTED_PREFIX))
        for line_number, line in enumerate(lines, 1)
        if line.startswith(PROJECTED_PREFIX) and "phase=promoted" in line
    )
    completed_line = next(
        line_number
        for line_number, line in enumerate(lines, 1)
        if line.startswith(COMPLETED_PREFIX)
    )
    wait_text = accepted["wait_for_panes"]
    if not wait_text.startswith("[") or not wait_text.endswith("]"):
        raise ValueError(f"invalid fixture wait_for_panes={wait_text!r}")
    wait_panes = [int(value) for value in wait_text[1:-1].split(",") if value]
    _region_ready, region_total = _integer_pair(accepted["region_final"], "region_final")
    return validate_transaction_readiness(
        lines,
        actor="session-switcher",
        request=int(accepted["request"]),
        interaction=int(accepted["interaction"]),
        event_sequence=int(accepted["event_sequence"]),
        epoch=int(projected["epoch"]),
        target_revision=int(accepted["target_revision"]),
        region_total=region_total,
        wait_panes=wait_panes,
        projected_line=projected_line,
        promoted_line=promoted_line,
        completed_line=completed_line,
    )


def self_test() -> None:
    fixtures = pathlib.Path(__file__).with_name("fixtures") / "resize-transaction-readiness"
    _fixture_transaction(fixtures / "positive.txt")
    negatives = sorted(fixtures.glob("negative-*.txt"))
    if not negatives:
        raise SystemExit("resize readiness self-test has no negative fixtures")
    unexpectedly_valid = []
    for path in negatives:
        try:
            _fixture_transaction(path)
        except (StopIteration, KeyError, ValueError):
            continue
        unexpectedly_valid.append(path.name)
    if unexpectedly_valid:
        raise SystemExit(f"invalid resize readiness fixtures passed: {unexpectedly_valid}")
    print(f"resize transaction readiness fixtures passed: 1 positive, {len(negatives)} negative")


if __name__ == "__main__":
    if sys.argv[1:] != ["--self-test"]:
        raise SystemExit(f"usage: {sys.argv[0]} --self-test")
    self_test()
