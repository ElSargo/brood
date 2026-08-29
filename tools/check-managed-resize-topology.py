#!/usr/bin/env python3
"""Validate setup geometry and one exact managed resize transaction."""

from __future__ import annotations

import argparse
import json
import pathlib
import re
from typing import Any


REST_GRANT = re.compile(
    r"^granted rest layout lease parent=(?P<parent>\d+) child=(?P<child>\d+) "
    r"actor=(?P<actor>\S+) panes=(?P<panes>\d+) pane_ids=\[(?P<pane_ids>[^]]*)\]$"
)
ACTOR_START = re.compile(
    r"^started presentation WASM actor id=(?P<actor>status-bar|session-switcher|resize-proof-slow) "
    r".* generation=(?P<generation>\d+) pane=(?P<pane>\d+) "
)
ACTOR_PROMOTION = re.compile(
    r"^render scheduling actor-frame promoted "
    r"actor=(?P<actor>status-bar|session-switcher|resize-proof-slow) "
    r"generation=(?P<generation>\d+) token=\d+ revision=\d+ "
    r"pane_count=(?P<pane_count>\d+) panes=\[(?P<panes>[^]]*)\]$"
)
ROOT_CLAIM = re.compile(
    r"^granted claimed root layout lease actor=(?P<actor>\S+) "
    r"actor_generation=(?P<generation>\d+) lease_generation=\d+$"
)
ORDINARY_FRAME = re.compile(
    r"^accepted ordinary managed frame backend=(?P<backend>\d+) "
    r"connection_generation=(?P<generation>\d+) pane=(?P<pane>\d+) "
    r"backend_pane=(?P<backend_pane>\d+) sequence=\d+ resource=\d+$"
)
BACKEND_GROUP = re.compile(
    r"^layout-state backend groups lease=(?P<lease>\d+) "
    r"actor=(?P<actor>\S+) backends=\[(?P<backends>[^]]*)\]$"
)
LEASED_LAYOUT = re.compile(
    r"^accepted leased actor layout items=(?P<items>\d+) panes=(?P<panes>\d+) "
    r"revision=(?P<revision>\d+) pending_presented_layout_revision=(?P<pending>\d+) "
    r"pane_rects=\[(?P<pane_rects>[^]]*)\]$"
)
LEASED_LAYOUT_PREFIX = re.compile(r"^accepted leased actor layout ")
PRESENTED_LAYOUT = re.compile(r"^presented committed layout revision=(?P<revision>\d+)$")
PRESENTED_LAYOUT_PREFIX = re.compile(r"^presented committed layout ")
SETTLED_PRESENT = re.compile(
    r"^resize-present interaction=0 event_sequence=0 epoch=0 divider_x=-?\d+ "
    r"composition_size=(?P<width>\d+)x(?P<height>\d+) phase=settled "
    r"wait_ready=0/0 staged_count=0 managed_expectations=0 managed_staged_resources=0 "
    r"degraded=false superseded_epochs=\d+$"
)
SETUP_QUIESCENT = re.compile(
    r"^setup layout quiescent presented_layout_revision=(?P<presented>\d+) "
    r"inventory_revision=(?P<inventory>\d+) native_resize_pending=false "
    r"window_resize_pending=false synchronized_layout_active=false staged_layouts=0 "
    r"pending_presented_layout_revision=none layout_state_dirty=false "
    r"pending_terminal_notifications=0 deferred_actor_host_messages=0 "
    r"runtime_outbound_host_messages=0 "
    r"runtime_pending_actor_inputs=0 runtime_observer_output_backlog=false "
    r"layout_publications_current=true$"
)
SETUP_QUIESCENT_PREFIX = re.compile(r"^setup layout quiescent ")
ACCEPTED = re.compile(
    r"^accepted synchronized layout actor=(?P<actor>\S+) request=(?P<request>\d+) "
    r"interaction=(?P<interaction>\d+) event_sequence=(?P<event_sequence>\d+) "
    r"native_resize_revision=(?P<native_resize_revision>\d+) epoch=(?P<epoch>\d+) "
    r"base_revision=(?P<base_revision>\d+) target_revision=(?P<target_revision>\d+) "
    r".* pane_ready=0/(?P<pane_total>\d+) wait=(?P<wait>\d+) .* "
    r"actor_generation=(?P<actor_generation>\d+) .* "
    r"wait_for_panes=\[(?P<wait_panes>[^]]*)\] "
    r"target_rects=\[(?P<target_rects>[^]]*)\]$"
)
ACCEPTED_PREFIX = re.compile(r"^accepted synchronized layout ")
REGION_ISSUED = re.compile(
    r"^layout-region issued epoch=(?P<epoch>\d+) lease=(?P<lease>\d+) "
    r"lease_generation=\d+ holder=(?P<holder>\S+) holder_generation=\d+ "
    r"source=\S+ target=(?P<target>-?\d+:-?\d+:\d+:\d+) "
)
PANE_NOTIFIED = re.compile(
    r"^layout-region pane-notified epoch=(?P<epoch>\d+) pane=(?P<pane>\d+) "
    r"owner_lease=(?P<owner>\d+) wait=(?P<wait>true|false) "
)
MANAGED_EXPECTATION = re.compile(
    r"^admitted managed resize expectation backend=(?P<backend>\d+) "
    r"connection_generation=(?P<connection_generation>\d+) pane=(?P<pane>\d+) "
    r"backend_pane=(?P<backend_pane>\d+) epoch=(?P<epoch>\d+) "
    r"wait=(?P<wait>true|false) "
)
TRANSACTION_TERMINAL = re.compile(
    r"^(?:synchronized layout ready|completed synchronized layout|degraded synchronized layout) "
    r"actor=session-switcher .* epoch=(?P<epoch>\d+) "
)
NATIVE_BOOTSTRAP_TERMINAL = re.compile(
    r"^(?P<kind>completed|degraded) synchronized layout actor=session-switcher "
    r"request=(?P<request>\d+) interaction=0 event_sequence=0 "
    r"native_resize_revision=(?P<native_resize_revision>\d+) "
    r"epoch=(?P<epoch>\d+) target_revision=(?P<target_revision>\d+) "
    r"region_final=(?P<region_ready>\d+)/(?P<region_total>\d+) "
    r"pane_ready=(?P<ready>\d+)/(?P<total>\d+) "
    r"recovered_regions=\[(?P<recovered_regions>[^]]*)\] recovered_count=(?P<recovered_count>\d+) "
    r"recovered_managed=(?P<recovered_managed>\{[^}]*\}) "
    r"recovered_managed_content=(?P<recovered_content>\{[^}]*\}) "
    r"missing=(?P<missing>\[[^]]*\]) duration_us=\d+$"
)
NATIVE_BOOTSTRAP_TERMINAL_PREFIX = re.compile(
    r"^(?:completed|degraded) synchronized layout "
)
NATIVE_PRESENT = re.compile(
    r"^window-resize-present native_resize_revision=(?P<native_resize_revision>\d+) "
    r"configure_sequence=\d+ width=(?P<width>\d+) height=(?P<height>\d+) "
    r"epoch=(?P<epoch>\d+) phase=(?P<phase>promoted|degraded) "
    r"wait_ready=(?P<ready>\d+)/(?P<total>\d+) staged_count=0 "
    r"managed_expectations=0 managed_staged_resources=0 degraded=(?P<degraded>true|false)$"
)
NATIVE_PRESENT_PREFIX = re.compile(r"^window-resize-present ")
REGION_RECOVERY = re.compile(
    r"^layout-region timeout-recovery epoch=(?P<epoch>\d+) recovered_leases=\[(?P<leases>[^]]*)\]$"
)
REGION_RECOVERY_PREFIX = re.compile(r"^layout-region timeout-recovery ")

Rect = tuple[int, int, int, int]


def _csv_integers(value: str, name: str) -> list[int]:
    if not value:
        return []
    try:
        return [int(item) for item in value.split(",")]
    except ValueError as error:
        raise ValueError(f"invalid {name}=[{value}]") from error


def _rects(value: str, name: str) -> tuple[list[int], dict[int, Rect]]:
    order: list[int] = []
    result: dict[int, Rect] = {}
    try:
        for item in value.split(","):
            pane, x, y, width, height = (int(part) for part in item.split(":"))
            if pane in result:
                raise ValueError(f"duplicate pane={pane}")
            order.append(pane)
            result[pane] = (x, y, width, height)
    except ValueError as error:
        raise ValueError(f"invalid {name}=[{value}]") from error
    return order, result


def _braced_integers(value: str, name: str) -> set[int]:
    if not (value.startswith("{") and value.endswith("}")):
        raise ValueError(f"invalid {name}={value}")
    body = value[1:-1]
    return set(_csv_integers(body.replace(" ", ""), name)) if body else set()


def _recovered_content_keys(value: str) -> set[int]:
    if not (value.startswith("{") and value.endswith("}")):
        raise ValueError(f"invalid recovered_managed_content={value}")
    body = value[1:-1]
    if not body:
        return set()
    try:
        return {int(item.split(":", 1)[0].strip()) for item in body.split(",")}
    except ValueError as error:
        raise ValueError(f"invalid recovered_managed_content={value}") from error


def _in_bounds(rect: Rect, width: int, height: int) -> bool:
    x, y, rect_width, rect_height = rect
    return (
        x >= 0
        and y >= 0
        and rect_width > 0
        and rect_height > 0
        and x + rect_width <= width
        and y + rect_height <= height
    )


