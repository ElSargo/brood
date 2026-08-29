#!/usr/bin/env python3
"""Verify the held tagged scene and its recovery-free managed supersession."""

from __future__ import annotations

import json
import pathlib
import re
import sys
from typing import Any


REQUEST = re.compile(
    r"^requested managed window resize window_id=(\d+) "
    r"logical_size=\d+x\d+ pixel_size=\d+x\d+ scale_120=\d+ "
    r"resize_epoch=(None|Some\((\d+)\)) recover_through_epoch=None$"
)
MANAGED_REQUEST = re.compile(
    r"^requested managed window resize window_id=(\d+) "
    r"logical_size=\d+x\d+ pixel_size=\d+x\d+ scale_120=\d+ "
    r"resize_epoch=(None|Some\((\d+)\)) "
    r"recover_through_epoch=(None|Some\((\d+)\))$"
)
ARMED = re.compile(
    r"^proof armed complete tagged managed scene backend=(\d+) "
    r"connection_generation=(\d+) epoch=(\d+) backend_panes=\{(\d+), (\d+)\}$"
)
HELD = re.compile(
    r"^proof held complete tagged managed scene backend=(\d+) "
    r"connection_generation=(\d+) epoch=(\d+) tagged_windows=(\d+) "
    r"backend_panes=\{(\d+), (\d+)\}$"
)
SNAPSHOT = re.compile(
    r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z  INFO "
    r"broodd::state: managed resize snapshot queued pane=(\d+) epoch=(\d+) "
    r"serial=(\d+) surface_commit=(\d+) logical_width=\d+ logical_height=\d+ "
    r"pixel_width=\d+ pixel_height=\d+ configure_to_snapshot_micros=\d+$"
)
CONFIGURE = re.compile(
    r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z  INFO "
    r"broodd::state: managed resize configure issued pane=(\d+) epoch=(\d+) "
    r"serial=(\d+) logical_width=\d+ logical_height=\d+ pixel_width=\d+ "
    r"pixel_height=\d+ scale_120=\d+$"
)
SUCCESSOR_ACCEPTED = re.compile(
    r"^accepted synchronized layout actor=session-switcher request=([1-9]\d*) "
    r"interaction=([1-9]\d*) event_sequence=(\d+) native_resize_revision=0 "
    r"epoch=(\d+) .+$"
)
RELEASED = re.compile(
    r"^proof released complete tagged managed scene held_epoch=(\d+) "
    r"active_epoch=(\d+)$"
)
DROPPED = re.compile(
    r"^dropped stale or unissued tagged managed frame backend=(\d+) "
    r"connection_generation=(\d+) pane=(\d+) backend_pane=(\d+) "
    r"resize_epoch=Some\((\d+)\)$"
)
ADMISSION = re.compile(
    r"^admitted managed resize expectation backend=(\d+) connection_generation=(\d+) "
    r"pane=(\d+) backend_pane=(\d+) epoch=(\d+) wait=true .+$"
)
STAGED = re.compile(
    r"^staged managed frame backend=(\d+) connection_generation=(\d+) "
    r"pane=(\d+) backend_pane=(\d+) epoch=(\d+) sequence=(\d+) "
    r"resource=(\d+) source=\d+x\d+ resource_extent=\d+x\d+$"
)
READY = re.compile(
    r"^synchronized layout ready actor=session-switcher request=([1-9]\d*) "
    r"interaction=(\d+) event_sequence=(\d+) epoch=(\d+) .+ "
    r"pane_ready=(\d+)/(\d+) .+ degraded=(true|false) duration_us=\d+$"
)
PROMOTED = re.compile(
    r"^resize-present interaction=(\d+) event_sequence=(\d+) epoch=(\d+) "
    r"divider_x=-?\d+ composition_size=\d+x\d+ phase=promoted "
    r"wait_ready=(\d+)/(\d+) staged_count=(\d+) managed_expectations=(\d+) "
    r"managed_staged_resources=(\d+) degraded=(true|false) superseded_epochs=\d+$"
)
PUBLISHED = re.compile(
    r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z  INFO "
    r"broodd::state: managed resize scene published pane=(\d+) epoch=(\d+) "
    r"surface_commit=(\d+) scene_sequence=(\d+) transport_resource=(\d+) "
    r"snapshot_to_publish_micros=\d+$"
)
PRESENTED = re.compile(
    r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z  INFO "
    r"broodd::state: managed resize scene presented pane=(\d+) epoch=(\d+) "
    r"surface_commit=(\d+) scene_sequence=(\d+) transport_resource=(\d+)$"
)
TERMINAL = (
    re.compile(
        r"^synchronized layout ready actor=session-switcher request=[1-9]\d* "
        r"interaction=\d+ event_sequence=\d+ epoch=(\d+)(?: .*)?$"
    ),
    re.compile(
        r"^(?:completed|degraded) synchronized layout actor=session-switcher "
        r"request=[1-9]\d* interaction=\d+ event_sequence=\d+ "
        r"native_resize_revision=\d+ epoch=(\d+)(?: .*)?$"
    ),
    re.compile(
        r"^aborted synchronized layout(?: for native window resize)? "
        r"actor=session-switcher request=[1-9]\d* epoch=(\d+)(?: .*)?$"
    ),
)


