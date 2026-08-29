#!/usr/bin/env python3
"""Assemble only observed render-scheduling metrics into the gate report."""

from __future__ import annotations

import argparse
import json
import math
import re
import sys
from pathlib import Path
from typing import Any


DIAGNOSTIC_PREFIX = "render scheduling diagnostics "
DETERMINISTIC_PREFIX = "render scheduling deterministic "


class EvidenceError(Exception):
    """Required machine evidence is missing or ambiguous."""


def load_json(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise EvidenceError(f"{path}: JSON root is not an object")
    return value


def diagnostic(path: Path, prefix: str) -> dict[str, Any]:
    matches = [
        line[len(prefix) :]
        for line in path.read_text(encoding="utf-8", errors="replace").splitlines()
        if line.startswith(prefix)
    ]
    if len(matches) != 1:
        raise EvidenceError(f"{path}: expected exactly one {prefix.strip()!r} line, found {len(matches)}")
    value = json.loads(matches[0])
    if not isinstance(value, dict):
        raise EvidenceError(f"{path}: diagnostic payload is not an object")
    return value


def deterministic_diagnostics(path: Path) -> dict[str, Any]:
    records: dict[str, dict[str, Any]] = {}
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        offset = line.find(DETERMINISTIC_PREFIX)
        if offset < 0:
            continue
        payload = line[offset + len(DETERMINISTIC_PREFIX) :]
        value = json.loads(payload)
        if not isinstance(value, dict):
            raise EvidenceError(f"{path}: deterministic payload is not an object")
        case = field(value, "case", "deterministic payload")
        if not isinstance(case, str) or case in records:
            raise EvidenceError(f"{path}: missing or duplicate deterministic case {case!r}")
        records[case] = {name: item for name, item in value.items() if name != "case"}
    expected = {"fixed_60hz", "fixed_120hz", "alternating_vrr", "cost_and_retarget"}
    if set(records) != expected:
        raise EvidenceError(f"{path}: deterministic cases are {sorted(records)}, expected {sorted(expected)}")
    return records


def passing_test_names(path: Path) -> set[str]:
    names = set()
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        if line.startswith("test ") and line.endswith(" ... ok"):
            names.add(line.removeprefix("test ").removesuffix(" ... ok"))
    return names


def behavior_invariants(path: Path) -> dict[str, Any]:
    passed = passing_test_names(path)
    required = {
        "request_coalescing": ("render_scheduler::tests::request_coalesces_without_losing_request_during_turn",),
        "subscription_semantics": ("render_scheduler::tests::subscription_and_separate_request_have_independent_state",),
        "one_outstanding_token_and_retarget": (
            "render_scheduler::tests::one_outstanding_token_and_armed_retarget_keep_fixed_dispatch",
        ),
        "causal_watermark": (
            "render_scheduler::tests::deadline_dispatch_freezes_cutoff_without_waiting_for_reduction",
            "render_scheduler::tests::continuous_one_millisecond_input_cannot_starve_due_dispatch",
            "actor_runtime::tests::retained_presentation_fifo_orders_cutoff_hint_before_later_input",
        ),
        "retry_empty_and_abandon": ("render_scheduler::tests::rejected_empty_and_abandoned_turns_preserve_contract",),
        "late_acceptance": ("render_scheduler::tests::late_batch_is_accepted_and_newer_frame_supersedes_old",),
        "atomic_visible_batch": (
            "render_scheduler::tests::atomic_frames_promote_visible_panes_together_and_ignore_hidden_barrier",
            "render_scheduler::tests::atomic_batch_rejects_duplicate_panes_without_mutation",
        ),
        "supersession": ("render_scheduler::tests::late_batch_is_accepted_and_newer_frame_supersedes_old",),
        "generation_replay_cancellation": (
            "plugin_graphics::tests::generation_cancel_and_renderer_replay_do_not_accept_stale_tokens",
        ),
        "full_correlation_stages": ("render_scheduler::tests::correlation_records_every_render_stage",),
        "resize_orthogonality": ("plugin_graphics::tests::tagged_work_is_admitted_only_for_the_current_pane_epoch",),
        "reliable_and_coalesced_queues": (
            "actor_runtime::tests::reliable_transitions_and_coalesced_state_remain_bounded",
        ),
    }
    result = {}
    for invariant, tests in required.items():
        missing = [name for name in tests if name not in passed]
        if missing:
            raise EvidenceError(f"{path}: {invariant} lacks passing focused tests {missing}")
        result[invariant] = {"exercised": len(tests), "violations": 0}
    return result


def component_actor_started(path: Path, actor: str, component: Path) -> bool:
    marker = f"started presentation WASM actor id={actor} kind=wasm-component "
    expected_path = f"path={component.resolve()}"
    return any(
        marker in line and expected_path in line
        for line in path.read_text(encoding="utf-8", errors="replace").splitlines()
    )


def maximum_promoted_panes(path: Path, actor: str) -> int:
    pattern = re.compile(
        rf"render scheduling actor-frame promoted actor={re.escape(actor)} .* pane_count=([0-9]+) "
    )
    counts = [
        int(match.group(1))
        for line in path.read_text(encoding="utf-8", errors="replace").splitlines()
        if (match := pattern.search(line)) is not None
    ]
    return max(counts, default=0)


def require_recovery_visual_sequence(path: Path) -> None:
    lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    recovery_pattern = re.compile(
        r"^recovering renderer generation=([0-9]+) replacement=([0-9]+): "
        r"render-scheduling F9 proof recovery$"
    )
    promoted_pattern = re.compile(
        r"render scheduling actor-frame promoted actor=render-proof .* "
        r"pane_count=2 panes=\[[^]]+\]"
    )
    recovery_matches = [
        (index, match)
        for index, line in enumerate(lines)
        if (match := recovery_pattern.match(line)) is not None
    ]
    if len(recovery_matches) != 1:
        raise EvidenceError(
            f"{path}: expected one explicit F9 renderer recovery, found {len(recovery_matches)}"
        )
    recovery_index, recovery = recovery_matches[0]
    if int(recovery.group(2)) <= int(recovery.group(1)):
        raise EvidenceError(f"{path}: F9 recovery did not advance renderer generation")
    if not any(promoted_pattern.search(line) for line in lines[recovery_index + 1 :]):
        raise EvidenceError(f"{path}: no two-pane actor frame was promoted after F9 recovery")


def field(value: dict[str, Any], name: str, context: str) -> Any:
    if name not in value:
        raise EvidenceError(f"{context}: missing raw field {name!r}")
    return value[name]


def scenario(path: Path, expected: str) -> dict[str, Any]:
    value = diagnostic(path, DIAGNOSTIC_PREFIX)
    if field(value, "scenario", str(path)) != expected:
        raise EvidenceError(f"{path}: expected scenario {expected!r}")
    return value


def sample_summary(samples: list[int], *, context: str) -> dict[str, int | float]:
    if not samples:
        raise EvidenceError(f"{context}: sample array is empty")
    ordered = sorted(samples)

    def percentile(percent: int) -> int:
        return ordered[(len(ordered) - 1) * percent // 100]

    mean = sum(ordered) / len(ordered)
    variance = sum((sample - mean) ** 2 for sample in ordered) / len(ordered)
    return {
        "p50_ns": percentile(50),
        "p95_ns": percentile(95),
        "p99_ns": percentile(99),
        "maximum_ns": ordered[-1],
        "stddev_ns": math.sqrt(variance),
    }


def scenario_gate(value: dict[str, Any], *, direct: bool = False) -> dict[str, Any]:
    context = f"scenario {value.get('scenario')}"
    graphics = field(value, "graphics", context)
    if not isinstance(graphics, dict):
        raise EvidenceError(f"{context}: graphics must be an object")
    scheduler = field(graphics, "render_scheduler", context)
    if not isinstance(scheduler, dict):
        raise EvidenceError(f"{context}: render_scheduler must be an object")
    interval_samples = field(graphics, "measured_frame_intervals_ns", context)
    if not isinstance(interval_samples, list) or not all(
        isinstance(item, int) and not isinstance(item, bool) and item > 0 for item in interval_samples
    ):
        raise EvidenceError(f"{context}: measured_frame_intervals_ns must contain positive integers")
    interval_summary = sample_summary(interval_samples, context=f"{context} frame intervals")
    for raw_name, summary_name in (
        ("frame_interval_p95_ns", "p95_ns"),
        ("frame_interval_p99_ns", "p99_ns"),
    ):
        if field(graphics, raw_name, context) != interval_summary[summary_name]:
            raise EvidenceError(f"{context}: {raw_name} disagrees with chronological raw samples")
    intervals = {
        "samples": len(interval_samples),
        "p95_us": (int(interval_summary["p95_ns"]) + 999) // 1_000,
        "p99_us": (int(interval_summary["p99_ns"]) + 999) // 1_000,
    }
    common = {
        "warmup_opportunities": field(graphics, "warmup_opportunities", context),
        "measured_opportunities": field(graphics, "measured_opportunities", context),
        "intended_target_misses": field(graphics, "measured_target_misses", context),
        "presented_frame_intervals": intervals,
    }
    if direct:
        elapsed_ns = sum(interval_samples)
        if elapsed_ns <= 0:
            raise EvidenceError(f"{context}: frame interval duration is zero")
        common.update(
            {
                "frames_per_second_milli": len(interval_samples) * 1_000_000_000_000 // elapsed_ns,
                "catch_up_bursts": field(scheduler, "catch_up_bursts", context),
            }
        )
        return common
    key_samples = field(graphics, "measured_key_to_present_ns", context)
    if not isinstance(key_samples, list) or not all(
        isinstance(item, int) and not isinstance(item, bool) and item >= 0 for item in key_samples
    ):
        raise EvidenceError(f"{context}: measured_key_to_present_ns must contain non-negative integers")
    key_summary = sample_summary(key_samples, context=f"{context} key-to-present")
    for raw_name, summary_name in (
        ("key_to_present_p50_ns", "p50_ns"),
        ("key_to_present_p95_ns", "p95_ns"),
        ("key_to_present_p99_ns", "p99_ns"),
        ("key_to_present_max_ns", "maximum_ns"),
    ):
        if field(graphics, raw_name, context) != key_summary[summary_name]:
            raise EvidenceError(f"{context}: {raw_name} disagrees with chronological raw samples")
    if abs(field(graphics, "key_to_present_stddev_ns", context) - key_summary["stddev_ns"]) > 1:
        raise EvidenceError(f"{context}: key_to_present_stddev_ns disagrees with raw samples")
    common.update(
        {
            "key_to_present": {
                "samples": len(key_samples),
                "p50_us": (int(key_summary["p50_ns"]) + 999) // 1_000,
                "p95_us": (int(key_summary["p95_ns"]) + 999) // 1_000,
                "p99_us": (int(key_summary["p99_ns"]) + 999) // 1_000,
                "maximum_us": (int(key_summary["maximum_ns"]) + 999) // 1_000,
                "stddev_us": key_summary["stddev_ns"] / 1_000.0,
            },
            "render_hint_deadline_misses": field(graphics, "measured_deadline_misses", context),
            "reliable_transition_drops": field(value, "reliable_transition_drops", context),
            "actor_failures": field(value, "actor_failures", context),
            "renderer_failures": field(value, "renderer_failures", context),
            "token_protocol_errors": field(scheduler, "protocol_errors", context),
        }
    )
    return common


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--deterministic-log", required=True, type=Path)
    parser.add_argument("--unsaturated-log", required=True, type=Path)
    parser.add_argument("--saturated-log", required=True, type=Path)
    parser.add_argument("--direct-log", required=True, type=Path)
    parser.add_argument("--server-status", required=True, type=Path)
    parser.add_argument("--client-binary", required=True, type=Path)
    parser.add_argument("--component", required=True, type=Path)
    parser.add_argument("--artifact-directory", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()

    try:
        deterministic = deterministic_diagnostics(args.deterministic_log)
        invariants = behavior_invariants(args.deterministic_log)
        unsaturated = scenario(args.unsaturated_log, "unsaturated_60hz")
        require_recovery_visual_sequence(args.unsaturated_log)
        saturated = scenario(args.saturated_log, "saturated_60hz")
        direct = scenario(args.direct_log, "direct_120hz")
        server = load_json(args.server_status)
        capture_names = (
            "unsaturated_60hz.png",
            "unsaturated_60hz-visible-before.png",
            "unsaturated_60hz-recovered.png",
            "unsaturated_60hz-one-pane-hidden.png",
            "unsaturated_60hz-visible-after.png",
            "saturated_60hz.png",
            "direct_120hz.png",
        )
        captures = []
        for name in capture_names:
            capture = args.artifact_directory / "captures" / name
            try:
                size = capture.stat().st_size
            except OSError as error:
                raise EvidenceError(f"required nested capture is missing: {capture}: {error}") from error
            if size <= 0:
                raise EvidenceError(f"required nested capture is empty: {capture}")
            captures.append({"path": str(capture.resolve()), "bytes": size})

        saturated_graphics = field(saturated, "graphics", "saturated scenario")
        if not isinstance(saturated_graphics, dict):
            raise EvidenceError("saturated scenario: graphics must be an object")
        progress_records = field(saturated_graphics, "actor_progress", "saturated scenario")
        if not isinstance(progress_records, list):
            raise EvidenceError("saturated scenario: actor_progress must be an array")
        progress = {
            record.get("actor"): record.get("accepted")
            for record in progress_records
            if isinstance(record, dict)
        }
        background = []
        for actor in ("render-cpu-a", "render-cpu-b", "render-cpu-c"):
            value = field(progress, actor, "client actor progress")
            if not isinstance(value, int) or isinstance(value, bool):
                raise EvidenceError(f"client actor progress: {actor} is not an integer")
            background.append(value)
        backend_progress = field(server, "backend_actor_progress_by_actor", "server status")
        if not isinstance(backend_progress, dict):
            raise EvidenceError("server status: backend_actor_progress_by_actor must be an object")
        for actor in ("render-cpu-a", "render-cpu-b", "render-cpu-c"):
            value = field(backend_progress, actor, "backend actor progress")
            if not isinstance(value, int) or isinstance(value, bool) or value <= 0:
                raise EvidenceError(f"backend actor progress: {actor} did not make measurable progress")
        saturated_behavior = field(saturated, "nested_behavior", "saturated scenario")
        if not isinstance(saturated_behavior, dict):
            raise EvidenceError("saturated scenario: nested_behavior must be an object")
        admitted_actor_messages = field(
            saturated_behavior,
            "backend_actor_messages_enqueued",
            "saturated nested behavior",
        )
        if (
            not isinstance(admitted_actor_messages, int)
            or isinstance(admitted_actor_messages, bool)
            or admitted_actor_messages < 200
        ):
            raise EvidenceError(
                "saturated nested behavior: fewer than 200 backend actor messages were admitted"
            )
        for name in ("backend_actor_messages_completed", "backend_requests_completed"):
            value = field(server, name, "server status")
            if not isinstance(value, int) or isinstance(value, bool) or value < 100:
                raise EvidenceError(f"server status: {name} did not prove bulk traffic")
        for name in (
            "backend_actor_event_drops",
            "backend_actor_message_drops",
            "backend_request_drops",
            "smithay_input_drops",
            "backend_lifecycle_drops",
            "plugin_actor_failures",
            "wayland_dispatch_errors",
        ):
            if field(server, name, "server status") != 0:
                raise EvidenceError(f"server status: {name} is nonzero")
        if field(server, "frames", "server status") <= 0:
            raise EvidenceError("server status: Smithay-hosted content did not render")

        behavior_raw = field(unsaturated, "nested_behavior", "unsaturated scenario")
        if not isinstance(behavior_raw, dict):
            raise EvidenceError("unsaturated scenario: nested_behavior must be an object")
        behavior = dict(behavior_raw)
        if not component_actor_started(args.unsaturated_log, "render-proof", args.component):
            raise EvidenceError(
                "unsaturated scenario: render-proof did not start the packaged Component Model actor"
            )
        behavior["real_wasm_component"] = True
        promoted_panes = maximum_promoted_panes(args.unsaturated_log, "render-proof")
        visible_panes = field(behavior_raw, "owned_visible_panes", "unsaturated nested behavior")
        if (
            not isinstance(visible_panes, int)
            or isinstance(visible_panes, bool)
            or visible_panes < 2
            or promoted_panes < 2
        ):
            raise EvidenceError(
                "unsaturated scenario: render-proof did not own and expose two visible panes"
            )
        behavior["owned_visible_panes"] = min(visible_panes, promoted_panes)
        unsaturated_winit = field(unsaturated, "winit_hot_path", "unsaturated scenario")
        if not isinstance(unsaturated_winit, dict):
            raise EvidenceError("unsaturated scenario: winit_hot_path must be an object")
        unsaturated_driver_calls = field(
            unsaturated_winit, "driver_present_calls_lifetime", "unsaturated Winit hot path"
        )
        if (
            not isinstance(unsaturated_driver_calls, int)
            or isinstance(unsaturated_driver_calls, bool)
            or unsaturated_driver_calls <= 0
        ):
            raise EvidenceError("unsaturated scenario: Winit/WGPU presentation was not exercised")
        behavior["winit_wgpu_composition"] = True
        # This observation belongs to the server. Do not accept the client's
        # proof-mode environment flag as evidence of Smithay-hosted rendering.
        behavior["smithay_hosted_content"] = True
        idle = field(unsaturated, "idle", "unsaturated scenario")
        winit_raw = field(saturated, "winit_hot_path", "saturated scenario")
        if not isinstance(winit_raw, dict):
            raise EvidenceError("saturated scenario: winit_hot_path must be an object")
        winit_samples = field(
            winit_raw,
            "measured_callback_ns_post_warmup",
            "saturated Winit hot path",
        )
        if not isinstance(winit_samples, list) or not all(
            isinstance(item, int) and not isinstance(item, bool) and item >= 0
            for item in winit_samples
        ):
            raise EvidenceError("saturated Winit hot path: measured_callback_ns is invalid")
        winit_summary = sample_summary(winit_samples, context="saturated Winit callbacks")
        callback_kinds = field(
            winit_raw, "callback_kind_counts_post_warmup", "saturated Winit hot path"
        )
        required_callback_kinds = {"window_event", "user_event", "about_to_wait"}
        if not isinstance(callback_kinds, dict) or not required_callback_kinds.issubset(
            callback_kinds
        ):
            raise EvidenceError("saturated Winit hot path: callback kinds were not fully reported")
        if any(
            not isinstance(callback_kinds[kind], int)
            or isinstance(callback_kinds[kind], bool)
            or callback_kinds[kind] <= 0
            for kind in required_callback_kinds
        ):
            raise EvidenceError("saturated Winit hot path: one callback kind was not exercised")
        if sum(callback_kinds[kind] for kind in required_callback_kinds) != len(winit_samples):
            raise EvidenceError(
                "saturated Winit hot path: callback-kind retention is inconsistent"
            )
        callbacks_lifetime = field(
            winit_raw, "callbacks_lifetime", "saturated Winit hot path"
        )
        if (
            not isinstance(callbacks_lifetime, int)
            or isinstance(callbacks_lifetime, bool)
            or callbacks_lifetime != len(winit_samples)
        ):
            raise EvidenceError(
                "saturated Winit hot path: raw callback retention rolled over or is incomplete"
            )
        driver_present_calls = field(
            winit_raw, "driver_present_calls_lifetime", "saturated Winit hot path"
        )
        if (
            not isinstance(driver_present_calls, int)
            or isinstance(driver_present_calls, bool)
            or driver_present_calls <= 0
        ):
            raise EvidenceError("saturated Winit hot path: driver present separation was not exercised")
        driver_samples = field(
            winit_raw, "driver_present_samples_ns_lifetime", "saturated Winit hot path"
        )
        if not isinstance(driver_samples, list) or not all(
            isinstance(item, int) and not isinstance(item, bool) and item >= 0
            for item in driver_samples
        ):
            raise EvidenceError("saturated Winit hot path: driver present samples are invalid")
        if driver_present_calls != len(driver_samples):
            raise EvidenceError(
                "saturated Winit hot path: raw driver retention rolled over or is incomplete"
            )
        driver_summary = sample_summary(driver_samples, context="saturated Winit driver presents")
        gpu_wait_samples = field(
            winit_raw,
            "gpu_submission_wait_samples_ns_retained",
            "saturated Winit GPU submission waits",
        )
        if not isinstance(gpu_wait_samples, list) or not all(
            isinstance(item, int) and not isinstance(item, bool) and item >= 0
            for item in gpu_wait_samples
        ):
            raise EvidenceError("saturated Winit GPU submission wait samples are invalid")
        gpu_wait_calls = field(
            winit_raw,
            "gpu_submission_wait_calls_lifetime",
            "saturated Winit GPU submission waits",
        )
        if (
            not isinstance(gpu_wait_calls, int)
            or isinstance(gpu_wait_calls, bool)
            or gpu_wait_calls <= 0
            or len(gpu_wait_samples) > 4_096
        ):
            raise EvidenceError("saturated Winit GPU submission wait lane was not exercised")
        if gpu_wait_calls != len(gpu_wait_samples):
            raise EvidenceError(
                "saturated Winit GPU submission wait retention rolled over or is incomplete"
            )
        gpu_wait_summary = sample_summary(
            gpu_wait_samples,
            context="saturated Winit GPU submission waits",
        )
        winit = {
            "samples": len(winit_samples),
            "callbacks_lifetime": callbacks_lifetime,
            "p99_us": (int(winit_summary["p99_ns"]) + 999) // 1_000,
            "maximum_us": (int(winit_summary["maximum_ns"]) + 999) // 1_000,
            "driver_present_excluded": field(
                winit_raw, "driver_present_excluded", "saturated Winit hot path"
            ),
            "callback_kind_counts": callback_kinds,
            "driver_present": {
                "samples": len(driver_samples),
                "calls_lifetime": driver_present_calls,
                "p99_us": (int(driver_summary["p99_ns"]) + 999) // 1_000,
                "maximum_us": (int(driver_summary["maximum_ns"]) + 999) // 1_000,
            },
        }
        smithay = {
            "samples": field(server, "smithay_callback_retained_samples", "server status"),
            "callbacks_lifetime": field(server, "smithay_callback_count", "server status"),
            "p99_us": field(server, "smithay_callback_p99_us", "server status"),
            "maximum_us": field(server, "smithay_callback_max_us", "server status"),
            # The server reports driver calls in a separate distribution and
            # smithay_callback_* is its exclusive duration.
            "driver_present_excluded": field(
                server, "smithay_callback_excludes_driver_calls", "server status"
            ),
            "driver_present": {
                "samples": field(
                    server, "smithay_driver_call_retained_samples", "server status"
                ),
                "calls_lifetime": field(server, "smithay_driver_call_count", "server status"),
                "p99_us": field(server, "smithay_driver_call_p99_us", "server status"),
                "maximum_us": field(server, "smithay_driver_call_max_us", "server status"),
            },
        }
        if smithay["callbacks_lifetime"] != smithay["samples"]:
            raise EvidenceError(
                "server status: Smithay callback retention rolled over after warm-up reset"
            )
        if smithay["driver_present"]["calls_lifetime"] != smithay["driver_present"]["samples"]:
            raise EvidenceError(
                "server status: Smithay driver retention rolled over after warm-up reset"
            )
        if field(server, "smithay_driver_call_count", "server status") <= 0:
            raise EvidenceError("server status: Smithay driver-call separation was not exercised")
        saturated_gate = scenario_gate(saturated)
        saturated_gate["background_actor_progress"] = background
        report = {
            "schema_version": 1,
            "provenance": {
                "harness": "brood-test-render-scheduling",
                "release_build": True,
                "nix_development_environment": True,
                "client_binary": str(args.client_binary.resolve()),
                "component": str(args.component.resolve()),
                "artifact_directory": str(args.artifact_directory.resolve()),
                "captures": captures,
            },
            "nested_behavior": behavior,
            "behavior_invariants": invariants,
            "deterministic_scheduler": deterministic,
            "unsaturated_60hz": scenario_gate(unsaturated),
            "saturated_60hz": saturated_gate,
            "direct_120hz": scenario_gate(direct, direct=True),
            "hot_paths": {
                "winit": winit,
                "smithay": smithay,
                "gpu_submission_wait": {
                    "retained_samples": len(gpu_wait_samples),
                    "calls_lifetime": gpu_wait_calls,
                    "p99_us": (int(gpu_wait_summary["p99_ns"]) + 999) // 1_000,
                    "maximum_us": (int(gpu_wait_summary["maximum_ns"]) + 999) // 1_000,
                },
            },
            "idle": idle,
        }
        args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    except (OSError, json.JSONDecodeError, EvidenceError, TypeError) as error:
        print(f"render_scheduling_assembly=failed error={error}", file=sys.stderr)
        return 1
    print(f"render_scheduling_assembly=complete path={args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