def _overlap(left: Rect, right: Rect) -> bool:
    left_x, left_y, left_width, left_height = left
    right_x, right_y, right_width, right_height = right
    return (
        max(left_x, right_x) < min(left_x + left_width, right_x + right_width)
        and max(left_y, right_y) < min(left_y + left_height, right_y + right_height)
    )


def _require_geometry(rects: dict[int, Rect], width: int, height: int, name: str) -> None:
    if any(not _in_bounds(rect, width, height) for rect in rects.values()):
        raise ValueError(f"{name} contains a nonpositive, negative-origin, or out-of-bounds rectangle")
    values = list(rects.items())
    for index, (left_pane, left) in enumerate(values):
        for right_pane, right in values[index + 1 :]:
            if _overlap(left, right):
                raise ValueError(f"{name} overlaps panes {left_pane} and {right_pane}")


def _require_exact_tiling(rects: dict[int, Rect], region: Rect) -> None:
    region_x, region_y, region_width, region_height = region
    if not rects:
        raise ValueError("child target tile is empty")
    if any(
        x < region_x
        or y < region_y
        or x + width > region_x + region_width
        or y + height > region_y + region_height
        for x, y, width, height in rects.values()
    ):
        raise ValueError("child target rectangle escapes the issued region")
    area = sum(width * height for _x, _y, width, height in rects.values())
    bounds = (
        min(rect[0] for rect in rects.values()),
        min(rect[1] for rect in rects.values()),
        max(rect[0] + rect[2] for rect in rects.values()),
        max(rect[1] + rect[3] for rect in rects.values()),
    )
    if bounds != (region_x, region_y, region_x + region_width, region_y + region_height):
        raise ValueError(f"child target bounds do not equal issued region: {bounds} region={region}")
    if area != region_width * region_height:
        raise ValueError(f"child target area does not exactly cover issued region: {area}")


def _one(records: list[tuple[int, re.Match[str]]], name: str) -> tuple[int, re.Match[str]]:
    if len(records) != 1:
        raise ValueError(f"expected exactly one {name}, got {len(records)}")
    return records[0]


def _strict_prefixed_records(
    lines: list[str],
    *,
    start_line: int,
    end_line: int,
    prefix: re.Pattern[str],
    pattern: re.Pattern[str],
    name: str,
) -> list[tuple[int, re.Match[str]]]:
    records = []
    for number, line in enumerate(lines[start_line:end_line - 1], start_line + 1):
        if prefix.match(line) is None:
            continue
        match = pattern.match(line)
        if match is None:
            raise ValueError(f"malformed {name} at line={number}")
        records.append((number, match))
    return records


def _advance_completed_native_setup(
    lines: list[str], setup: dict[str, Any]
) -> dict[str, Any]:
    quiescent_line = setup["quiescent_line"]
    later_presentations = _strict_prefixed_records(
        lines,
        start_line=quiescent_line,
        end_line=len(lines) + 1,
        prefix=PRESENTED_LAYOUT_PREFIX,
        pattern=PRESENTED_LAYOUT,
        name="post-quiescence presented layout",
    )
    for number, line in enumerate(lines[quiescent_line:], quiescent_line + 1):
        if ACCEPTED_PREFIX.match(line) is None:
            continue
        accepted = ACCEPTED.match(line)
        if accepted is None:
            raise ValueError(f"malformed synchronized acceptance after setup at line={number}")
        interaction = int(accepted["interaction"])
        event_sequence = int(accepted["event_sequence"])
        native_revision = int(accepted["native_resize_revision"])
        epoch = int(accepted["epoch"])
        if interaction > 0 and native_revision == 0:
            return setup
        if interaction == 0 and event_sequence == 0 and native_revision > 0 and epoch > 0:
            base_revision = int(accepted["base_revision"])
            target_revision = int(accepted["target_revision"])
            if (
                accepted["actor"] != "session-switcher"
                or int(accepted["actor_generation"])
                != setup["actor_identities"]["session-switcher"]["generation"]
                or base_revision != setup["quiescent_presented_layout_revision"]
                or target_revision != base_revision + 1
            ):
                raise ValueError(f"later native acceptance has invalid identity/revisions line={number}")
            presentations_before_accept = [
                record for record in later_presentations if record[0] < number
            ]
            if presentations_before_accept and (
                int(presentations_before_accept[-1][1]["revision"]) != base_revision
                or any(
                    int(record[1]["revision"]) != base_revision
                    for record in presentations_before_accept
                )
            ):
                raise ValueError("later native base presentation is ambiguous or superseded")
            later_acceptances = [
                later
                for later, later_line in enumerate(lines[number:], number + 1)
                if ACCEPTED_PREFIX.match(later_line) is not None
            ]
            successor_boundary = later_acceptances[0] if later_acceptances else len(lines) + 1
            later_quiescences = _strict_prefixed_records(
                lines,
                start_line=number,
                end_line=successor_boundary,
                prefix=SETUP_QUIESCENT_PREFIX,
                pattern=SETUP_QUIESCENT,
                name="later native setup quiescence",
            )
            if not later_quiescences:
                raise ValueError(
                    f"selected setup was superseded by incomplete native acceptance line={number}"
                )
            next_quiescent_line, next_quiescent = _one(
                later_quiescences, "later native setup quiescence"
            )
            if (
                int(next_quiescent["presented"]) != target_revision
                or int(next_quiescent["inventory"]) != setup["leased_layout_revision"]
            ):
                raise ValueError("later native quiescence revisions are not exact")
            if any(number < record[0] < next_quiescent_line for record in later_presentations):
                raise ValueError("later native presentation anchor was superseded before quiescence")
            terminals = _strict_prefixed_records(
                lines,
                start_line=number,
                end_line=next_quiescent_line,
                prefix=NATIVE_BOOTSTRAP_TERMINAL_PREFIX,
                pattern=NATIVE_BOOTSTRAP_TERMINAL,
                name="later native terminal",
            )
            terminal_line, terminal = _one(terminals, "later native terminal")
            expected_count = len(setup["pane_rects"])
            degraded = terminal["kind"] == "degraded"
            recovered_regions = _csv_integers(
                terminal["recovered_regions"], "later recovered regions"
            )
            recovered_managed = _braced_integers(
                terminal["recovered_managed"], "later recovered managed"
            )
            recovered_content = _recovered_content_keys(terminal["recovered_content"])
            ordinary_panes = {route["pane"] for route in setup["ordinary_routes"]}
            recovered_region_set = set(recovered_regions)
            recovery_records = [
                match
                for _later, text in enumerate(lines[number:terminal_line - 1], number + 1)
                if (match := REGION_RECOVERY.match(text)) is not None
                and int(match["epoch"]) == epoch
            ]
            if (
                int(terminal["native_resize_revision"]) != native_revision
                or int(terminal["request"]) != int(accepted["request"])
                or int(terminal["epoch"]) != epoch
                or int(terminal["target_revision"]) != target_revision
                or int(terminal["region_ready"]) != 1
                or int(terminal["region_total"]) != 1
                or int(terminal["ready"]) != expected_count
                or int(terminal["total"]) != expected_count
                or terminal["missing"] != "[]"
                or int(terminal["recovered_count"]) != len(recovered_regions)
                or recovered_regions != sorted(recovered_region_set)
                or recovered_managed != recovered_content
                or not recovered_managed.issubset(ordinary_panes)
                or not recovered_region_set.issubset({setup["child_lease"]})
                or (
                    recovered_region_set
                    and (
                        len(recovery_records) != 1
                        or set(
                            _csv_integers(
                                recovery_records[0]["leases"], "later recovery leases"
                            )
                        )
                        != recovered_region_set
                    )
                )
                or (not degraded and (recovered_regions or recovered_managed))
                or (degraded and not (recovered_regions or recovered_managed))
            ):
                raise ValueError("later native terminal is not exact")
            native_presents = _strict_prefixed_records(
                lines,
                start_line=terminal_line,
                end_line=next_quiescent_line,
                prefix=NATIVE_PRESENT_PREFIX,
                pattern=NATIVE_PRESENT,
                name="later post-terminal native presentation",
            )
            native_present_line, native_present = _one(
                native_presents, "later post-terminal native presentation"
            )
            if (
                int(native_present["native_resize_revision"]) != native_revision
                or int(native_present["epoch"]) != epoch
                or int(native_present["ready"]) != expected_count
                or int(native_present["total"]) != expected_count
                or (native_present["phase"] == "degraded") != degraded
                or (native_present["degraded"] == "true") != degraded
            ):
                raise ValueError("later native presentation differs from terminal")
            wait_panes = _csv_integers(accepted["wait_panes"], "later native wait panes")
            target_order, target_rects = _rects(
                accepted["target_rects"], "later native target rects"
            )
            expected_inventory = {int(pane) for pane in setup["pane_rects"]}
            if (
                wait_panes != sorted(expected_inventory)
                or target_order != sorted(expected_inventory)
                or set(target_rects) != expected_inventory
                or int(accepted["wait"]) != expected_count
                or int(accepted["pane_total"]) != expected_count
            ):
                raise ValueError("later native wait/target inventory is not exact")
            width = int(native_present["width"])
            height = int(native_present["height"])
            _require_geometry(target_rects, width, height, "later native target rects")
            switcher_pane = setup["actor_identities"]["session-switcher"]["pane"]
            issued_records = [
                (later, match)
                for later, text in enumerate(lines[quiescent_line:number - 1], quiescent_line + 1)
                if (match := REGION_ISSUED.match(text)) is not None
                and int(match["epoch"]) == epoch
            ]
            issued_line, issued = _one(issued_records, "later native delegated region")
            if int(issued["lease"]) != setup["child_lease"] or issued["holder"] != "workspace":
                raise ValueError("later native region ownership is not exact")
            region = tuple(int(part) for part in issued["target"].split(":"))
            child_panes = set(setup["rest_grant_panes"])
            _require_exact_tiling({pane: target_rects[pane] for pane in child_panes}, region)
            if region[0] + region[2] != target_rects[switcher_pane][0]:
                raise ValueError("later native region does not end at switcher boundary")
            routes = [
                match
                for _later, text in enumerate(lines[number:terminal_line - 1], number + 1)
                if (match := MANAGED_EXPECTATION.match(text)) is not None
                and int(match["epoch"]) == epoch
                and match["wait"] == "true"
            ]
            actual_routes = {
                (
                    int(match["backend"]),
                    int(match["connection_generation"]),
                    int(match["pane"]),
                    int(match["backend_pane"]),
                )
                for match in routes
            }
            expected_routes = {
                (
                    route["backend"], route["connection_generation"],
                    route["pane"], route["backend_pane"],
                )
                for route in setup["ordinary_routes"]
            }
            if actual_routes != expected_routes or len(routes) != len(expected_routes):
                raise ValueError("later native managed routes are not exact")
            notifications: dict[int, list[int]] = {}
            for _later, text in enumerate(lines[issued_line:terminal_line - 1], issued_line + 1):
                match = PANE_NOTIFIED.match(text)
                if match is None or int(match["epoch"]) != epoch or match["wait"] != "true":
                    continue
                notifications.setdefault(int(match["pane"]), []).append(int(match["owner"]))
            expected_owners = {pane: setup["child_lease"] for pane in child_panes}
            expected_owners.update({
                setup["actor_identities"]["status-bar"]["pane"]: setup["parent_lease"],
                switcher_pane: setup["parent_lease"],
            })
            if set(notifications) != expected_inventory or any(
                notifications[pane] != [owner] for pane, owner in expected_owners.items()
            ):
                raise ValueError("later native pane notifications/owners are not exact")
            final_groups = [
                (later, match)
                for later, text in enumerate(
                    lines[native_present_line:next_quiescent_line - 1], native_present_line + 1
                )
                if (match := BACKEND_GROUP.match(text)) is not None
            ]
            backend = setup["ordinary_routes"][0]["backend"]
            for lease, actor in (
                (setup["parent_lease"], "session-switcher"),
                (setup["child_lease"], "workspace"),
            ):
                _line, group = _one(
                    [record for record in final_groups if int(record[1]["lease"]) == lease],
                    "later native final backend group",
                )
                if group["actor"] != actor or _csv_integers(group["backends"], "later backends") != [backend]:
                    raise ValueError("later native final backend group is not exact")
            updated = {
                **setup,
                "anchor_kind": "native-bootstrap",
                "anchor_start_line": quiescent_line,
                "anchor_accept_line": number,
                "settled_line": native_present_line,
                "quiescent_line": next_quiescent_line,
                "quiescent_presented_layout_revision": target_revision,
                "startup_acceptances": [{
                    "line": number,
                    "epoch": epoch,
                    "interaction": 0,
                    "native_resize_revision": native_revision,
                }],
                "divider_line": number,
                "ready_divider": target_rects[switcher_pane][0],
                "composition_width": width,
                "composition_height": height,
                "pane_rects": {str(pane): rect for pane, rect in target_rects.items()},
            }
            return _advance_completed_native_setup(lines, updated)
        raise ValueError(f"invalid non-interactive acceptance after setup at line={number}")
    if later_presentations:
        raise ValueError("selected setup was superseded by later presented layout")
    return setup