def terminal_epoch(text: str) -> int | None:
    for pattern in TERMINAL:
        match = pattern.fullmatch(text)
        if match is not None:
            return int(match.group(1))
    return None


def verify(
    client_lines: list[str],
    server_lines: list[str],
    client_after_line: int,
    accept_line: int,
    arm_line: int,
    held_line: int,
    epoch: int,
    pane_a: int,
    pane_b: int,
) -> dict[str, Any]:
    expected_panes = {pane_a, pane_b}
    valid_inputs = (
        0 <= client_after_line < accept_line < arm_line < held_line <= len(client_lines)
        and epoch > 0
        and pane_a > 0
        and pane_b > 0
        and pane_a != pane_b
    )
    arm_match = ARMED.fullmatch(client_lines[arm_line - 1]) if valid_inputs else None
    held_match = HELD.fullmatch(client_lines[held_line - 1]) if valid_inputs else None
    arm_panes = (
        {int(arm_match.group(4)), int(arm_match.group(5))}
        if arm_match is not None
        else set()
    )
    held_panes = (
        {int(held_match.group(5)), int(held_match.group(6))}
        if held_match is not None
        else set()
    )
    identity_ok = (
        arm_match is not None
        and held_match is not None
        and int(arm_match.group(1)) == int(held_match.group(1)) > 0
        and int(arm_match.group(2)) == int(held_match.group(2)) > 0
        and int(arm_match.group(3)) == int(held_match.group(3)) == epoch
        and int(held_match.group(4)) == 2
        and arm_panes == held_panes == expected_panes
    )

    tagged: list[dict[str, int]] = []
    bare: list[int] = []
    terminal: list[int] = []
    if valid_inputs:
        for line_number in range(client_after_line + 1, held_line + 1):
            text = client_lines[line_number - 1]
            request = REQUEST.fullmatch(text)
            if request is not None and int(request.group(1)) in expected_panes:
                if request.group(2) == "None":
                    bare.append(line_number)
                elif int(request.group(3)) == epoch:
                    tagged.append(
                        {"line": line_number, "pane": int(request.group(1))}
                    )
            completion_epoch = terminal_epoch(text)
            if completion_epoch == epoch:
                terminal.append(line_number)

    snapshots: list[dict[str, int]] = []
    for line_number, text in enumerate(server_lines, 1):
        match = SNAPSHOT.fullmatch(text)
        if match is not None and int(match.group(2)) == epoch:
            snapshots.append({"line": line_number, "pane": int(match.group(1))})

    verified = (
        valid_inputs
        and identity_ok
        and not bare
        and not terminal
        and len(tagged) == 2
        and {record["pane"] for record in tagged} == expected_panes
        and len(snapshots) == 2
        and {record["pane"] for record in snapshots} == expected_panes
    )
    return {
        "verified": verified,
        "epoch": epoch,
        "client_after_line": client_after_line,
        "accept_line": accept_line,
        "arm_line": arm_line,
        "held_line": held_line,
        "backend_panes": sorted(expected_panes),
        "tagged_requests": tagged,
        "bare_requests": bare,
        "terminal_before_hold": terminal,
        "snapshots": snapshots,
    }


