#!/usr/bin/env python3
"""Verify one causally bound managed recovery scene and its client acceptance."""

from __future__ import annotations

import json
import pathlib
import re
import sys
from typing import Any, NamedTuple


PUBLISHED = re.compile(
    r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z  INFO "
    r"broodd::frame_export: published window scene "
    r"sequence=(\d+) panes=\[([0-9]+(?:, [0-9]+)*)\]$"
)
ACCEPTED = re.compile(
    r"^accepted ordinary managed frame "
    r"backend=(\d+) connection_generation=(\d+) pane=(\d+) "
    r"backend_pane=(\d+) sequence=(\d+) resource=(\d+)$"
)


class Route(NamedTuple):
    backend: int
    connection_generation: int
    pane: int
    backend_pane: int


def parse_route(value: str) -> Route:
    try:
        route = Route(*(int(part) for part in value.split(":")))
    except (TypeError, ValueError) as error:
        raise ValueError(
            f"route must be BACKEND:GENERATION:PANE:BACKEND_PANE, got {value!r}"
        ) from error
    return route


def verify(
    client_lines: list[str],
    server_lines: list[str],
    server_after_line: int,
    epoch: int,
    client_after_line: int,
    route_a: Route,
    route_b: Route,
) -> dict[str, Any]:
    expected_routes = {route_a, route_b}
    expected_backend_panes = {route_a.backend_pane, route_b.backend_pane}
    valid_inputs = (
        server_after_line >= 0
        and epoch > 0
        and client_after_line > 0
        and len(expected_routes) == 2
        and len(expected_backend_panes) == 2
        and route_a.backend == route_b.backend > 0
        and route_a.connection_generation == route_b.connection_generation > 0
        and route_a.pane > 0
        and route_b.pane > 0
        and route_a.pane != route_b.pane
        and route_a.backend_pane > 0
        and route_b.backend_pane > 0
    )

    scene_line = 0
    sequence = 0
    if valid_inputs:
        for line_number, text in enumerate(
            server_lines[server_after_line:], server_after_line + 1
        ):
            match = PUBLISHED.fullmatch(text)
            if match is None:
                continue
            current_sequence = int(match.group(1))
            panes = [int(value) for value in match.group(2).split(", ")]
            if (
                current_sequence > 0
                and len(panes) == 2
                and len(set(panes)) == 2
                and set(panes) == expected_backend_panes
            ):
                scene_line = line_number
                sequence = current_sequence
                break

    records: list[dict[str, int]] = []
    if sequence > 0:
        for line_number, text in enumerate(
            client_lines[client_after_line:], client_after_line + 1
        ):
            match = ACCEPTED.fullmatch(text)
            if match is None:
                continue
            backend, generation, pane, backend_pane, current_sequence, resource = map(
                int, match.groups()
            )
            if current_sequence != sequence:
                continue
            records.append(
                {
                    "line": line_number,
                    "backend": backend,
                    "connection_generation": generation,
                    "pane": pane,
                    "backend_pane": backend_pane,
                    "sequence": current_sequence,
                    "resource": resource,
                }
            )

    actual_routes = {
        Route(
            record["backend"],
            record["connection_generation"],
            record["pane"],
            record["backend_pane"],
        )
        for record in records
    }
    verified = (
        valid_inputs
        and scene_line > server_after_line
        and sequence > 0
        and len(records) == 2
        and len(actual_routes) == 2
        and actual_routes == expected_routes
        and all(record["line"] > client_after_line for record in records)
        and all(record["resource"] > 0 for record in records)
        and len({record["resource"] for record in records}) == 2
    )
    result: dict[str, Any] = {
        "verified": verified,
        "epoch": epoch,
        "server_after_line": server_after_line,
        "client_after_line": client_after_line,
        "scene_line": scene_line,
        "sequence": sequence,
        "expected_routes": [route_a._asdict(), route_b._asdict()],
        "records": records,
    }
    if verified:
        by_backend_pane = {record["backend_pane"]: record for record in records}
        record_a = by_backend_pane[route_a.backend_pane]
        record_b = by_backend_pane[route_b.backend_pane]
        result.update(
            {
                "line_a": record_a["line"],
                "line_b": record_b["line"],
                "resources": [record_a["resource"], record_b["resource"]],
            }
        )
    return result