def _active_actors(lines: list[str]) -> dict[str, dict[str, int]]:
    actors = ("status-bar", "session-switcher", "resize-proof-slow")
    starts: dict[str, tuple[int, re.Match[str]]] = {}
    for number, line in enumerate(lines, 1):
        if match := ACTOR_START.match(line):
            starts[match["actor"]] = (number, match)
    if set(starts) != set(actors):
        raise ValueError(f"active actor starts incomplete: {sorted(starts)}")
    result = {}
    for actor in actors:
        start_line, start = starts[actor]
        generation = int(start["generation"])
        pane = int(start["pane"])
        promotions = []
        for number, line in enumerate(lines[start_line:], start_line + 1):
            match = ACTOR_PROMOTION.match(line)
            if match is None or match["actor"] != actor:
                continue
            panes = _csv_integers(match["panes"], f"{actor} promoted panes")
            if (
                int(match["generation"]) == generation
                and int(match["pane_count"]) == 1
                and panes == [pane]
            ):
                promotions.append(number)
            elif int(match["generation"]) == generation:
                raise ValueError(f"malformed promotion for active actor={actor}")
        if not promotions or generation == 0 or pane == 0:
            raise ValueError(f"active actor lacks exact promotion actor={actor}")
        result[actor] = {
            "start_line": start_line,
            "first_promotion_line": promotions[0],
            "promotion_line": promotions[-1],
            "generation": generation,
            "pane": pane,
        }
    if len({identity["pane"] for identity in result.values()}) != 3:
        raise ValueError("active actor panes are not distinct")
    return result