def verify_successor(
    client_lines: list[str],
    server_lines: list[str],
    held_line: int,
    accept_line: int,
    release_line: int,
    promoted_line: int,
    old_epoch: int,
    new_epoch: int,
    pane_a: int,
    pane_b: int,
    view_a: int,
    view_b: int,
) -> dict[str, Any]:
    backend_panes = {pane_a, pane_b}
    view_panes = {view_a, view_b}
    expected_routes = {(view_a, pane_a), (view_b, pane_b)}
    valid_inputs = (
        0 < held_line < accept_line < release_line < promoted_line <= len(client_lines)
        and 0 < old_epoch < new_epoch
        and len(backend_panes) == len(view_panes) == 2
        and min(backend_panes | view_panes) > 0
    )
    accepted = (
        SUCCESSOR_ACCEPTED.fullmatch(client_lines[accept_line - 1])
        if valid_inputs
        else None
    )
    released = (
        RELEASED.fullmatch(client_lines[release_line - 1]) if valid_inputs else None
    )
    promoted = (
        PROMOTED.fullmatch(client_lines[promoted_line - 1]) if valid_inputs else None
    )
    held = HELD.fullmatch(client_lines[held_line - 1]) if valid_inputs else None
    held_backend = int(held.group(1)) if held is not None else 0
    held_generation = int(held.group(2)) if held is not None else 0
    held_identity_ok = (
        held is not None
        and held_backend > 0
        and held_generation > 0
        and int(held.group(3)) == old_epoch
        and int(held.group(4)) == 2
        and {int(held.group(5)), int(held.group(6))} == backend_panes
    )

    tagged: list[dict[str, int]] = []
    forbidden_recovery: list[dict[str, int | str]] = []
    drops: list[dict[str, int]] = []
    successor_drops: list[dict[str, int]] = []
    admissions: list[dict[str, int]] = []
    staged: list[dict[str, int]] = []
    ready_records: list[dict[str, int | bool]] = []
    early_terminals: list[dict[str, int | str]] = []
    if valid_inputs:
        for line_number in range(held_line + 1, promoted_line + 1):
            text = client_lines[line_number - 1]
            request = MANAGED_REQUEST.fullmatch(text)
            if (
                line_number < promoted_line
                and request is not None
                and int(request.group(1)) in view_panes
            ):
                pane = int(request.group(1))
                if request.group(2) == f"Some({new_epoch})" and request.group(4) == "None":
                    tagged.append({"line": line_number, "pane": pane})
                else:
                    forbidden_recovery.append(
                        {
                            "line": line_number,
                            "pane": pane,
                            "resize_epoch": request.group(2),
                            "recover_through_epoch": request.group(4),
                        }
                    )
            dropped = DROPPED.fullmatch(text)
            if dropped is not None:
                record = {
                    "line": line_number,
                    "backend": int(dropped.group(1)),
                    "connection_generation": int(dropped.group(2)),
                    "pane": int(dropped.group(3)),
                    "backend_pane": int(dropped.group(4)),
                }
                if int(dropped.group(5)) == old_epoch:
                    drops.append(record)
                elif int(dropped.group(5)) == new_epoch:
                    successor_drops.append(record)
            admission = ADMISSION.fullmatch(text)
            if admission is not None and int(admission.group(5)) == new_epoch:
                admissions.append(
                    {
                        "line": line_number,
                        "backend": int(admission.group(1)),
                        "connection_generation": int(admission.group(2)),
                        "pane": int(admission.group(3)),
                        "backend_pane": int(admission.group(4)),
                    }
                )
            staged_frame = STAGED.fullmatch(text)
            if staged_frame is not None and int(staged_frame.group(5)) == new_epoch:
                staged.append(
                    {
                        "line": line_number,
                        "backend": int(staged_frame.group(1)),
                        "connection_generation": int(staged_frame.group(2)),
                        "pane": int(staged_frame.group(3)),
                        "backend_pane": int(staged_frame.group(4)),
                        "sequence": int(staged_frame.group(6)),
                        "resource": int(staged_frame.group(7)),
                    }
                )
            ready = READY.fullmatch(text)
            if ready is not None and int(ready.group(4)) == new_epoch:
                ready_records.append(
                    {
                        "line": line_number,
                        "request": int(ready.group(1)),
                        "interaction": int(ready.group(2)),
                        "event_sequence": int(ready.group(3)),
                        "ready": int(ready.group(5)),
                        "total": int(ready.group(6)),
                        "degraded": ready.group(7) == "true",
                    }
                )
            if terminal_epoch(text) == new_epoch and ready is None:
                early_terminals.append(
                    {
                        "line": line_number,
                        "kind": text.split(maxsplit=1)[0],
                    }
                )

    configures: list[dict[str, int]] = []
    successor_snapshots: list[dict[str, int]] = []
    published: list[dict[str, int]] = []
    presented: list[dict[str, int]] = []
    for line_number, text in enumerate(server_lines, 1):
        configured = CONFIGURE.fullmatch(text)
        if configured is not None and int(configured.group(2)) == new_epoch:
            configures.append(
                {
                    "line": line_number,
                    "pane": int(configured.group(1)),
                    "epoch": int(configured.group(2)),
                    "serial": int(configured.group(3)),
                }
            )
        snapshot = SNAPSHOT.fullmatch(text)
        if snapshot is not None and int(snapshot.group(2)) == new_epoch:
            successor_snapshots.append(
                {
                    "line": line_number,
                    "pane": int(snapshot.group(1)),
                    "epoch": int(snapshot.group(2)),
                    "serial": int(snapshot.group(3)),
                    "surface_commit": int(snapshot.group(4)),
                }
            )
        scene_published = PUBLISHED.fullmatch(text)
        if scene_published is not None and int(scene_published.group(2)) == new_epoch:
            published.append(
                {
                    "line": line_number,
                    "pane": int(scene_published.group(1)),
                    "epoch": int(scene_published.group(2)),
                    "surface_commit": int(scene_published.group(3)),
                    "scene_sequence": int(scene_published.group(4)),
                    "transport_resource": int(scene_published.group(5)),
                }
            )
        scene_presented = PRESENTED.fullmatch(text)
        if scene_presented is not None and int(scene_presented.group(2)) == new_epoch:
            presented.append(
                {
                    "line": line_number,
                    "pane": int(scene_presented.group(1)),
                    "epoch": int(scene_presented.group(2)),
                    "surface_commit": int(scene_presented.group(3)),
                    "scene_sequence": int(scene_presented.group(4)),
                    "transport_resource": int(scene_presented.group(5)),
                }
            )

    configure_by_pane = {record["pane"]: record for record in configures}
    snapshot_by_pane = {record["pane"]: record for record in successor_snapshots}
    published_by_pane = {record["pane"]: record for record in published}
    presented_by_pane = {record["pane"]: record for record in presented}
    staged_by_backend_pane = {record["backend_pane"]: record for record in staged}
    server_identity_chain_ok = all(
        pane in configure_by_pane
        and pane in snapshot_by_pane
        and pane in published_by_pane
        and pane in presented_by_pane
        and configure_by_pane[pane]["serial"] == snapshot_by_pane[pane]["serial"]
        and snapshot_by_pane[pane]["surface_commit"]
        == published_by_pane[pane]["surface_commit"]
        and {
            key: published_by_pane[pane][key]
            for key in (
                "pane",
                "epoch",
                "surface_commit",
                "scene_sequence",
                "transport_resource",
            )
        }
        == {
            key: presented_by_pane[pane][key]
            for key in (
                "pane",
                "epoch",
                "surface_commit",
                "scene_sequence",
                "transport_resource",
            )
        }
        and configure_by_pane[pane]["line"]
        < snapshot_by_pane[pane]["line"]
        < published_by_pane[pane]["line"]
        < presented_by_pane[pane]["line"]
        for pane in backend_panes
    )
    server_phase_order_ok = (
        bool(configures and successor_snapshots and published and presented)
        and max(record["line"] for record in configures)
        < min(record["line"] for record in successor_snapshots)
        and max(record["line"] for record in successor_snapshots)
        < min(record["line"] for record in published)
        and max(record["line"] for record in published)
        < min(record["line"] for record in presented)
    )

    # Every same-pane request between successor staging and its exact tagged
    # admission must itself be that successor tag. This rejects both bare and
    # scoped recovery traffic; there is no runtime requirement to manufacture
    # an obsolete request merely so the server can reject it.
    request_window = tagged + forbidden_recovery
    tagged_successor = tagged
    verified = (
        valid_inputs
        and held_identity_ok
        and accepted is not None
        and int(accepted.group(4)) == new_epoch
        and released is not None
        and (int(released.group(1)), int(released.group(2))) == (old_epoch, new_epoch)
        and promoted is not None
        and int(promoted.group(3)) == new_epoch
        and int(promoted.group(4)) == int(promoted.group(5)) == 4
        and int(promoted.group(6)) == 0
        and int(promoted.group(7)) == 0
        and int(promoted.group(8)) == 0
        and promoted.group(9) == "false"
        and not forbidden_recovery
        and len(request_window) == 2
        and len(tagged_successor) == 2
        and {record["pane"] for record in tagged_successor} == view_panes
        and len(admissions) == 2
        and {(record["pane"], record["backend_pane"]) for record in admissions}
        == expected_routes
        and all(
            (record["backend"], record["connection_generation"])
            == (held_backend, held_generation)
            for record in admissions
        )
        and len(drops) == 2
        and {(record["pane"], record["backend_pane"]) for record in drops}
        == expected_routes
        and all(
            (record["backend"], record["connection_generation"])
            == (held_backend, held_generation)
            for record in drops
        )
        and all(
            max(record["line"] for record in drops) < record["line"] < promoted_line
            for record in admissions
        )
        and all(
            max(record["line"] for record in admissions) < record["line"] < promoted_line
            for record in tagged_successor
        )
        and len(staged) == 2
        and {(record["pane"], record["backend_pane"]) for record in staged}
        == expected_routes
        and all(
            (record["backend"], record["connection_generation"])
            == (held_backend, held_generation)
            for record in staged
        )
        and all(
            pane in staged_by_backend_pane
            and pane in published_by_pane
            and (
                staged_by_backend_pane[pane]["sequence"],
                staged_by_backend_pane[pane]["resource"],
            )
            == (
                published_by_pane[pane]["scene_sequence"],
                published_by_pane[pane]["transport_resource"],
            )
            for pane in backend_panes
        )
        and len(configures) == 2
        and {record["pane"] for record in configures} == backend_panes
        and len(successor_snapshots) == 2
        and {record["pane"] for record in successor_snapshots} == backend_panes
        and all(release_line < record["line"] < min(record["line"] for record in admissions) for record in drops)
        and not successor_drops
        and len(ready_records) == 1
        and ready_records[0]["ready"] == ready_records[0]["total"] == 4
        and ready_records[0]["degraded"] is False
        and (
            ready_records[0]["request"],
            ready_records[0]["interaction"],
            ready_records[0]["event_sequence"],
        )
        == (
            int(accepted.group(1)),
            int(accepted.group(2)),
            int(accepted.group(3)),
        )
        and (int(promoted.group(1)), int(promoted.group(2)))
        == (int(accepted.group(2)), int(accepted.group(3)))
        and max(record["line"] for record in tagged_successor)
        < min(record["line"] for record in staged)
        and max(record["line"] for record in staged)
        < ready_records[0]["line"]
        < promoted_line
        and not early_terminals
        and len(published) == 2
        and {record["pane"] for record in published} == backend_panes
        and len(presented) == 2
        and {record["pane"] for record in presented} == backend_panes
        and len({record["scene_sequence"] for record in published}) == 1
        and len({record["transport_resource"] for record in published}) == 2
        and server_identity_chain_ok
        and server_phase_order_ok
    )
    return {
        "verified": verified,
        "old_epoch": old_epoch,
        "new_epoch": new_epoch,
        "tagged_requests": tagged_successor,
        "forbidden_recovery": forbidden_recovery,
        "admissions": admissions,
        "staged": staged,
        "configures": configures,
        "snapshots": successor_snapshots,
        "drops": drops,
        "successor_drops": successor_drops,
        "ready": ready_records,
        "early_terminals": early_terminals,
        "published": published,
        "presented": presented,
        "release_line": release_line,
        "promoted_line": promoted_line,
    }


