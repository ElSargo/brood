#!/usr/bin/env python3
"""Validate the frozen render-scheduling behavior and performance gates."""

from __future__ import annotations

import argparse
import copy
import json
import math
import sys
from pathlib import Path
from typing import Any


class GateFailure(Exception):
    """One acceptance gate did not pass."""


def require(condition: bool, message: str) -> None:
    if not condition:
        raise GateFailure(message)


def object_field(value: dict[str, Any], name: str) -> dict[str, Any]:
    field = value.get(name)
    require(isinstance(field, dict), f"{name} must be an object")
    return field


def integer_field(value: dict[str, Any], name: str) -> int:
    field = value.get(name)
    require(isinstance(field, int) and not isinstance(field, bool), f"{name} must be an integer")
    require(field >= 0, f"{name} must be non-negative")
    return field


def boolean_field(value: dict[str, Any], name: str) -> bool:
    field = value.get(name)
    require(isinstance(field, bool), f"{name} must be a boolean")
    return field


def string_field(value: dict[str, Any], name: str) -> str:
    field = value.get(name)
    require(isinstance(field, str) and bool(field.strip()), f"{name} must be a non-empty string")
    return field


def finite_number_field(value: dict[str, Any], name: str) -> float:
    field = value.get(name)
    require(
        isinstance(field, (int, float)) and not isinstance(field, bool),
        f"{name} must be a number",
    )
    number = float(field)
    require(math.isfinite(number) and number >= 0.0, f"{name} must be finite and non-negative")
    return number


def check_latency(
    section: dict[str, Any],
    *,
    minimum_samples: int,
    p50_us: int | None,
    p95_us: int,
    p99_us: int,
    maximum_us: int,
    stddev_us: int,
    label: str,
) -> None:
    latency = object_field(section, "key_to_present")
    require(integer_field(latency, "samples") >= minimum_samples, f"{label}: too few key samples")
    if p50_us is not None:
        require(integer_field(latency, "p50_us") <= p50_us, f"{label}: key p50 exceeds {p50_us} us")
    require(integer_field(latency, "p95_us") <= p95_us, f"{label}: key p95 exceeds {p95_us} us")
    require(integer_field(latency, "p99_us") <= p99_us, f"{label}: key p99 exceeds {p99_us} us")
    require(integer_field(latency, "maximum_us") <= maximum_us, f"{label}: key maximum exceeds {maximum_us} us")
    require(finite_number_field(latency, "stddev_us") <= stddev_us, f"{label}: key stddev exceeds {stddev_us} us")


def check_rate(section: dict[str, Any], misses_name: str, percent: int, label: str) -> None:
    opportunities = integer_field(section, "measured_opportunities")
    misses = integer_field(section, misses_name)
    require(opportunities >= 600, f"{label}: fewer than 600 measured opportunities")
    require(misses * 100 <= opportunities * percent, f"{label}: {misses_name} exceeds {percent}%")


def check_intervals(section: dict[str, Any], p95_us: int, p99_us: int, label: str) -> None:
    intervals = object_field(section, "presented_frame_intervals")
    require(integer_field(intervals, "samples") >= 599, f"{label}: too few frame intervals")
    require(integer_field(intervals, "p95_us") <= p95_us, f"{label}: frame p95 exceeds {p95_us} us")
    require(integer_field(intervals, "p99_us") <= p99_us, f"{label}: frame p99 exceeds {p99_us} us")


def check_zero_failures(section: dict[str, Any], label: str) -> None:
    for name in (
        "reliable_transition_drops",
        "actor_failures",
        "renderer_failures",
        "token_protocol_errors",
    ):
        require(integer_field(section, name) == 0, f"{label}: {name} is nonzero")


def check_hot_path(report: dict[str, Any], name: str) -> None:
    hot_paths = object_field(report, "hot_paths")
    path = object_field(hot_paths, name)
    require(integer_field(path, "samples") >= 600, f"{name}: too few callback samples")
    require(
        integer_field(path, "callbacks_lifetime") == integer_field(path, "samples"),
        f"{name}: callback retention is not the complete measured set",
    )
    require(integer_field(path, "p99_us") <= 2_500, f"{name}: callback p99 exceeds 2500 us")
    require(integer_field(path, "maximum_us") <= 10_000, f"{name}: callback maximum exceeds 10000 us")
    require(boolean_field(path, "driver_present_excluded"), f"{name}: driver presentation was not separated")
    driver = object_field(path, "driver_present")
    require(integer_field(driver, "samples") > 0, f"{name}: driver presentation has no samples")
    require(
        integer_field(driver, "calls_lifetime") == integer_field(driver, "samples"),
        f"{name}: driver retention is not the complete measured set",
    )
    integer_field(driver, "p99_us")
    integer_field(driver, "maximum_us")
    if name == "winit":
        kinds = object_field(path, "callback_kind_counts")
        for kind in ("window_event", "user_event", "about_to_wait"):
            require(integer_field(kinds, kind) > 0, f"winit: {kind} callbacks were not exercised")