def _native_bootstrap_candidate(
    lines: list[str],
    *,
    grant_line: int,
    parent: int,
    child: int,
    expected_inventory: set[int],
    pane_ids: list[int],
    ordinary_routes: list[tuple[int, int, int, int]],
    actors: dict[str, dict[str, int]],
    required_promotions: int,
) -> dict[str, Any]:
    all_acceptance_lines = [
        (number, line)
        for number, line in enumerate(lines, 1)
        if number > required_promotions and ACCEPTED_PREFIX.match(line) is not None
    ]
    if not all_acceptance_lines:
        raise ValueError("native-bootstrap synchronized acceptance is missing")
    first_acceptance_line = all_acceptance_lines[0][0]
    first_cycle_boundary = (
        all_acceptance_lines[1][0] if len(all_acceptance_lines) > 1 else len(lines) + 1
    )
    quiescences = _strict_prefixed_records(
        lines,
        start_line=first_acceptance_line,
        end_line=first_cycle_boundary,
        prefix=SETUP_QUIESCENT_PREFIX,
        pattern=SETUP_QUIESCENT,
        name="native-bootstrap setup quiescence",
    )
    quiescent_line, quiescent = _one(quiescences, "native-bootstrap setup quiescence")
    acceptance_lines = [
        record for record in all_acceptance_lines if record[0] < quiescent_line
    ]
    if len(acceptance_lines) != 1:
        raise ValueError(
            f"expected exactly one native-bootstrap synchronized acceptance, got {len(acceptance_lines)}"
        )
    accepted_line, accepted_text = acceptance_lines[0]
    accepted = ACCEPTED.match(accepted_text)
    if accepted is None:
        raise ValueError("native-bootstrap synchronized acceptance is malformed")
    switcher = actors["session-switcher"]
    epoch = int(accepted["epoch"])
    native_revision = int(accepted["native_resize_revision"])
    base_revision = int(accepted["base_revision"])
    target_revision = int(accepted["target_revision"])
    if (
        accepted["actor"] != "session-switcher"
        or int(accepted["actor_generation"]) != switcher["generation"]
        or int(accepted["interaction"]) != 0
        or int(accepted["event_sequence"]) != 0
        or native_revision <= 0
        or epoch <= 0
        or base_revision <= 0
        or target_revision != base_revision + 1
    ):
        raise ValueError("native-bootstrap acceptance identity/revisions are not exact")

    baselines = _strict_prefixed_records(
        lines,
        start_line=grant_line,
        end_line=accepted_line,
        prefix=LEASED_LAYOUT_PREFIX,
        pattern=LEASED_LAYOUT,
        name="transitional leased baseline",
    )
    baseline_line, baseline = _one(baselines, "transitional leased baseline")
    if int(baseline["revision"]) != base_revision:
        raise ValueError("transitional baseline revision differs from native base revision")
    baseline_order, baseline_rects = _rects(baseline["pane_rects"], "baseline pane_rects")
    baseline_panes = set(baseline_rects)
    if (
        int(baseline["items"]) != len(baseline_panes)
        or int(baseline["panes"]) != len(baseline_panes)
        or baseline_order != sorted(baseline_panes)
        or not set(pane_ids).issubset(baseline_panes)
        or not baseline_panes.issubset(expected_inventory)
    ):
        raise ValueError("transitional baseline inventory is not an exact delegated subset")
    all_presentation_records = _strict_prefixed_records(
        lines,
        start_line=baseline_line,
        end_line=quiescent_line,
        prefix=PRESENTED_LAYOUT_PREFIX,
        pattern=PRESENTED_LAYOUT,
        name="pre-bootstrap presented layout",
    )
    presentation_records = [
        record for record in all_presentation_records if record[0] < accepted_line
    ]
    if any(record[0] > accepted_line for record in all_presentation_records):
        raise ValueError("native-bootstrap presentation anchor was superseded before quiescence")
    baseline_presented = [
        record
        for record in presentation_records
        if int(record[1]["revision"]) == int(baseline["pending"])
    ]
    baseline_presented_line, _ = _one(
        baseline_presented, "transitional baseline presented revision"
    )
    baseline_settled = [
        (number, match)
        for number, line in enumerate(
            lines[baseline_presented_line:accepted_line - 1], baseline_presented_line + 1
        )
        if (match := SETTLED_PRESENT.match(line)) is not None
    ]
    if not baseline_settled:
        raise ValueError("transitional baseline lacks a settled presentation")
    baseline_settled_line, _ = baseline_settled[0]
    base_presented = [
        record
        for record in presentation_records
        if record[0] > required_promotions and int(record[1]["revision"]) == base_revision
    ]
    base_presented_line, _ = _one(base_presented, "native base presented revision")
    if presentation_records[-1][0] != base_presented_line:
        raise ValueError("native base presentation was superseded before acceptance")

    terminal_lines = [
        (number, line)
        for number, line in enumerate(lines[accepted_line:quiescent_line - 1], accepted_line + 1)
        if NATIVE_BOOTSTRAP_TERMINAL_PREFIX.match(line) is not None
    ]
    if len(terminal_lines) != 1:
        raise ValueError(f"expected exactly one native-bootstrap terminal, got {len(terminal_lines)}")
    terminal_line, terminal_text = terminal_lines[0]
    terminal = NATIVE_BOOTSTRAP_TERMINAL.match(terminal_text)
    if terminal is None:
        raise ValueError("native-bootstrap terminal is malformed")
    expected_count = len(expected_inventory)
    terminal_degraded = terminal["kind"] == "degraded"
    recovered_regions = _csv_integers(
        terminal["recovered_regions"], "terminal recovered_regions"
    )
    recovered_managed = _braced_integers(
        terminal["recovered_managed"], "terminal recovered_managed"
    )
    recovered_content = _recovered_content_keys(terminal["recovered_content"])
    ordinary_panes = {route[2] for route in ordinary_routes}
    recovery_records = _strict_prefixed_records(
        lines,
        start_line=accepted_line,
        end_line=terminal_line,
        prefix=REGION_RECOVERY_PREFIX,
        pattern=REGION_RECOVERY,
        name="native region recovery",
    )
    recovered_region_set = set(recovered_regions)
    recovery_record_invalid = (
        len(recovery_records) > 1
        or (not recovery_records and bool(recovered_region_set))
        or (
            len(recovery_records) == 1
            and (
                int(recovery_records[0][1]["epoch"]) != epoch
                or set(
                    _csv_integers(
                        recovery_records[0][1]["leases"], "region recovery leases"
                    )
                )
                != recovered_region_set
            )
        )
    )
    if (
        int(terminal["region_ready"]) != 1
        or int(terminal["region_total"]) != 1
        or int(terminal["ready"]) != expected_count
        or int(terminal["total"]) != expected_count
        or int(terminal["request"]) != int(accepted["request"])
        or int(terminal["epoch"]) != epoch
        or int(terminal["target_revision"]) != target_revision
        or int(terminal["native_resize_revision"]) != native_revision
        or int(terminal["recovered_count"]) != len(recovered_regions)
        or recovered_regions != sorted(set(recovered_regions))
        or recovered_managed != recovered_content
        or not recovered_managed.issubset(ordinary_panes)
        or not recovered_region_set.issubset({child})
        or recovery_record_invalid
        or terminal["missing"] != "[]"
        or (
            not terminal_degraded
            and (
                recovered_regions
                or recovered_managed
                or recovered_content
            )
        )
        or (
            terminal_degraded
            and not (
                recovered_regions
                or recovered_managed
                or recovered_content
            )
        )
    ):
        raise ValueError("native-bootstrap terminal does not exactly complete its wait set")
    native_presents = _strict_prefixed_records(
        lines,
        start_line=terminal_line,
        end_line=quiescent_line,
        prefix=NATIVE_PRESENT_PREFIX,
        pattern=NATIVE_PRESENT,
        name="post-terminal native presentation",
    )
    native_present_line, native_present = _one(
        native_presents, "post-terminal native presentation"
    )
    if (
        int(native_present["native_resize_revision"]) != native_revision
        or int(native_present["epoch"]) != epoch
        or int(native_present["ready"]) != expected_count
        or int(native_present["total"]) != expected_count
        or (native_present["phase"] == "degraded") != terminal_degraded
        or (native_present["degraded"] == "true") != terminal_degraded
    ):
        raise ValueError("post-terminal native presentation differs from terminal result")
    composition_width = int(native_present["width"])
    composition_height = int(native_present["height"])
    if composition_width <= 0 or composition_height <= 0:
        raise ValueError("native-bootstrap composition is empty")

    wait_panes = _csv_integers(accepted["wait_panes"], "native wait_for_panes")
    target_order, target_rects = _rects(accepted["target_rects"], "native target_rects")
    if (
        int(accepted["wait"]) != expected_count
        or int(accepted["pane_total"]) != expected_count
        or wait_panes != sorted(expected_inventory)
        or target_order != sorted(expected_inventory)
        or set(target_rects) != expected_inventory
    ):
        raise ValueError("native-bootstrap wait/targets differ from exact expected inventory")
    _require_geometry(target_rects, composition_width, composition_height, "native target_rects")

    issued_records = [
        (number, match)
        for number, line in enumerate(lines[base_presented_line:accepted_line - 1], base_presented_line + 1)
        if (match := REGION_ISSUED.match(line)) is not None and int(match["epoch"]) == epoch
    ]
    issued_line, issued = _one(issued_records, "native-bootstrap delegated region")
    if int(issued["lease"]) != child or issued["holder"] != "workspace":
        raise ValueError("native-bootstrap region is not owned by workspace child")
    region = tuple(int(part) for part in issued["target"].split(":"))
    if not _in_bounds(region, composition_width, composition_height):
        raise ValueError("native-bootstrap issued region is outside composition")
    child_rects = {pane: target_rects[pane] for pane in pane_ids}
    _require_exact_tiling(child_rects, region)
    if region[0] + region[2] != target_rects[switcher["pane"]][0]:
        raise ValueError("native-bootstrap child region does not end at switcher boundary")

    routes = [
        (number, match)
        for number, line in enumerate(lines[accepted_line:terminal_line - 1], accepted_line + 1)
        if (match := MANAGED_EXPECTATION.match(line)) is not None
        and int(match["epoch"]) == epoch
        and match["wait"] == "true"
    ]
    actual_routes = {
        (
            int(match["backend"]),
            int(match["connection_generation"]),
            int(match["pane"]),
            int(match["backend_pane"]),
        )
        for _number, match in routes
    }
    if actual_routes != set(ordinary_routes) or len(routes) != len(ordinary_routes):
        raise ValueError("native-bootstrap managed routes differ from exact ordinary routes")

    notifications: dict[int, list[tuple[int, int]]] = {}
    for number, line in enumerate(lines[issued_line:terminal_line - 1], issued_line + 1):
        match = PANE_NOTIFIED.match(line)
        if match is None or int(match["epoch"]) != epoch or match["wait"] != "true":
            continue
        notifications.setdefault(int(match["pane"]), []).append((number, int(match["owner"])))
    if set(notifications) != expected_inventory:
        raise ValueError("native-bootstrap notifications differ from exact wait inventory")
    owners = {pane: child for pane in pane_ids}
    owners.update({actors["status-bar"]["pane"]: parent, switcher["pane"]: parent})
    for pane, owner in owners.items():
        if len(notifications[pane]) != 1 or notifications[pane][0][1] != owner:
            raise ValueError(f"wrong or duplicate native-bootstrap owner pane={pane}")

    initial_groups = [
        (number, match)
        for number, line in enumerate(lines[grant_line:baseline_line - 1], grant_line + 1)
        if (match := BACKEND_GROUP.match(line)) is not None
    ]
    final_groups = [
        (number, match)
        for number, line in enumerate(lines[terminal_line:quiescent_line - 1], terminal_line + 1)
        if (match := BACKEND_GROUP.match(line)) is not None
    ]
    group_lines: dict[str, int] = {}
    backend = ordinary_routes[0][0]
    for phase, records in (("initial", initial_groups), ("final", final_groups)):
        for name, lease, actor in (("parent", parent, "session-switcher"), ("child", child, "workspace")):
            matching = [record for record in records if int(record[1]["lease"]) == lease]
            line, match = _one(matching, f"{phase} {name} backend group")
            if match["actor"] != actor or _csv_integers(match["backends"], f"{phase} {name} backends") != [backend]:
                raise ValueError(f"{phase} {name} backend group identity is not exact")
            group_lines[f"{phase}_{name}"] = line
    if int(quiescent["presented"]) != target_revision or int(quiescent["inventory"]) != base_revision:
        raise ValueError("native-bootstrap quiescence revisions differ from accepted target/base")
    if not (
        grant_line < group_lines["initial_parent"] < baseline_line
        and grant_line < group_lines["initial_child"] < baseline_line
        and baseline_line < baseline_presented_line < baseline_settled_line
        and required_promotions < base_presented_line < issued_line < accepted_line
        and accepted_line < terminal_line < native_present_line
        and native_present_line < group_lines["final_parent"] < quiescent_line
        and native_present_line < group_lines["final_child"] < quiescent_line
    ):
        raise ValueError("native-bootstrap records are not strictly ordered")
    return {
        "anchor_kind": "native-bootstrap",
        "anchor_start_line": baseline_line,
        "anchor_accept_line": accepted_line,
        "leased_layout_line": baseline_line,
        "leased_layout_revision": base_revision,
        "parent_group_line": group_lines["final_parent"],
        "child_group_line": group_lines["final_child"],
        "presented_line": base_presented_line,
        "settled_line": native_present_line,
        "quiescent_line": quiescent_line,
        "quiescent_presented_layout_revision": target_revision,
        "startup_acceptances": [{
            "line": accepted_line,
            "epoch": epoch,
            "interaction": 0,
            "native_resize_revision": native_revision,
        }],
        "divider_line": accepted_line,
        "ready_divider": target_rects[switcher["pane"]][0],
        "composition_width": composition_width,
        "composition_height": composition_height,
        "pane_rects": target_rects,
    }