def self_test() -> int:
    client = [
        "accepted synchronized layout actor=session-switcher epoch=7",
        "proof armed complete tagged managed scene backend=1 connection_generation=2 epoch=7 backend_panes={11, 12}",
        "requested managed window resize window_id=12 logical_size=20x30 pixel_size=40x60 scale_120=240 resize_epoch=Some(7) recover_through_epoch=None",
        "requested managed window resize window_id=11 logical_size=20x30 pixel_size=40x60 scale_120=240 resize_epoch=Some(7) recover_through_epoch=None",
        "proof held complete tagged managed scene backend=1 connection_generation=2 epoch=7 tagged_windows=2 backend_panes={11, 12}",
    ]
    server = [
        "2026-08-24T07:27:01.339297Z  INFO broodd::state: managed resize snapshot queued pane=12 epoch=7 serial=33 surface_commit=9 logical_width=20 logical_height=30 pixel_width=40 pixel_height=60 configure_to_snapshot_micros=88",
        "2026-08-24T07:27:01.511992Z  INFO broodd::state: managed resize snapshot queued pane=11 epoch=7 serial=32 surface_commit=9 logical_width=20 logical_height=30 pixel_width=40 pixel_height=60 configure_to_snapshot_micros=180",
    ]
    assert verify(client, server, 0, 1, 2, 5, 7, 11, 12)["verified"]

    mutations = {
        "bare": client[:2]
        + [
            "requested managed window resize window_id=11 logical_size=20x30 pixel_size=40x60 scale_120=240 resize_epoch=None recover_through_epoch=None"
        ]
        + client[2:],
        "missing-tagged": client[:3] + client[4:],
        "duplicate-tagged": client[:3] + [client[2]] + client[4:],
        "wrong-tagged-pane": client[:3]
        + [client[3].replace("window_id=11", "window_id=13")]
        + client[4:],
        "terminal-before-hold": client[:4]
        + [
            "degraded synchronized layout actor=session-switcher request=1 interaction=2 event_sequence=3 native_resize_revision=0 epoch=7 target_revision=4"
        ]
        + client[4:],
        "abort-before-hold": client[:4]
        + [
            "aborted synchronized layout actor=session-switcher request=1 epoch=7 reason=route lost"
        ]
        + client[4:],
        "native-abort-before-hold": client[:4]
        + [
            "aborted synchronized layout for native window resize actor=session-switcher request=1 epoch=7 reason=native resize"
        ]
        + client[4:],
        "abort-trailing-misleading-epoch": client[:4]
        + [
            "aborted synchronized layout actor=session-switcher request=1 epoch=7 reason=stale message epoch=8"
        ]
        + client[4:],
    }
    for name, lines in mutations.items():
        held = len(lines)
        assert not verify(lines, server, 0, 1, 2, held, 7, 11, 12)["verified"], name

    server_mutations = {
        "missing-snapshot": server[:1],
        "duplicate-snapshot": server + [server[0]],
        "wrong-snapshot-pane": [server[0], server[1].replace("pane=11", "pane=13")],
        "snapshot-junk": [server[0], server[1] + " misleading"],
    }
    for name, lines in server_mutations.items():
        assert not verify(client, lines, 0, 1, 2, 5, 7, 11, 12)["verified"], name

    assert not verify(client, server, 0, 2, 1, 5, 7, 11, 12)["verified"]
    for abort_after_hold in [
        "aborted synchronized layout actor=session-switcher request=1 epoch=7 reason=after hold",
        "aborted synchronized layout for native window resize actor=session-switcher request=1 epoch=7 reason=after hold",
    ]:
        assert verify(
            client + [abort_after_hold], server, 0, 1, 2, 5, 7, 11, 12
        )["verified"]
    successor_client = [
        client[-1],
        "accepted synchronized layout actor=session-switcher request=2 interaction=3 event_sequence=4 native_resize_revision=0 epoch=8 target_revision=9",
        "proof released complete tagged managed scene held_epoch=7 active_epoch=8",
        "dropped stale or unissued tagged managed frame backend=1 connection_generation=2 pane=101 backend_pane=11 resize_epoch=Some(7)",
        "dropped stale or unissued tagged managed frame backend=1 connection_generation=2 pane=102 backend_pane=12 resize_epoch=Some(7)",
        "admitted managed resize expectation backend=1 connection_generation=2 pane=101 backend_pane=11 epoch=8 wait=true logical_size=20x30 pixel_size=40x60 scale_120=240",
        "admitted managed resize expectation backend=1 connection_generation=2 pane=102 backend_pane=12 epoch=8 wait=true logical_size=20x30 pixel_size=40x60 scale_120=240",
        "requested managed window resize window_id=101 logical_size=20x30 pixel_size=40x60 scale_120=240 resize_epoch=Some(8) recover_through_epoch=None",
        "requested managed window resize window_id=102 logical_size=20x30 pixel_size=40x60 scale_120=240 resize_epoch=Some(8) recover_through_epoch=None",
        "staged managed frame backend=1 connection_generation=2 pane=101 backend_pane=11 epoch=8 sequence=4 resource=70 source=40x60 resource_extent=40x60",
        "staged managed frame backend=1 connection_generation=2 pane=102 backend_pane=12 epoch=8 sequence=4 resource=71 source=40x60 resource_extent=40x60",
        "synchronized layout ready actor=session-switcher request=2 interaction=3 event_sequence=4 epoch=8 target_revision=9 region_final=1/1 pane_ready=4/4 recovered_regions=[] recovered_count=0 recovered_managed={} recovered_managed_content={} missing=[] degraded=false duration_us=120000",
        "resize-present interaction=3 event_sequence=4 epoch=8 divider_x=20 composition_size=40x60 phase=promoted wait_ready=4/4 staged_count=0 managed_expectations=0 managed_staged_resources=0 degraded=false superseded_epochs=1",
    ]
    successor_server = [
        "2026-08-24T07:27:01.339297Z  INFO broodd::state: managed resize configure issued pane=12 epoch=8 serial=33 logical_width=20 logical_height=30 pixel_width=40 pixel_height=60 scale_120=240",
        "2026-08-24T07:27:01.511992Z  INFO broodd::state: managed resize configure issued pane=11 epoch=8 serial=32 logical_width=20 logical_height=30 pixel_width=40 pixel_height=60 scale_120=240",
        "2026-08-24T07:27:01.539297Z  INFO broodd::state: managed resize snapshot queued pane=12 epoch=8 serial=33 surface_commit=10 logical_width=20 logical_height=30 pixel_width=40 pixel_height=60 configure_to_snapshot_micros=88",
        "2026-08-24T07:27:01.551992Z  INFO broodd::state: managed resize snapshot queued pane=11 epoch=8 serial=32 surface_commit=9 logical_width=20 logical_height=30 pixel_width=40 pixel_height=60 configure_to_snapshot_micros=180",
        "2026-08-24T07:27:01.611992Z  INFO broodd::state: managed resize scene published pane=11 epoch=8 surface_commit=9 scene_sequence=4 transport_resource=70 snapshot_to_publish_micros=180",
        "2026-08-24T07:27:01.611993Z  INFO broodd::state: managed resize scene published pane=12 epoch=8 surface_commit=10 scene_sequence=4 transport_resource=71 snapshot_to_publish_micros=181",
        "2026-08-24T07:27:01.711992Z  INFO broodd::state: managed resize scene presented pane=11 epoch=8 surface_commit=9 scene_sequence=4 transport_resource=70",
        "2026-08-24T07:27:01.711993Z  INFO broodd::state: managed resize scene presented pane=12 epoch=8 surface_commit=10 scene_sequence=4 transport_resource=71",
    ]
    successor_args = (1, 2, 3, 13, 7, 8, 11, 12, 101, 102)
    assert verify_successor(
        successor_client, successor_server, *successor_args
    )["verified"]
    successor_mutations = {
        "held-junk": [successor_client[0] + " junk"] + successor_client[1:],
        "client-backend-generation-swapped": successor_client[:1]
        + [
            line.replace("backend=1", "backend=9").replace(
                "connection_generation=2", "connection_generation=8"
            )
            for line in successor_client[1:]
        ],
        "bare-recovery": successor_client[:7]
        + [
            "requested managed window resize window_id=101 logical_size=20x30 pixel_size=40x60 scale_120=240 resize_epoch=None recover_through_epoch=None"
        ]
        + successor_client[7:],
        "scoped-recovery": successor_client[:7]
        + [
            "requested managed window resize window_id=101 logical_size=20x30 pixel_size=40x60 scale_120=240 resize_epoch=None recover_through_epoch=Some(7)"
        ]
        + successor_client[7:],
        "duplicate-tag": successor_client[:9] + [successor_client[8]] + successor_client[9:],
        "missing-tag": successor_client[:8] + successor_client[9:],
        "missing-stage": successor_client[:10] + successor_client[11:],
        "duplicate-stage": successor_client[:11]
        + [successor_client[10]]
        + successor_client[11:],
        "wrong-stage-route": successor_client[:10]
        + [successor_client[10].replace("backend_pane=12", "backend_pane=13")]
        + successor_client[11:],
        "staged-sequence-mismatch": successor_client[:10]
        + [successor_client[10].replace("sequence=4", "sequence=5")]
        + successor_client[11:],
        "staged-resource-mismatch": successor_client[:10]
        + [successor_client[10].replace("resource=71", "resource=72")]
        + successor_client[11:],
        "missing-admission": successor_client[:6] + successor_client[7:],
        "wrong-admission": successor_client[:6]
        + [successor_client[6].replace("backend_pane=12", "backend_pane=13")]
        + successor_client[7:],
        "wrong-drop": successor_client[:4]
        + [successor_client[4].replace("backend_pane=12", "backend_pane=13")]
        + successor_client[5:],
        "missing-drop": successor_client[:4] + successor_client[5:],
        "cross-swapped-drops": successor_client[:3]
        + [
            successor_client[3].replace("backend_pane=11", "backend_pane=12"),
            successor_client[4].replace("backend_pane=12", "backend_pane=11"),
        ]
        + successor_client[5:],
        "admission-before-drop": successor_client[:3]
        + successor_client[5:7]
        + successor_client[3:5]
        + successor_client[7:],
        "partial-ready": successor_client[:-2]
        + [successor_client[-2].replace("pane_ready=4/4", "pane_ready=3/4")]
        + successor_client[-1:],
        "ready-identities-changed": successor_client[:-2]
        + [
            successor_client[-2]
            .replace("request=2", "request=9")
            .replace("interaction=3", "interaction=8")
            .replace("event_sequence=4", "event_sequence=7")
        ]
        + successor_client[-1:],
        "promoted-identities-changed": successor_client[:-1]
        + [
            successor_client[-1]
            .replace("interaction=3", "interaction=8")
            .replace("event_sequence=4", "event_sequence=7")
        ],
        "early-terminal": successor_client[:-2]
        + [
            "degraded synchronized layout actor=session-switcher request=2 interaction=3 event_sequence=4 native_resize_revision=0 epoch=8 target_revision=9 region_final=1/1 pane_ready=3/4 missing=[12] duration_us=100"
        ]
        + successor_client[-2:],
        "early-abort": successor_client[:-2]
        + [
            "aborted synchronized layout actor=session-switcher request=2 epoch=8 reason=route lost"
        ]
        + successor_client[-2:],
        "successor-drop": successor_client[:-2]
        + [
            "dropped stale or unissued tagged managed frame backend=1 connection_generation=2 pane=101 backend_pane=11 resize_epoch=Some(8)"
        ]
        + successor_client[-2:],
        "dirty-stage": successor_client[:-1]
        + [successor_client[-1].replace("staged_count=0", "staged_count=1")],
        "dirty-expectation": successor_client[:-1]
        + [
            successor_client[-1].replace(
                "managed_expectations=0", "managed_expectations=1"
            )
        ],
        "dirty-resource": successor_client[:-1]
        + [
            successor_client[-1].replace(
                "managed_staged_resources=0", "managed_staged_resources=1"
            )
        ],
        "degraded": successor_client[:-1]
        + [successor_client[-1].replace("degraded=false", "degraded=true")],
    }
    for name, lines in successor_mutations.items():
        release_line = next(
            (index for index, text in enumerate(lines, 1) if RELEASED.fullmatch(text)), 0
        )
        promoted_line = len(lines)
        assert not verify_successor(
            lines,
            successor_server,
            1,
            2,
            release_line,
            promoted_line,
            7,
            8,
            11,
            12,
            101,
            102,
        )["verified"], name
    for server_name, lines in {
        "missing-configure": successor_server[1:],
        "duplicate-configure": successor_server + [successor_server[0]],
        "missing-snapshot": successor_server[:2] + successor_server[3:],
        "wrong-snapshot-serial": successor_server[:2]
        + [successor_server[2].replace("serial=33", "serial=34")]
        + successor_server[3:],
        "missing-publish": successor_server[:4] + successor_server[5:],
        "missing-present": successor_server[:-1],
        "wrong-publish-pane": successor_server[:4]
        + [successor_server[4].replace("pane=11", "pane=13")]
        + successor_server[5:],
        "configure-after-publish": [
            successor_server[index] for index in [0, 2, 3, 4, 1, 5, 6, 7]
        ],
        "present-before-publish": [
            successor_server[index] for index in [0, 1, 2, 3, 6, 4, 5, 7]
        ],
        "pane-chains-interleaved": [
            successor_server[index] for index in [1, 3, 4, 0, 2, 5, 6, 7]
        ],
        "cross-pane-publish-present-interleaved": [
            successor_server[index] for index in [0, 1, 2, 3, 4, 6, 5, 7]
        ],
        "present-commit-mismatch": successor_server[:6]
        + [successor_server[6].replace("surface_commit=9", "surface_commit=8")]
        + successor_server[7:],
        "present-sequence-mismatch": successor_server[:6]
        + [successor_server[6].replace("scene_sequence=4", "scene_sequence=5")]
        + successor_server[7:],
        "present-resource-mismatch": successor_server[:6]
        + [successor_server[6].replace("transport_resource=70", "transport_resource=72")]
        + successor_server[7:],
        "snapshot-publish-commit-mismatch": successor_server[:4]
        + [successor_server[4].replace("surface_commit=9", "surface_commit=8")]
        + successor_server[5:6]
        + [successor_server[6].replace("surface_commit=9", "surface_commit=8")]
        + successor_server[7:],
        "cross-pane-sequence-mismatch": successor_server[:4]
        + [successor_server[4].replace("scene_sequence=4", "scene_sequence=5")]
        + successor_server[5:6]
        + [successor_server[6].replace("scene_sequence=4", "scene_sequence=5")]
        + successor_server[7:],
        "duplicate-resource-identity": successor_server[:4]
        + [successor_server[4].replace("transport_resource=70", "transport_resource=71")]
        + successor_server[5:6]
        + [successor_server[6].replace("transport_resource=70", "transport_resource=71")]
        + successor_server[7:],
        "duplicate-snapshot": successor_server + [successor_server[2]],
        "duplicate-publish": successor_server + [successor_server[4]],
        "duplicate-present": successor_server + [successor_server[6]],
        "published-junk": successor_server[:4]
        + [successor_server[4] + " junk"]
        + successor_server[5:],
    }.items():
        assert not verify_successor(
            successor_client, lines, *successor_args
        )["verified"], server_name

    print(json.dumps({"verified": True, "cases": 69}, sort_keys=True))
    return 0


def main(argv: list[str]) -> int:
    if argv == ["--self-test"]:
        return self_test()
    if len(argv) == 13 and argv[0] == "--verify-successor":
        client_path, server_path = map(pathlib.Path, argv[1:3])
        result = verify_successor(
            client_path.read_text(encoding="utf-8", errors="replace").splitlines(),
            server_path.read_text(encoding="utf-8", errors="replace").splitlines(),
            *(int(value) for value in argv[3:]),
        )
        print(json.dumps(result, sort_keys=True))
        return 0 if result["verified"] else 1
    if len(argv) != 9:
        print(
            "usage: check-managed-proof-hold.py CLIENT_LOG SERVER_LOG "
            "CLIENT_AFTER_LINE ACCEPT_LINE ARM_LINE HELD_LINE EPOCH PANE_A PANE_B",
            file=sys.stderr,
        )
        return 2
    client_path, server_path = map(pathlib.Path, argv[:2])
    result = verify(
        client_path.read_text(encoding="utf-8", errors="replace").splitlines(),
        server_path.read_text(encoding="utf-8", errors="replace").splitlines(),
        *(int(value) for value in argv[2:]),
    )
    print(json.dumps(result, sort_keys=True))
    return 0 if result["verified"] else 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