def validate(report: dict[str, Any]) -> None:
    require(integer_field(report, "schema_version") == 1, "unsupported report schema_version")
    provenance = object_field(report, "provenance")
    require(string_field(provenance, "harness") == "brood-test-render-scheduling", "unexpected harness")
    require(boolean_field(provenance, "release_build"), "proof did not use a release build")
    require(boolean_field(provenance, "nix_development_environment"), "proof did not run through Nix")
    for name in ("client_binary", "component", "artifact_directory"):
        string_field(provenance, name)
    captures = provenance.get("captures")
    require(isinstance(captures, list) and len(captures) >= 7, "proof omitted required nested captures")
    for capture in captures:
        require(isinstance(capture, dict), "capture evidence must be an object")
        string_field(capture, "path")
        require(integer_field(capture, "bytes") > 0, "capture evidence is empty")

    behavior = object_field(report, "nested_behavior")
    require(boolean_field(behavior, "real_wasm_component"), "nested proof did not use a real WASM component")
    require(integer_field(behavior, "owned_visible_panes") >= 2, "nested proof did not show two actor-owned panes")
    require(integer_field(behavior, "real_keyboard_events") >= 200, "nested proof sent fewer than 200 real keys")
    require(boolean_field(behavior, "winit_wgpu_composition"), "nested proof did not reach Winit/WGPU composition")
    require(boolean_field(behavior, "smithay_hosted_content"), "nested proof did not reach Smithay-hosted content")
    require(integer_field(behavior, "renderer_recreations") >= 1, "nested proof did not recreate the renderer")
    require(integer_field(behavior, "visibility_changes") >= 2, "nested proof did not hide and restore actor content")
    require(integer_field(behavior, "atomic_multi_pane_batches") > 0, "nested proof observed no multi-pane actor batch")
    require(integer_field(behavior, "partial_actor_frame_promotions") == 0, "nested proof observed a partial actor-frame promotion")
    require(integer_field(behavior, "renderer_replays") >= 1, "nested proof observed no renderer replay")

    invariants = object_field(report, "behavior_invariants")
    for name in (
        "request_coalescing",
        "subscription_semantics",
        "one_outstanding_token_and_retarget",
        "causal_watermark",
        "retry_empty_and_abandon",
        "late_acceptance",
        "atomic_visible_batch",
        "supersession",
        "generation_replay_cancellation",
        "full_correlation_stages",
        "resize_orthogonality",
        "reliable_and_coalesced_queues",
    ):
        result = object_field(invariants, name)
        require(integer_field(result, "exercised") > 0, f"{name}: invariant was not exercised")
        require(integer_field(result, "violations") == 0, f"{name}: invariant violation observed")

    deterministic = object_field(report, "deterministic_scheduler")
    for case in ("fixed_60hz", "fixed_120hz", "alternating_vrr", "cost_and_retarget"):
        result = object_field(deterministic, case)
        require(integer_field(result, "opportunities") >= 10_000, f"{case}: too few opportunities")
        for counter in (
            "invalid_token_attempts",
            "request_revision_checks",
            "armed_dispatch_checks",
            "latest_safe_dispatch_checks",
            "infeasible_target_step_checks",
            "catch_up_burst_checks",
        ):
            require(integer_field(result, counter) > 0, f"{case}: {counter} was not exercised")
        for counter in (
            "duplicate_tokens_accepted",
            "foreign_tokens_accepted",
            "reused_tokens_accepted",
            "out_of_order_tokens_accepted",
            "lost_request_revisions",
            "stale_armed_dispatches",
            "latest_safe_dispatch_errors",
            "infeasible_target_step_errors",
            "catch_up_bursts",
        ):
            require(integer_field(result, counter) == 0, f"{case}: {counter} is nonzero")
        require(integer_field(result, "background_progress") > 0, f"{case}: background work made no progress")

    unsaturated = object_field(report, "unsaturated_60hz")
    require(integer_field(unsaturated, "warmup_opportunities") >= 60, "unsaturated: warm-up too short")
    check_latency(
        unsaturated,
        minimum_samples=200,
        p50_us=12_000,
        p95_us=20_000,
        p99_us=25_000,
        maximum_us=50_000,
        stddev_us=6_000,
        label="unsaturated",
    )
    check_rate(unsaturated, "render_hint_deadline_misses", 1, "unsaturated")
    check_rate(unsaturated, "intended_target_misses", 2, "unsaturated")
    check_intervals(unsaturated, 20_000, 25_000, "unsaturated")
    check_zero_failures(unsaturated, "unsaturated")

    saturated = object_field(report, "saturated_60hz")
    require(integer_field(saturated, "warmup_opportunities") >= 60, "saturated: warm-up too short")
    check_latency(
        saturated,
        minimum_samples=200,
        p50_us=None,
        p95_us=25_000,
        p99_us=35_000,
        maximum_us=75_000,
        stddev_us=8_000,
        label="saturated",
    )
    check_rate(saturated, "render_hint_deadline_misses", 5, "saturated")
    check_rate(saturated, "intended_target_misses", 5, "saturated")
    check_intervals(saturated, 25_000, 33_334, "saturated")
    check_zero_failures(saturated, "saturated")
    progress = saturated.get("background_actor_progress")
    require(isinstance(progress, list) and len(progress) >= 3, "saturated: fewer than three background actors")
    require(all(isinstance(item, int) and not isinstance(item, bool) and item > 0 for item in progress),
            "saturated: a background actor made no progress")

    direct = object_field(report, "direct_120hz")
    require(integer_field(direct, "warmup_opportunities") >= 60, "120hz: warm-up too short")
    check_rate(direct, "intended_target_misses", 3, "120hz")
    require(integer_field(direct, "frames_per_second_milli") >= 114_000, "120hz: presentation rate below 114 fps")
    check_intervals(direct, 10_000, 16_667, "120hz")
    require(integer_field(direct, "catch_up_bursts") == 0, "120hz: catch-up bursts observed")

    check_hot_path(report, "winit")
    check_hot_path(report, "smithay")

    idle = object_field(report, "idle")
    require(integer_field(idle, "duration_ms") >= 5_000, "idle: interval shorter than five seconds")
    for name in (
        "render_hints",
        "actor_render_turns",
        "scheduler_deadline_wakeups",
        "scheduler_redraw_requests",
        "periodic_actor_polls",
        "periodic_renderer_polls",
        "periodic_lifecycle_polls",
    ):
        require(integer_field(idle, name) == 0, f"idle: {name} is nonzero")