def validate_setup(lines: list[str]) -> dict[str, Any]:
    grants = [
        (number, match)
        for number, line in enumerate(lines, 1)
        if (match := REST_GRANT.match(line)) is not None
    ]
    grant_line, grant = _one(grants, "anchored rest-layout grant")
    parent = int(grant["parent"])
    child = int(grant["child"])
    pane_ids = _csv_integers(grant["pane_ids"], "delegated pane_ids")
    if grant["actor"] != "workspace" or parent == 0 or child == 0 or parent == child:
        raise ValueError("rest grant actor/lease identities are invalid")
    if (
        int(grant["panes"]) != len(pane_ids)
        or pane_ids != sorted(pane_ids)
        or len(set(pane_ids)) != len(pane_ids)
        or any(pane == 0 for pane in pane_ids)
    ):
        raise ValueError("rest grant count/list must be equal, sorted, unique, and nonzero")

    actors = _active_actors(lines)
    switcher = actors["session-switcher"]
    status = actors["status-bar"]
    slow = actors["resize-proof-slow"]
    slow_delegated = slow["start_line"] < grant_line and any(
        slow["start_line"] < number < grant_line
        and (match := ACTOR_PROMOTION.match(line)) is not None
        and match["actor"] == "resize-proof-slow"
        and int(match["generation"]) == slow["generation"]
        and int(match["pane_count"]) == 1
        and _csv_integers(match["panes"], "resize-proof-slow promoted panes") == [slow["pane"]]
        for number, line in enumerate(lines[:grant_line], 1)
    )

    ordinary_by_pane: dict[int, tuple[int, re.Match[str]]] = {}
    for number, line in enumerate(lines[:grant_line], 1):
        if match := ORDINARY_FRAME.match(line):
            ordinary_by_pane[int(match["pane"])] = (number, match)
    ordinary = sorted(ordinary_by_pane.values(), key=lambda record: int(record[1]["pane"]))
    if len(ordinary) != 2:
        raise ValueError(f"expected exactly two active ordinary panes, got {len(ordinary)}")
    ordinary_routes = [
        (
            int(match["backend"]),
            int(match["generation"]),
            int(match["pane"]),
            int(match["backend_pane"]),
        )
        for _line, match in ordinary
    ]
    if (
        len({route[2] for route in ordinary_routes}) != 2
        or len({route[3] for route in ordinary_routes}) != 2
        or len({route[0] for route in ordinary_routes}) != 1
        or len({route[1] for route in ordinary_routes}) != 1
        or any(value == 0 for route in ordinary_routes for value in route)
    ):
        raise ValueError("ordinary routes are not two distinct nonzero pairs on one backend/generation")
    expected_grant = {route[2] for route in ordinary_routes}
    if slow_delegated:
        expected_grant.add(slow["pane"])
    if set(pane_ids) != expected_grant or len(pane_ids) != (3 if slow_delegated else 2):
        raise ValueError("grant IDs differ from ordinary panes plus the pre-grant promoted slow pane")

    claims = [
        (number, match)
        for number, line in enumerate(lines[:grant_line], 1)
        if (match := ROOT_CLAIM.match(line)) is not None
    ]
    if not claims:
        raise ValueError("missing root claim before grant")
    claim_line, claim = claims[-1]
    if claim["actor"] != "session-switcher" or int(claim["generation"]) != switcher["generation"]:
        raise ValueError("root claim does not bind active session-switcher")

    expected_inventory = set(pane_ids) | {status["pane"], switcher["pane"]}
    if len(expected_inventory) != len(pane_ids) + 2:
        raise ValueError("root shell panes collide with delegated inventory")
    required_promotions = max(identity["first_promotion_line"] for identity in actors.values())
    candidate_errors = []
    selected = None
    latest_leased_line = None
    post_promotion_leased = _strict_prefixed_records(
        lines,
        start_line=max(grant_line, required_promotions),
        end_line=len(lines) + 1,
        prefix=LEASED_LAYOUT_PREFIX,
        pattern=LEASED_LAYOUT,
        name="post-promotion leased layout",
    )
    for leased_line, leased in post_promotion_leased:
        latest_leased_line = leased_line
        try:
            expected_count = len(expected_inventory)
            if int(leased["items"]) != expected_count or int(leased["panes"]) != expected_count:
                raise ValueError("leased layout items/panes are not the exact expected inventory count")
            pane_order, pane_rects = _rects(leased["pane_rects"], "leased pane_rects")
            if pane_order != sorted(expected_inventory) or set(pane_rects) != expected_inventory:
                raise ValueError("leased pane_rect keys are not the sorted exact expected inventory")
            later_leased = next(
                (
                    number
                    for number, line in enumerate(lines[leased_line:], leased_line + 1)
                    if LEASED_LAYOUT_PREFIX.match(line) is not None
                ),
                len(lines) + 1,
            )
            presentation_records = _strict_prefixed_records(
                lines,
                start_line=leased_line,
                end_line=later_leased,
                prefix=PRESENTED_LAYOUT_PREFIX,
                pattern=PRESENTED_LAYOUT,
                name="leased candidate presented layout",
            )
            presented = [
                record
                for record in presentation_records
                if int(record[1]["revision"]) == int(leased["pending"])
            ]
            presented_line, _presented = _one(
                presented, "exact presented revision for leased layout"
            )
            settled = [
                (number, match)
                for number, line in enumerate(lines[presented_line:], presented_line + 1)
                if (match := SETTLED_PRESENT.match(line)) is not None
            ]
            if not settled:
                raise ValueError("presented delegated layout has no later stable settled observation")
            settled_line, settled_match = settled[0]
            if settled_line >= later_leased:
                raise ValueError("presented layout was superseded before its stable settled observation")
            composition_width = int(settled_match["width"])
            composition_height = int(settled_match["height"])
            if composition_width == 0 or composition_height == 0:
                raise ValueError("settled composition is empty")
            _require_geometry(pane_rects, composition_width, composition_height, "leased pane_rects")
            quiescent = [
                (number, match)
                for number, line in enumerate(
                    lines[settled_line : later_leased - 1], settled_line + 1
                )
                if (match := SETUP_QUIESCENT.match(line)) is not None
                and int(match["presented"]) == int(leased["pending"])
            ]
            quiescent_line, quiescent_match = _one(
                quiescent, "exact setup quiescence for presented layout"
            )
            if any(
                number > presented_line and number < quiescent_line
                for number, _match in presentation_records
            ):
                raise ValueError("leased presentation was superseded before quiescence")
            if int(quiescent_match["inventory"]) != int(leased["revision"]):
                raise ValueError("setup quiescence inventory revision differs from leased layout")
            groups = [
                (number, match)
                for number, line in enumerate(lines[grant_line:leased_line - 1], grant_line + 1)
                if (match := BACKEND_GROUP.match(line)) is not None
            ]
            parent_groups = [record for record in groups if int(record[1]["lease"]) == parent]
            child_groups = [record for record in groups if int(record[1]["lease"]) == child]
            if not parent_groups or parent_groups[-1][1]["actor"] != "session-switcher":
                raise ValueError("latest pre-layout parent group is not session-switcher")
            if not child_groups or child_groups[-1][1]["actor"] != "workspace":
                raise ValueError("latest pre-layout child group is not workspace")
            backend = ordinary_routes[0][0]
            for name, record in (("parent", parent_groups[-1]), ("child", child_groups[-1])):
                if _csv_integers(record[1]["backends"], f"{name} backends") != [backend]:
                    raise ValueError(f"{name} backend group is not exact singleton backend")
            if not (
                grant_line
                < parent_groups[-1][0]
                < leased_line
                < presented_line
                < settled_line
                < quiescent_line
                and grant_line < child_groups[-1][0] < leased_line
            ):
                raise ValueError("setup records are not strictly ordered")
            selected = (
                leased_line,
                leased,
                pane_rects,
                parent_groups[-1][0],
                child_groups[-1][0],
                presented_line,
                settled_line,
                quiescent_line,
                composition_width,
                composition_height,
            )
        except ValueError as error:
            candidate_errors.append(f"line={leased_line}: {error}")
    if selected is None:
        if latest_leased_line is not None:
            raise ValueError(f"no stable exact delegated layout candidate: {candidate_errors}")
        native = _native_bootstrap_candidate(
            lines,
            grant_line=grant_line,
            parent=parent,
            child=child,
            expected_inventory=expected_inventory,
            pane_ids=pane_ids,
            ordinary_routes=ordinary_routes,
            actors=actors,
            required_promotions=required_promotions,
        )
        result = {
            "verified": True,
            "phase": "setup",
            "rest_grant_line": grant_line,
            "rest_grant_panes": pane_ids,
            "parent_lease": parent,
            "child_lease": child,
            "root_claim_line": claim_line,
            "slow_delegated": slow_delegated,
            "actor_identities": actors,
            "ordinary_routes": [
                {
                    "line": line,
                    "backend": route[0],
                    "connection_generation": route[1],
                    "pane": route[2],
                    "backend_pane": route[3],
                }
                for (line, _match), route in zip(ordinary, ordinary_routes, strict=True)
            ],
            **native,
            "pane_rects": {str(pane): rect for pane, rect in native["pane_rects"].items()},
        }
        return _advance_completed_native_setup(lines, result)
    if selected[0] != latest_leased_line:
        raise ValueError(
            "latest delegated layout lacks exact inventory/presented/settled/quiescent completion: "
            f"line={latest_leased_line} errors={candidate_errors}"
        )
    (
        leased_line,
        leased,
        pane_rects,
        parent_group_line,
        child_group_line,
        presented_line,
        settled_line,
        quiescent_line,
        composition_width,
        composition_height,
    ) = selected
    ready_divider = pane_rects[switcher["pane"]][0]
    if ready_divider <= 0:
        raise ValueError("session-switcher pane does not expose a positive root boundary")
    startup_acceptances = [
        {
            "line": number,
            "epoch": int(match["epoch"]),
            "interaction": int(match["interaction"]),
            "native_resize_revision": int(match["native_resize_revision"]),
        }
        for number, line in enumerate(
            lines[leased_line:quiescent_line - 1], leased_line + 1
        )
        if (match := ACCEPTED.match(line)) is not None
    ]
    if len(startup_acceptances) > 1:
        raise ValueError(
            f"selected setup interval has multiple synchronized acceptances: {startup_acceptances}"
        )
    if startup_acceptances:
        startup = startup_acceptances[0]
        startup_match = ACCEPTED.match(lines[startup["line"] - 1])
        if startup_match is None:
            raise ValueError("setup startup acceptance record disappeared")
        if (
            startup_match["actor"] != "session-switcher"
            or int(startup_match["actor_generation"]) != switcher["generation"]
            or startup["interaction"] != 0
            or int(startup_match["event_sequence"]) != 0
            or startup["native_resize_revision"] <= 0
            or startup["epoch"] <= 0
        ):
            raise ValueError(
                "setup acceptance is not exact current session-switcher native startup semantics"
            )
    result = {
        "verified": True,
        "phase": "setup",
        "rest_grant_line": grant_line,
        "rest_grant_panes": pane_ids,
        "parent_lease": parent,
        "child_lease": child,
        "root_claim_line": claim_line,
        "anchor_kind": "leased",
        "anchor_start_line": leased_line,
        "anchor_accept_line": startup_acceptances[0]["line"] if startup_acceptances else None,
        "parent_group_line": parent_group_line,
        "child_group_line": child_group_line,
        "leased_layout_line": leased_line,
        "leased_layout_revision": int(leased["revision"]),
        "presented_line": presented_line,
        "settled_line": settled_line,
        "quiescent_line": quiescent_line,
        "quiescent_presented_layout_revision": int(leased["pending"]),
        "startup_acceptances": startup_acceptances,
        "divider_line": leased_line,
        "ready_divider": ready_divider,
        "composition_width": composition_width,
        "composition_height": composition_height,
        "slow_delegated": slow_delegated,
        "actor_identities": actors,
        "pane_rects": {str(pane): rect for pane, rect in pane_rects.items()},
        "ordinary_routes": [
            {
                "line": line,
                "backend": route[0],
                "connection_generation": route[1],
                "pane": route[2],
                "backend_pane": route[3],
            }
            for (line, _match), route in zip(ordinary, ordinary_routes, strict=True)
        ],
    }
    return _advance_completed_native_setup(lines, result)