def self_test() -> int:
    route_a = Route(1, 7, 101, 11)
    route_b = Route(1, 7, 102, 12)
    server = [
        "unrelated",
        "2026-08-24T06:40:13.226076Z  INFO broodd::frame_export: published window scene sequence=4 panes=[11, 12]",
    ]
    client = [
        "accepted ordinary managed frame backend=1 connection_generation=7 pane=101 backend_pane=11 sequence=4 resource=8",
        "degraded synchronized layout actor=session-switcher epoch=3",
        "accepted ordinary managed frame backend=1 connection_generation=7 pane=101 backend_pane=11 sequence=4 resource=9",
        "accepted ordinary managed frame backend=1 connection_generation=7 pane=102 backend_pane=12 sequence=4 resource=10",
    ]
    result = verify(client, server, 1, 3, 2, route_a, route_b)
    assert result["verified"]
    assert (result["scene_line"], result["line_a"], result["line_b"]) == (2, 3, 4)
    reversed_result = verify(
        client[:2] + list(reversed(client[2:])), server, 1, 3, 2, route_a, route_b
    )
    assert reversed_result["verified"]

    cases = {
        "wrong-sequence": (
            [line.replace("sequence=4", "sequence=5") for line in client],
            server,
            route_a,
            route_b,
        ),
        "wrong-pane": (
            client[:3] + [client[3].replace("backend_pane=12", "backend_pane=13")],
            server,
            route_a,
            route_b,
        ),
        "duplicate": (client[:3] + [client[2]], server, route_a, route_b),
        "missing": (client[:3], server, route_a, route_b),
        "wrong-expected-backend": (client, server, Route(2, 7, 101, 11), route_b),
        "wrong-expected-generation": (client, server, Route(1, 8, 101, 11), route_b),
        "swapped-mapping": (client, server, Route(1, 7, 102, 11), route_b),
        "accepted-backend-mismatch": (
            client[:3] + [client[3].replace("backend=1", "backend=2")],
            server,
            route_a,
            route_b,
        ),
        "accepted-generation-mismatch": (
            client[:3]
            + [client[3].replace("connection_generation=7", "connection_generation=8")],
            server,
            route_a,
            route_b,
        ),
        "zero-resource": (
            client[:3] + [client[3].replace("resource=10", "resource=0")],
            server,
            route_a,
            route_b,
        ),
        "duplicate-resource": (
            client[:3] + [client[3].replace("resource=10", "resource=9")],
            server,
            route_a,
            route_b,
        ),
        "pre-terminal-stale-only": (client[:2], server, route_a, route_b),
        "extra-scene-pane": (
            client,
            [server[0], server[1].replace("[11, 12]", "[11, 12, 13]")],
            route_a,
            route_b,
        ),
        "scene-full-line-junk": (
            client,
            [server[0], server[1] + " misleading"],
            route_a,
            route_b,
        ),
        "accept-full-line-junk": (
            client[:3] + [client[3] + " misleading"],
            server,
            route_a,
            route_b,
        ),
        "accept-field-order": (
            client[:3]
            + [
                client[3].replace(
                    "backend_pane=12 sequence=4", "sequence=4 backend_pane=12"
                )
            ],
            server,
            route_a,
            route_b,
        ),
    }
    for name, (case_client, case_server, case_a, case_b) in cases.items():
        assert not verify(case_client, case_server, 1, 3, 2, case_a, case_b)[
            "verified"
        ], name
    print(f"managed recovery acceptance self-test passed: 2 positive, {len(cases)} negative")
    return 0


def main() -> int:
    if sys.argv[1:] == ["--self-test"]:
        return self_test()
    if len(sys.argv) != 8:
        raise SystemExit(
            "usage: check-managed-recovery-acceptance.py CLIENT_LOG SERVER_LOG "
            "SERVER_AFTER_LINE EPOCH CLIENT_AFTER_LINE ROUTE_A ROUTE_B"
        )
    client_log = pathlib.Path(sys.argv[1])
    server_log = pathlib.Path(sys.argv[2])
    server_after_line, epoch, client_after_line = map(int, sys.argv[3:6])
    route_a, route_b = map(parse_route, sys.argv[6:8])
    print(
        json.dumps(
            verify(
                client_log.read_text(encoding="utf-8", errors="replace").splitlines(),
                server_log.read_text(encoding="utf-8", errors="replace").splitlines(),
                server_after_line,
                epoch,
                client_after_line,
                route_a,
                route_b,
            ),
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