def valid_fixture() -> dict[str, Any]:
    deterministic_case = {
        "opportunities": 10_000,
        "invalid_token_attempts": 4,
        "duplicate_tokens_accepted": 0,
        "foreign_tokens_accepted": 0,
        "reused_tokens_accepted": 0,
        "out_of_order_tokens_accepted": 0,
        "request_revision_checks": 1,
        "lost_request_revisions": 0,
        "armed_dispatch_checks": 1,
        "stale_armed_dispatches": 0,
        "latest_safe_dispatch_checks": 1,
        "latest_safe_dispatch_errors": 0,
        "infeasible_target_step_checks": 1,
        "infeasible_target_step_errors": 0,
        "catch_up_burst_checks": 1,
        "catch_up_bursts": 0,
        "background_progress": 1,
    }
    failures = {
        "reliable_transition_drops": 0,
        "actor_failures": 0,
        "renderer_failures": 0,
        "token_protocol_errors": 0,
    }
    return {
        "schema_version": 1,
        "provenance": {
            "harness": "brood-test-render-scheduling",
            "release_build": True,
            "nix_development_environment": True,
            "client_binary": "/fixture/brood-client",
            "component": "/fixture/render.component.wasm",
            "artifact_directory": "/fixture/artifacts",
            "captures": [
                {"path": f"/fixture/capture-{index}.png", "bytes": 1}
                for index in range(7)
            ],
        },
        "nested_behavior": {
            "real_wasm_component": True,
            "owned_visible_panes": 2,
            "real_keyboard_events": 400,
            "winit_wgpu_composition": True,
            "smithay_hosted_content": True,
            "renderer_recreations": 1,
            "visibility_changes": 2,
            "atomic_multi_pane_batches": 1,
            "partial_actor_frame_promotions": 0,
            "renderer_replays": 1,
        },
        "behavior_invariants": {
            name: {"exercised": 1, "violations": 0}
            for name in (
                "request_coalescing",
                "subscription_semantics",
                "one_outstanding_token_and_retarget",
                "causal_watermark",
                "retry_empty_and_abandon",
                "late_acceptance",
                "atomic_visible_batch",
                "supersession",
                "generation_replay_cancellation",
                "full_correlation_stages",
                "resize_orthogonality",
                "reliable_and_coalesced_queues",
            )
        },
        "deterministic_scheduler": {
            name: copy.deepcopy(deterministic_case)
            for name in ("fixed_60hz", "fixed_120hz", "alternating_vrr", "cost_and_retarget")
        },
        "unsaturated_60hz": {
            "warmup_opportunities": 60,
            "measured_opportunities": 600,
            "key_to_present": {
                "samples": 200,
                "p50_us": 12_000,
                "p95_us": 20_000,
                "p99_us": 25_000,
                "maximum_us": 50_000,
                "stddev_us": 6_000.0,
            },
            "render_hint_deadline_misses": 6,
            "intended_target_misses": 12,
            "presented_frame_intervals": {"samples": 599, "p95_us": 20_000, "p99_us": 25_000},
            **failures,
        },
        "saturated_60hz": {
            "warmup_opportunities": 60,
            "measured_opportunities": 600,
            "key_to_present": {
                "samples": 200,
                "p50_us": 0,
                "p95_us": 25_000,
                "p99_us": 35_000,
                "maximum_us": 75_000,
                "stddev_us": 8_000.0,
            },
            "render_hint_deadline_misses": 30,
            "intended_target_misses": 30,
            "presented_frame_intervals": {"samples": 599, "p95_us": 25_000, "p99_us": 33_334},
            "background_actor_progress": [1, 1, 1],
            **failures,
        },
        "direct_120hz": {
            "warmup_opportunities": 60,
            "measured_opportunities": 600,
            "intended_target_misses": 18,
            "frames_per_second_milli": 114_000,
            "presented_frame_intervals": {"samples": 599, "p95_us": 10_000, "p99_us": 16_667},
            "catch_up_bursts": 0,
        },
        "hot_paths": {
            "winit": {
                "samples": 600,
                "callbacks_lifetime": 600,
                "p99_us": 2_500,
                "maximum_us": 10_000,
                "driver_present_excluded": True,
                "callback_kind_counts": {
                    "window_event": 1,
                    "user_event": 1,
                    "about_to_wait": 1,
                },
                "driver_present": {
                    "samples": 1,
                    "calls_lifetime": 1,
                    "p99_us": 1,
                    "maximum_us": 1,
                },
            },
            "smithay": {
                "samples": 600,
                "callbacks_lifetime": 600,
                "p99_us": 2_500,
                "maximum_us": 10_000,
                "driver_present_excluded": True,
                "driver_present": {
                    "samples": 1,
                    "calls_lifetime": 1,
                    "p99_us": 1,
                    "maximum_us": 1,
                },
            },
        },
        "idle": {
            "duration_ms": 5_000,
            "render_hints": 0,
            "actor_render_turns": 0,
            "scheduler_deadline_wakeups": 0,
            "scheduler_redraw_requests": 0,
            "periodic_actor_polls": 0,
            "periodic_renderer_polls": 0,
            "periodic_lifecycle_polls": 0,
        },
    }