def validate(
    lines: list[str], *, after_line: int, accepted_line: int, expected_epoch: int
) -> dict[str, Any]:
    setup = validate_setup(lines)
    if after_line < setup["quiescent_line"] or accepted_line != after_line + 1:
        raise ValueError("exact acceptance anchor must immediately follow after-line and stable setup")
    if accepted_line > len(lines):
        raise ValueError("exact accepted-line is beyond the log")
    accepted = ACCEPTED.match(lines[accepted_line - 1])
    if accepted is None:
        raise ValueError("exact accepted-line is not an anchored synchronized acceptance")
    switcher = setup["actor_identities"]["session-switcher"]
    epoch = int(accepted["epoch"])
    if (
        accepted["actor"] != "session-switcher"
        or int(accepted["actor_generation"]) != switcher["generation"]
        or epoch != expected_epoch
        or int(accepted["interaction"]) <= 0
        or int(accepted["native_resize_revision"]) != 0
    ):
        raise ValueError("exact acceptance is not the requested interactive non-native transaction")

    terminal_line = len(lines) + 1
    for number, line in enumerate(lines[accepted_line:], accepted_line + 1):
        terminal = TRANSACTION_TERMINAL.match(line)
        if terminal is not None and int(terminal["epoch"]) == epoch:
            terminal_line = number
            break
    issued = [
        (number, match)
        for number, line in enumerate(lines[setup["settled_line"] : terminal_line - 1], setup["settled_line"] + 1)
        if (match := REGION_ISSUED.match(line)) is not None and int(match["epoch"]) == epoch
    ]
    issued_line, issued_match = _one(issued, "exact delegated region issuance")
    if int(issued_match["lease"]) != setup["child_lease"] or issued_match["holder"] != "workspace":
        raise ValueError("issued region is not owned by exact workspace child")
    region = tuple(int(part) for part in issued_match["target"].split(":"))
    composition_width = setup["composition_width"]
    composition_height = setup["composition_height"]
    if not _in_bounds(region, composition_width, composition_height):
        raise ValueError("issued child region is nonpositive or outside composition")

    routes = [
        (number, match)
        for number, line in enumerate(lines[accepted_line:terminal_line - 1], accepted_line + 1)
        if (match := MANAGED_EXPECTATION.match(line)) is not None
        and int(match["epoch"]) == epoch
        and match["wait"] == "true"
    ]
    if len(routes) != 2:
        raise ValueError(f"expected two exact managed routes, got {len(routes)}")
    actual_routes = {
        (
            int(match["backend"]),
            int(match["connection_generation"]),
            int(match["pane"]),
            int(match["backend_pane"]),
        )
        for _line, match in routes
    }
    expected_routes = {
        (
            route["backend"],
            route["connection_generation"],
            route["pane"],
            route["backend_pane"],
        )
        for route in setup["ordinary_routes"]
    }
    if actual_routes != expected_routes or len(actual_routes) != 2:
        raise ValueError("synchronized routes differ from exact setup routes")

    managed_panes = sorted(route[2] for route in actual_routes)
    child_wait = set(managed_panes)
    if setup["slow_delegated"]:
        child_wait.add(setup["actor_identities"]["resize-proof-slow"]["pane"])
    root_wait = {
        setup["actor_identities"]["status-bar"]["pane"],
        setup["actor_identities"]["session-switcher"]["pane"],
    }
    expected_wait = child_wait | root_wait
    wait_panes = _csv_integers(accepted["wait_panes"], "wait_for_panes")
    target_order, target_rects = _rects(accepted["target_rects"], "target_rects")
    if (
        int(accepted["wait"]) != len(expected_wait)
        or int(accepted["pane_total"]) != len(expected_wait)
        or wait_panes != sorted(expected_wait)
        or target_order != sorted(expected_wait)
        or set(target_rects) != expected_wait
    ):
        raise ValueError("wait list/targets are not the sorted exact child-plus-root union")
    _require_geometry(target_rects, composition_width, composition_height, "target_rects")
    child_rects = {pane: target_rects[pane] for pane in child_wait}
    _require_exact_tiling(child_rects, region)
    session_target_x = target_rects[switcher["pane"]][0]
    if region[0] + region[2] != session_target_x:
        raise ValueError("issued child region does not end at session-switcher boundary")

    notifications: dict[int, list[tuple[int, int]]] = {}
    for number, line in enumerate(lines[setup["settled_line"] : terminal_line - 1], setup["settled_line"] + 1):
        match = PANE_NOTIFIED.match(line)
        if match is None or int(match["epoch"]) != epoch or match["wait"] != "true":
            continue
        notifications.setdefault(int(match["pane"]), []).append((number, int(match["owner"])))
    if set(notifications) != expected_wait:
        raise ValueError("wait notifications differ from exact wait set")
    owners = {pane: setup["child_lease"] for pane in child_wait}
    owners.update({pane: setup["parent_lease"] for pane in root_wait})
    for pane, owner in owners.items():
        if notifications[pane] != [(notifications[pane][0][0], owner)] or len(notifications[pane]) != 1:
            raise ValueError(f"wrong or duplicate synchronized owner pane={pane}")
    if not (
        setup["leased_layout_line"] < setup["presented_line"] < setup["settled_line"]
        and setup["settled_line"] < setup["quiescent_line"] < issued_line < accepted_line
        and all(line > accepted_line for line, _match in routes)
    ):
        raise ValueError("transaction records are not strictly ordered")

    return {
        **setup,
        "phase": "transaction",
        "after_line": after_line,
        "accepted_line": accepted_line,
        "epoch": epoch,
        "managed_panes": managed_panes,
        "managed_routes": [
            {
                "line": line,
                "backend": int(match["backend"]),
                "connection_generation": int(match["connection_generation"]),
                "pane": int(match["pane"]),
                "backend_pane": int(match["backend_pane"]),
            }
            for line, match in routes
        ],
        "status_start_line": setup["actor_identities"]["status-bar"]["start_line"],
        "session_switcher_start_line": switcher["start_line"],
        "session_switcher_generation": switcher["generation"],
        "status_pane": setup["actor_identities"]["status-bar"]["pane"],
        "session_switcher_pane": switcher["pane"],
        "wait_panes": wait_panes,
        "child_wait_panes": sorted(child_wait),
        "root_wait_panes": sorted(root_wait),
        "route_backend": next(iter(actual_routes))[0],
        "session_target_x": session_target_x,
    }