def self_test() -> None:
    validate(valid_fixture())
    failures = {
        "missing runtime field": ("unsaturated_60hz", "presented_frame_intervals"),
        "too few panes": ("nested_behavior", "owned_visible_panes"),
        "idle polling": ("idle", "periodic_actor_polls"),
        "deadline regression": ("unsaturated_60hz", "render_hint_deadline_misses"),
        "short active cadence window": (
            "direct_120hz",
            "presented_frame_intervals",
            "samples",
        ),
        "hot-path regression": ("hot_paths", "winit", "p99_us"),
        "callback retention rollover": ("hot_paths", "winit", "callbacks_lifetime"),
        "unexercised invariant": ("behavior_invariants", "full_correlation_stages", "exercised"),
        "unexercised scheduler probe": (
            "deterministic_scheduler",
            "fixed_60hz",
            "latest_safe_dispatch_checks",
        ),
    }
    for label, path in failures.items():
        fixture = valid_fixture()
        parent: Any = fixture
        for name in path[:-1]:
            parent = parent[name]
        field = path[-1]
        if label == "missing runtime field":
            del parent[field]
        elif label == "callback retention rollover":
            parent[field] += 1
        elif label == "too few panes":
            parent[field] = 1
        elif label == "idle polling":
            parent[field] = 1
        elif label == "deadline regression":
            parent[field] = 7
        elif label == "short active cadence window":
            parent[field] = 598
        elif label in ("unexercised invariant", "unexercised scheduler probe"):
            parent[field] = 0
        else:
            parent[field] = 2_501
        try:
            validate(fixture)
        except GateFailure:
            continue
        raise GateFailure(f"negative analyzer fixture passed: {label}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("report", type=Path, nargs="?")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    try:
        if args.self_test:
            require(args.report is None, "--self-test does not accept a report")
            self_test()
            print("render_scheduling_analyzer_self_test=passed")
            return 0
        require(args.report is not None, "a report path is required")
        decoded = json.loads(args.report.read_text(encoding="utf-8"))
        require(isinstance(decoded, dict), "report root must be an object")
        validate(decoded)
    except (OSError, json.JSONDecodeError, GateFailure) as error:
        print(f"render_scheduling_report=failed error={error}", file=sys.stderr)
        return 1
    print(f"render_scheduling_report=verified path={args.report}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