def self_test() -> None:
    fixture_root = pathlib.Path(__file__).with_name("fixtures") / "managed-resize-topology"
    positives = ["positive-grant2.txt", "positive-grant3.txt", "positive-native-bootstrap.txt"]
    fixtures = {name: (fixture_root / name).read_text(encoding="utf-8") for name in positives}
    for name, text in fixtures.items():
        lines = text.splitlines()
        setup = validate_setup(lines)
        expected_divider = 960 if name == "positive-native-bootstrap.txt" else 80
        if setup["ready_divider"] != expected_divider:
            raise AssertionError("setup divider must come from session-switcher rect.x")
        if name == "positive-native-bootstrap.txt":
            if (
                setup["anchor_kind"] != "native-bootstrap"
                or setup["anchor_accept_line"] != setup["divider_line"]
                or len(setup["startup_acceptances"]) != 1
            ):
                raise AssertionError("native bootstrap did not expose exact setup anchors")
            continue
        accepted_line = next(
            number
            for number, line in enumerate(lines, 1)
            if (match := ACCEPTED.match(line)) is not None and int(match["interaction"]) > 0
        )
        epoch = int(ACCEPTED.match(lines[accepted_line - 1])["epoch"])
        validate(lines, after_line=accepted_line - 1, accepted_line=accepted_line, expected_epoch=epoch)
    native_positive = fixtures["positive-native-bootstrap.txt"]
    completed_native = native_positive.replace(
        "degraded synchronized layout", "completed synchronized layout", 1
    ).replace(
        'recovered_managed={1, 2} recovered_managed_content={1: "old-projected", 2: "old-projected"}',
        "recovered_managed={} recovered_managed_content={}",
        1,
    ).replace(
        "phase=degraded wait_ready=4/4 staged_count=0 managed_expectations=0 managed_staged_resources=0 degraded=true",
        "phase=promoted wait_ready=4/4 staged_count=0 managed_expectations=0 managed_staged_resources=0 degraded=false",
        1,
    )
    if validate_setup(completed_native.splitlines())["anchor_kind"] != "native-bootstrap":
        raise AssertionError("completed native bootstrap did not select native anchor")
    native_mutations = {
        "wrong-base": ("base_revision=2 target_revision=3", "base_revision=1 target_revision=3"),
        "wrong-target": ("base_revision=2 target_revision=3", "base_revision=2 target_revision=4"),
        "wrong-region-owner": ("epoch=1 lease=2 lease_generation=1 holder=workspace", "epoch=1 lease=1 lease_generation=1 holder=workspace"),
        "wrong-root-owner": ("pane=18446744073709551615 owner_lease=1", "pane=18446744073709551615 owner_lease=2"),
        "wrong-route-generation": ("connection_generation=1 pane=2 backend_pane=2 epoch=1", "connection_generation=2 pane=2 backend_pane=2 epoch=1"),
        "wrong-wait-inventory": ("wait_for_panes=[1,2,18446744073709551611,18446744073709551615]", "wait_for_panes=[1,2,18446744073709551615]"),
        "wrong-target-pane": ("18446744073709551615:960:0:320:686", "99:960:0:320:686"),
        "terminal-target-mismatch": ("epoch=1 target_revision=3 region_final=1/1", "epoch=1 target_revision=4 region_final=1/1"),
        "terminal-region-incomplete": ("region_final=1/1 pane_ready=4/4 recovered_regions", "region_final=0/1 pane_ready=4/4 recovered_regions"),
        "terminal-region-total": ("region_final=1/1 pane_ready=4/4 recovered_regions", "region_final=1/2 pane_ready=4/4 recovered_regions"),
        "terminal-missing-pane": ("missing=[] duration_us=250875", "missing=[2] duration_us=250875"),
        "degraded-without-recovery": ('recovered_managed={1, 2} recovered_managed_content={1: "old-projected", 2: "old-projected"}', "recovered_managed={} recovered_managed_content={}"),
        "recovered-managed-outside-routes": ('recovered_managed={1, 2} recovered_managed_content={1: "old-projected", 2: "old-projected"}', 'recovered_managed={1, 99} recovered_managed_content={1: "old-projected", 99: "old-projected"}'),
        "recovered-content-key-mismatch": ('recovered_managed_content={1: "old-projected", 2: "old-projected"}', 'recovered_managed_content={1: "old-projected"}'),
        "recovered-region-without-evidence": ("recovered_regions=[] recovered_count=0", "recovered_regions=[2] recovered_count=1"),
        "missing-native-present": ("window-resize-present native_resize_revision=1 configure_sequence=1 width=1280 height=720 epoch=1 phase=degraded wait_ready=4/4 staged_count=0 managed_expectations=0 managed_staged_resources=0 degraded=true\n", ""),
        "missing-final-child-group": ("layout-state backend groups lease=2 actor=workspace backends=[1]\nsetup layout quiescent", "setup layout quiescent"),
        "wrong-quiescent-target": ("quiescent presented_layout_revision=3 inventory_revision=2", "quiescent presented_layout_revision=4 inventory_revision=2"),
        "wrong-quiescent-base": ("presented_layout_revision=3 inventory_revision=2 native_resize_pending", "presented_layout_revision=3 inventory_revision=3 native_resize_pending"),
    }
    for label, (old, new) in native_mutations.items():
        mutated = native_positive.replace(old, new, 1)
        if mutated == native_positive:
            raise AssertionError(f"native self-test mutation did not apply: {label}")
        try:
            validate_setup(mutated.splitlines())
        except ValueError:
            continue
        raise AssertionError(f"invalid native bootstrap passed: {label}")
    native_acceptance = next(line for line in native_positive.splitlines() if ACCEPTED.match(line))
    duplicate_native = native_positive.replace(native_acceptance, f"{native_acceptance}\n{native_acceptance}", 1)
    switcher_promotion = next(
        line
        for line in native_positive.splitlines()
        if line.startswith("render scheduling actor-frame promoted actor=session-switcher ")
    )
    acceptance_before_promotion = native_positive.replace(
        f"{switcher_promotion}\n", "", 1
    ).replace(native_acceptance, f"{native_acceptance}\n{switcher_promotion}", 1)
    later_native_leased = native_positive.replace(
        "setup layout quiescent",
        "accepted leased actor layout items=4 panes=4 revision=2 pending_presented_layout_revision=4 pane_rects=[1:0:0:480:686,2:480:0:480:686,18446744073709551611:0:686:1280:34,18446744073709551615:960:0:320:686]\nsetup layout quiescent",
        1,
    )
    malformed_baseline = native_positive.replace(
        "accepted leased actor layout items=3 panes=3 revision=2",
        "accepted leased actor layout items=oops panes=3 revision=2",
        1,
    )
    malformed_presented = native_positive.replace(
        "presented committed layout revision=2",
        "presented committed layout revision=oops",
        1,
    )
    malformed_post_accept_presented = native_positive.replace(
        "degraded synchronized layout",
        "presented committed layout revision=oops\ndegraded synchronized layout",
        1,
    )
    malformed_native_present = native_positive.replace(
        "window-resize-present native_resize_revision=1",
        "window-resize-present native_resize_revision=oops",
        1,
    )
    native_present_line = next(
        line for line in native_positive.splitlines() if NATIVE_PRESENT.match(line)
    )
    competing_native_present = native_positive.replace(
        native_present_line, f"{native_present_line}\n{native_present_line}", 1
    )
    superseded_base_presented = native_positive.replace(
        "layout-region issued epoch=1",
        "presented committed layout revision=99\nlayout-region issued epoch=1",
        1,
    )
    later_native_acceptance = native_positive.replace(
        "setup layout quiescent",
        f"setup layout quiescent",
        1,
    ) + "\n" + native_acceptance.replace("request=1", "request=2", 1).replace(
        "native_resize_revision=1 epoch=1 base_revision=2 target_revision=3",
        "native_resize_revision=2 epoch=2 base_revision=3 target_revision=4",
        1,
    )
    later_presented_only = native_positive + "\npresented committed layout revision=4"
    for label, mutated in (
        ("duplicate-acceptance", duplicate_native),
        ("acceptance-before-final-promotion", acceptance_before_promotion),
        ("later-leased", later_native_leased),
        ("malformed-baseline", malformed_baseline),
        ("malformed-presented", malformed_presented),
        ("malformed-post-accept-presented", malformed_post_accept_presented),
        ("malformed-native-present", malformed_native_present),
        ("competing-native-present", competing_native_present),
        ("superseded-base-presented", superseded_base_presented),
        ("later-native-acceptance", later_native_acceptance),
        ("later-presented-only", later_presented_only),
    ):
        try:
            validate_setup(mutated.splitlines())
        except ValueError:
            continue
        raise AssertionError(f"ambiguous native bootstrap passed: {label}")
    completed_successor = "\n".join(
        (
            "layout-region issued epoch=2 lease=2 lease_generation=1 holder=workspace holder_generation=2 source=0:0:960:686 target=0:0:900:686 accepted_us=1",
            "layout-region pane-notified epoch=2 pane=18446744073709551611 owner_lease=1 wait=true accepted_us=2",
            "layout-region pane-notified epoch=2 pane=18446744073709551615 owner_lease=1 wait=true accepted_us=2",
            "accepted synchronized layout actor=session-switcher request=2 interaction=0 event_sequence=0 native_resize_revision=2 epoch=2 base_revision=3 target_revision=4 region_final=0/1 pane_ready=0/4 wait=4 affected=4 mode=ScaleThenAtomic actor_generation=2 focus=2 recovered_regions=[] wait_for_panes=[1,2,18446744073709551611,18446744073709551615] target_rects=[1:0:0:450:686,2:450:0:450:686,18446744073709551611:0:686:1280:34,18446744073709551615:900:0:380:686]",
            "admitted managed resize expectation backend=1 connection_generation=1 pane=1 backend_pane=1 epoch=2 wait=true logical_size=300x457 pixel_size=450x686 scale_120=180",
            "admitted managed resize expectation backend=1 connection_generation=1 pane=2 backend_pane=2 epoch=2 wait=true logical_size=300x457 pixel_size=450x686 scale_120=180",
            "layout-region pane-notified epoch=2 pane=1 owner_lease=2 wait=true accepted_us=3",
            "layout-region pane-notified epoch=2 pane=2 owner_lease=2 wait=true accepted_us=3",
            'degraded synchronized layout actor=session-switcher request=2 interaction=0 event_sequence=0 native_resize_revision=2 epoch=2 target_revision=4 region_final=1/1 pane_ready=4/4 recovered_regions=[] recovered_count=0 recovered_managed={1, 2} recovered_managed_content={1: "old-projected", 2: "old-projected"} missing=[] duration_us=9',
            "window-resize-present native_resize_revision=2 configure_sequence=2 width=1280 height=720 epoch=2 phase=degraded wait_ready=4/4 staged_count=0 managed_expectations=0 managed_staged_resources=0 degraded=true",
            "layout-state backend groups lease=1 actor=session-switcher backends=[1]",
            "layout-state backend groups lease=2 actor=workspace backends=[1]",
            "setup layout quiescent presented_layout_revision=4 inventory_revision=2 native_resize_pending=false window_resize_pending=false synchronized_layout_active=false staged_layouts=0 pending_presented_layout_revision=none layout_state_dirty=false pending_terminal_notifications=0 deferred_actor_host_messages=0 runtime_outbound_host_messages=0 runtime_pending_actor_inputs=0 runtime_observer_output_backlog=false layout_publications_current=true",
        )
    )
    advanced = validate_setup(f"{native_positive}\n{completed_successor}".splitlines())
    if advanced["quiescent_presented_layout_revision"] != 4 or advanced["ready_divider"] != 900:
        raise AssertionError("fully completed later native cycle did not advance setup selection")
    successor_terminal = next(
        line
        for line in completed_successor.splitlines()
        if NATIVE_BOOTSTRAP_TERMINAL.match(line)
    )
    successor_quiescent = next(
        line for line in completed_successor.splitlines() if SETUP_QUIESCENT.match(line)
    )
    successor_mutations = {
        "malformed-terminal": completed_successor.replace(
            "degraded synchronized layout actor=session-switcher",
            "degraded synchronized layout actor=wrong",
            1,
        ),
        "competing-terminal": completed_successor.replace(
            successor_terminal,
            f"{successor_terminal}\n{successor_terminal.replace('request=2', 'request=99', 1)}",
            1,
        ),
        "malformed-quiescence": completed_successor.replace(
            "setup layout quiescent presented_layout_revision=4 inventory_revision=2",
            "setup layout quiescent presented_layout_revision=4 inventory_revision=oops",
            1,
        ),
        "competing-quiescence": completed_successor.replace(
            successor_quiescent,
            f"{successor_quiescent}\n{successor_quiescent.replace('presented_layout_revision=4', 'presented_layout_revision=99', 1)}",
            1,
        ),
    }
    for label, successor in successor_mutations.items():
        try:
            validate_setup(f"{native_positive}\n{successor}".splitlines())
        except ValueError:
            continue
        raise AssertionError(f"invalid recursive native successor passed: {label}")
    third_successor = completed_successor.replace(
        "base_revision=3 target_revision=4", "base_revision=4 target_revision=5", 1
    ).replace("target_revision=4", "target_revision=5").replace(
        "presented_layout_revision=4", "presented_layout_revision=5", 1
    ).replace("request=2", "request=3").replace(
        "native_resize_revision=2", "native_resize_revision=3"
    ).replace("epoch=2", "epoch=3")
    third_advanced = validate_setup(
        f"{native_positive}\n{completed_successor}\n{third_successor}".splitlines()
    )
    if third_advanced["quiescent_presented_layout_revision"] != 5:
        raise AssertionError("three-cycle native bootstrap did not select latest quiescence")
    later_candidate = "\n".join(
        (
            "layout-state backend groups lease=1 actor=session-switcher backends=[1]",
            "layout-state backend groups lease=2 actor=workspace backends=[1]",
            "accepted leased actor layout items=5 panes=5 revision=2 pending_presented_layout_revision=4 pane_rects=[1:0:0:35:30,2:35:0:35:30,9:0:30:70:30,11:0:60:100:4,15:70:0:30:60]",
            "presented committed layout revision=4",
            "resize-present interaction=0 event_sequence=0 epoch=0 divider_x=27 composition_size=100x64 phase=settled wait_ready=0/0 staged_count=0 managed_expectations=0 managed_staged_resources=0 degraded=false superseded_epochs=0",
            "setup layout quiescent presented_layout_revision=4 inventory_revision=2 native_resize_pending=false window_resize_pending=false synchronized_layout_active=false staged_layouts=0 pending_presented_layout_revision=none layout_state_dirty=false pending_terminal_notifications=0 deferred_actor_host_messages=0 runtime_outbound_host_messages=0 runtime_pending_actor_inputs=0 runtime_observer_output_backlog=false layout_publications_current=true",
        )
    )
    appended = fixtures["positive-grant3.txt"].replace(
        "layout-region issued epoch=3", f"{later_candidate}\nlayout-region issued epoch=3", 1
    )
    appended_setup = validate_setup(appended.splitlines())
    if (
        appended_setup["quiescent_presented_layout_revision"] != 4
        or appended_setup["ready_divider"] != 70
    ):
        raise AssertionError("setup did not select the latest exact quiescent candidate")
    partial_later_candidate = "\n".join(later_candidate.splitlines()[:3])
    partial_appended = fixtures["positive-grant3.txt"].replace(
        "layout-region issued epoch=3",
        f"{partial_later_candidate}\nlayout-region issued epoch=3",
        1,
    )
    try:
        validate_setup(partial_appended.splitlines())
    except ValueError:
        pass
    else:
        raise AssertionError("incomplete latest qualifying leased revision did not invalidate setup")
    malformed_later_candidates = {
        "items-panes": partial_later_candidate.replace(
            "accepted leased actor layout items=5 panes=5",
            "accepted leased actor layout items=4 panes=4",
            1,
        ),
        "pane-key": partial_later_candidate.replace(
            "15:70:0:30:60",
            "99:70:0:30:60",
            1,
        ),
    }
    for label, malformed_candidate in malformed_later_candidates.items():
        malformed_appended = fixtures["positive-grant3.txt"].replace(
            "layout-region issued epoch=3",
            f"{malformed_candidate}\nlayout-region issued epoch=3",
            1,
        )
        try:
            validate_setup(malformed_appended.splitlines())
        except ValueError:
            continue
        raise AssertionError(f"malformed latest leased revision did not invalidate setup: {label}")
    streaming_lines = fixtures["positive-grant2.txt"].splitlines()
    streaming_setup = validate_setup(streaming_lines)
    streaming_native_accept = next(
        number
        for number, line in enumerate(streaming_lines, 1)
        if (match := ACCEPTED.match(line)) is not None
        and int(match["native_resize_revision"]) > 0
    )
    for label, prefix_line in (
        ("generic-settled", streaming_setup["settled_line"]),
        ("native-accepted", streaming_native_accept),
        ("native-terminal-and-settled", streaming_setup["quiescent_line"] - 1),
    ):
        try:
            validate_setup(streaming_lines[:prefix_line])
        except ValueError:
            continue
        raise AssertionError(f"streaming setup qualified before exact quiescence: {label}")
    cases = json.loads((fixture_root / "negative-cases.json").read_text(encoding="utf-8"))
    unexpectedly_valid = []
    for name, mutation in cases.items():
        positive = fixtures[mutation.get("fixture", positives[1])]
        mutated = positive.replace(mutation["old"], mutation["new"], 1)
        if mutated == positive:
            raise AssertionError(f"self-test mutation did not apply: {name}")
        try:
            lines = mutated.splitlines()
            validate_setup(lines)
            accepted_line = next(
                number
                for number, line in enumerate(lines, 1)
                if (match := ACCEPTED.match(line)) is not None and int(match["interaction"]) > 0
            )
            epoch = int(ACCEPTED.match(lines[accepted_line - 1])["epoch"])
            overrides = mutation.get("arguments", {})
            validate(
                lines,
                after_line=overrides.get("after_line", accepted_line - 1),
                accepted_line=overrides.get("accepted_line", accepted_line),
                expected_epoch=overrides.get("epoch", epoch),
            )
        except (StopIteration, TypeError, ValueError):
            continue
        unexpectedly_valid.append(name)
    if unexpectedly_valid:
        raise SystemExit(f"invalid managed topology cases passed: {unexpectedly_valid}")
    print(f"managed resize topology self-test passed: {len(positives)} positive, {len(cases)} negative")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("log", nargs="?", type=pathlib.Path)
    parser.add_argument("--setup", action="store_true")
    parser.add_argument("--after-line", type=int)
    parser.add_argument("--accepted-line", type=int)
    parser.add_argument("--epoch", type=int)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        if args.log is not None or args.setup or any(
            value is not None for value in (args.after_line, args.accepted_line, args.epoch)
        ):
            parser.error("--self-test does not accept a log or validation arguments")
        self_test()
        return
    if args.log is None:
        parser.error("log is required unless --self-test is used")
    lines = args.log.read_text(encoding="utf-8", errors="replace").splitlines()
    if args.setup:
        if any(value is not None for value in (args.after_line, args.accepted_line, args.epoch)):
            parser.error("--setup does not accept transaction anchors")
        result = validate_setup(lines)
    else:
        if any(value is None for value in (args.after_line, args.accepted_line, args.epoch)):
            parser.error("full validation requires --after-line, --accepted-line, and --epoch")
        result = validate(
            lines,
            after_line=args.after_line,
            accepted_line=args.accepted_line,
            expected_epoch=args.epoch,
        )
    print(json.dumps(result))


if __name__ == "__main__":
    main()
