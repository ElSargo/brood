#!/usr/bin/env python3
"""Grade a real Cua-driven Brood session-switcher/terminal divider drag."""

from __future__ import annotations

import argparse
import json
import math
import pathlib
import re
import statistics
import subprocess
import sys


DIVIDER_LINEAR = {
    "normal": (0.48, 0.54, 0.64),
    "hover": (0.35, 0.66, 0.95),
    "active": (0.20, 0.78, 1.00),
}


def srgb(linear: float) -> int:
    if linear <= 0.0031308:
        return round(linear * 12.92 * 255)
    return round((1.055 * linear ** (1 / 2.4) - 0.055) * 255)


PALETTE = [
    color
    for source in DIVIDER_LINEAR.values()
    for color in (
        tuple(round(channel * 255) for channel in source),
        tuple(srgb(channel) for channel in source),
    )
]


def load_rgb(path: pathlib.Path) -> tuple[int, int, bytes]:
    dimensions = subprocess.check_output(
        ["magick", str(path), "-format", "%w %h", "info:"], text=True
    ).split()
    width, height = map(int, dimensions)
    raw = subprocess.check_output(
        ["magick", str(path), "-alpha", "off", "-depth", "8", "rgb:-"]
    )
    if len(raw) != width * height * 3:
        raise ValueError(f"invalid RGB payload for {path}")
    return width, height, raw


def is_divider(red: int, green: int, blue: int) -> bool:
    return min(
        (red - target[0]) ** 2
        + (green - target[1]) ** 2
        + (blue - target[2]) ** 2
        for target in PALETTE
    ) <= 30**2


def detect_divider(
    width: int, height: int, raw: bytes, start_x: int, target_x: int
) -> int | None:
    left = max(0, min(start_x, target_x) - 32)
    right = min(width - 1, max(start_x, target_x) + 32)
    row_start = round(height * 0.05)
    row_end = round(height * 0.92)
    scores: list[tuple[float, int]] = []
    for x in range(left, right + 1):
        matches = 0
        for y in range(row_start, row_end):
            offset = (y * width + x) * 3
            matches += is_divider(raw[offset], raw[offset + 1], raw[offset + 2])
        scores.append((matches / max(1, row_end - row_start), x))
    if not scores:
        return None
    score, x = max(scores)
    return x if score >= 0.45 else None


def clear_slice_columns(
    width: int, height: int, raw: bytes, start_x: int, target_x: int
) -> list[int]:
    clear = tuple(srgb(channel) for channel in (0.04, 0.05, 0.07))
    row_start = round(height * 0.05)
    row_end = round(height * 0.92)
    threshold = math.ceil((row_end - row_start) * 0.95)
    candidates = set(range(0, min(8, width)))
    candidates.update(range(max(0, width - 8), width))
    candidates.update(
        range(max(0, min(start_x, target_x) - 32), min(width, max(start_x, target_x) + 33))
    )
    columns = []
    for x in sorted(candidates):
        matches = 0
        for y in range(row_start, row_end):
            offset = (y * width + x) * 3
            matches += tuple(raw[offset : offset + 3]) == clear
        if matches >= threshold:
            columns.append(x)
    return columns


def clamp(value: float, low: float = 0.0, high: float = 1.0) -> float:
    return min(high, max(low, value))


def grade_letter(score: float) -> str:
    if score >= 95:
        return "A+"
    if score >= 90:
        return "A"
    if score >= 80:
        return "B"
    if score >= 70:
        return "C"
    if score >= 60:
        return "D"
    return "F"


def nearest_rank(values: list[float], percentile: float) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    rank = max(1, math.ceil(percentile * len(ordered)))
    return ordered[min(len(ordered), rank) - 1]


def distribution(values: list[float]) -> dict[str, float | int | None]:
    return {
        "samples": len(values),
        "p50_us": nearest_rank(values, 0.50),
        "p95_us": nearest_rank(values, 0.95),
        "p99_us": nearest_rank(values, 0.99),
        "max_us": max(values, default=None),
    }


def analyze_resize_pipeline(segment: list[str]) -> dict[str, object]:
    events: dict[tuple[str, int], int] = {}
    pane_actors: dict[int, str] = {}
    for line in segment:
        match = re.search(
            r"resize-pipeline-event time_ns=(\d+) actor=(\S+) generation=\d+ "
            r"pane=(\d+) event_watermark=(\d+)",
            line,
        )
        if match and match.group(2) in ("session-switcher", "terminal-ui"):
            actor = match.group(2)
            pane = int(match.group(3))
            watermark = int(match.group(4))
            events[(actor, watermark)] = int(match.group(1))
            pane_actors[pane] = actor

    frames: dict[int, tuple[str, int]] = {}
    for line in segment:
        match = re.search(
            r"resize-pipeline-frame token=(\d+) actor=(\S+) generation=\d+ "
            r"sequence=(\d+)",
            line,
        )
        if match:
            frames[int(match.group(1))] = (match.group(2), int(match.group(3)))

    preparation = {"queue": [], "run": [], "completion_hop": []}
    preparation_by_actor = {
        actor: {"queue": [], "run": [], "completion_hop": []}
        for actor in ("session-switcher", "terminal-ui")
    }
    for line in segment:
        match = re.search(
            r"resize-pipeline-prepare pane=(\d+) sequence=(\d+) width=\d+ height=\d+ "
            r"queue_us=(\d+) run_us=(\d+) completion_hop_us=(\d+)",
            line,
        )
        if not match or int(match.group(1)) not in pane_actors:
            continue
        actor = pane_actors[int(match.group(1))]
        for stage, group in zip(
            ("queue", "run", "completion_hop"), match.groups()[2:5]
        ):
            preparation[stage].append(float(group))
            preparation_by_actor[actor][stage].append(float(group))

    token_pattern = re.compile(
        r"resize-pipeline-token token=(\d+) actor=(\S+) generation=\d+ "
        r"event_watermark=(\d+) target_ns=(\d+) deadline_ns=(\d+) "
        r"dispatched_ns=(\d+) guest_finished_ns=(\d+) accepted_ns=(\d+) "
        r"graphics_ready_ns=(\d+) composition_ns=(\d+) gpu_submission_ns=(\d+) "
        r"presentation_ns=(\d+) settlement=(.*?) deadline_met="
    )
    joined: dict[tuple[str, int], dict[str, object]] = {}
    presented_without_event = 0
    for line in segment:
        match = token_pattern.search(line)
        if not match or not match.group(13).startswith("Presented"):
            continue
        token = int(match.group(1))
        actor = match.group(2)
        watermark = int(match.group(3))
        event_ns = events.get((actor, watermark))
        if event_ns is None:
            presented_without_event += 1
            continue
        target_ns, deadline_ns, dispatched_ns, guest_finished_ns, accepted_ns, graphics_ready_ns, composition_ns, gpu_submission_ns, presentation_ns = map(
            int, match.groups()[3:12]
        )
        sample = {
            "token": token,
            "actor": actor,
            "event_watermark": watermark,
            "sequence": frames.get(token, (actor, 0))[1],
            "event_to_dispatch_us": (dispatched_ns - event_ns) / 1_000,
            "guest_draw_us": (guest_finished_ns - dispatched_ns) / 1_000,
            "host_validation_us": (accepted_ns - guest_finished_ns) / 1_000,
            "accepted_to_graphics_ready_us": (graphics_ready_ns - accepted_ns) / 1_000,
            "graphics_ready_to_composition_us": (composition_ns - graphics_ready_ns) / 1_000,
            "composition_to_submission_us": (gpu_submission_ns - composition_ns) / 1_000,
            "submission_to_present_us": (presentation_ns - gpu_submission_ns) / 1_000,
            "event_to_present_us": (presentation_ns - event_ns) / 1_000,
            "present_vs_target_us": (presentation_ns - target_ns) / 1_000,
            "present_vs_deadline_us": (presentation_ns - deadline_ns) / 1_000,
        }
        # A subscription may draw the same admitted state again. Preserve the
        # first presentation causally attributable to each resize watermark.
        joined.setdefault((actor, watermark), sample)

    stage_names = (
        "event_to_dispatch_us",
        "guest_draw_us",
        "host_validation_us",
        "accepted_to_graphics_ready_us",
        "graphics_ready_to_composition_us",
        "composition_to_submission_us",
        "submission_to_present_us",
        "event_to_present_us",
        "present_vs_target_us",
    )
    samples = list(joined.values())
    by_actor = {}
    for actor in ("session-switcher", "terminal-ui"):
        actor_samples = [sample for sample in samples if sample["actor"] == actor]
        by_actor[actor] = {
            "samples": len(actor_samples),
            "stages": {
                stage: distribution([float(sample[stage]) for sample in actor_samples])
                for stage in stage_names
            },
            "preparation": {
                stage: distribution(values)
                for stage, values in preparation_by_actor[actor].items()
            },
        }
    return {
        "resize_events": len(events),
        "joined_presentations": len(samples),
        "presented_tokens_without_segment_event": presented_without_event,
        "preparation": {
            stage: distribution(values) for stage, values in preparation.items()
        },
        "stages": {
            stage: distribution([float(sample[stage]) for sample in samples])
            for stage in stage_names
        },
        "actors": by_actor,
        "worst_presentations": sorted(
            samples, key=lambda sample: float(sample["event_to_present_us"]), reverse=True
        )[:5],
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", required=True, type=pathlib.Path)
    parser.add_argument("--client-log", required=True, type=pathlib.Path)
    parser.add_argument("--runs", required=True, type=pathlib.Path)
    parser.add_argument("--samples", required=True, type=pathlib.Path)
    parser.add_argument("--metrics", required=True, type=pathlib.Path)
    parser.add_argument("--report", required=True, type=pathlib.Path)
    parser.add_argument("--minimum-score", type=float, default=90.0)
    parser.add_argument("--allow-fail", action="store_true")
    args = parser.parse_args()

    lines = args.client_log.read_text(errors="replace").splitlines()
    runs = [json.loads(line) for line in args.runs.read_text().splitlines() if line]
    samples = [json.loads(line) for line in args.samples.read_text().splitlines() if line]
    switcher_match = next(
        (
            re.search(r"started presentation WASM actor id=session-switcher .* pane=(\d+)", line)
            for line in lines
            if "started presentation WASM actor id=session-switcher" in line
        ),
        None,
    )
    switcher_pane = int(switcher_match.group(1)) if switcher_match else None

    run_metrics = []
    hard_failures: list[str] = []
    for run in runs:
        run_number = int(run["run"])
        segment = lines[max(0, int(run["start_line"]) - 1) : int(run["end_line"])]
        motion_events = sum(
            "pointer-interaction event" in line and "phase=Motion" in line
            for line in segment
        )
        motion_timestamps = [
            int(match.group(1))
            for line in segment
            if "pointer-interaction event" in line and "phase=Motion" in line
            if (match := re.search(r"wall_ns=(\d+)", line))
        ]
        transactions = sum(
            "accepted synchronized layout actor=session-switcher" in line
            and " interaction=0 " not in line
            for line in segment
        )
        supersessions = sum(
            "synchronized interaction superseded" in line for line in segment
        )
        stale_frames = sum(
            "discarded stale tagged plugin actor frame" in line for line in segment
        )
        stale_regions = sum(
            "layout-region response-rejected" in line and "StaleEpoch" in line
            for line in segment
        )
        element_samples = []
        for line in segment:
            if "resize-element-present name=new-session-button" not in line:
                continue
            header = re.search(
                r"present=(\d+) time_ns=(\d+) interval_us=(\d+) pane=(\d+) "
                r"sequence=(\d+) source_extent=(\d+)x(\d+) "
                r"target_rect=(-?\d+):(-?\d+):(\d+):(\d+)",
                line,
            )
            drawn = re.search(
                r"drawn_rect=(-?[0-9.]+):(-?[0-9.]+):(-?[0-9.]+):(-?[0-9.]+)",
                line,
            )
            expected_rect = re.search(
                r"expected_rect=(-?[0-9.]+):(-?[0-9.]+):(-?[0-9.]+):(-?[0-9.]+)",
                line,
            )
            error = re.search(
                r"error=(-?[0-9.]+):(-?[0-9.]+):(-?[0-9.]+):(-?[0-9.]+)",
                line,
            )
            if not header or not drawn or not expected_rect or not error:
                continue
            element_samples.append(
                {
                    "present": int(header.group(1)),
                    "time_ns": int(header.group(2)),
                    "interval_us": int(header.group(3)),
                    "pane": int(header.group(4)),
                    "sequence": int(header.group(5)),
                    "source_extent": [int(header.group(6)), int(header.group(7))],
                    "target_rect": [int(value) for value in header.groups()[7:11]],
                    "drawn_rect": [float(value) for value in drawn.groups()],
                    "expected_rect": [float(value) for value in expected_rect.groups()],
                    "error": [float(value) for value in error.groups()],
                }
            )
        element_intervals = [
            float(sample["interval_us"])
            for sample in element_samples[1:]
            if sample["interval_us"] > 0
        ]
        max_element_position_error = max(
            (max(abs(sample["error"][0]), abs(sample["error"][1])) for sample in element_samples),
            default=0.0,
        )
        max_element_size_error = max(
            (max(abs(sample["error"][2]), abs(sample["error"][3])) for sample in element_samples),
            default=0.0,
        )
        unlocked_element_samples = sum(
            max(abs(value) for value in sample["error"]) > 0.25
            for sample in element_samples
        )
        resize_pipeline = analyze_resize_pipeline(segment)
        render_geometry = [
            tuple(map(int, match.groups()))
            for line in segment
            if "render-geometry-invariant" in line
            if (
                match := re.search(
                    r"panes=(\d+) verified=(\d+) missing_surfaces=(\d+) "
                    r"missing_painted_extents=(\d+) area_mismatches=(\d+) "
                    r"geometry_mismatches=(\d+)",
                    line,
                )
            )
        ]
        render_geometry_samples = len(render_geometry)
        render_geometry_deferred = sum(
            "render-geometry-deferred" in line for line in segment
        )
        render_geometry_panes = sum(sample[0] for sample in render_geometry)
        render_geometry_verified = sum(sample[1] for sample in render_geometry)
        render_missing_surfaces = sum(sample[2] for sample in render_geometry)
        render_missing_painted_extents = sum(sample[3] for sample in render_geometry)
        render_area_mismatches = sum(sample[4] for sample in render_geometry)
        render_geometry_mismatches = sum(sample[5] for sample in render_geometry)
        render_geometry_ok = bool(render_geometry) and not any(
            (
                render_missing_surfaces,
                render_missing_painted_extents,
                render_area_mismatches,
                render_geometry_mismatches,
            )
        )
        projection_frames = []
        for line in segment:
            if "render-content-projection" not in line:
                continue
            header = re.search(
                r"panes=(\d+) projected_panes=(\d+) max_width_delta=(\d+) "
                r"max_height_delta=(\d+) max_scale_error_ppm=(\d+) details=\[(.*)\]",
                line,
            )
            if not header:
                continue
            panes = []
            for detail in filter(None, header.group(6).split(",")):
                fields = detail.split(":")
                if len(fields) != 6 or fields[2] == "missing":
                    continue
                panes.append(tuple(map(int, fields)))
            projection_frames.append(
                (tuple(map(int, header.groups()[:5])), panes)
            )
        projected_frame_count = sum(
            header[1] > 0 for header, _panes in projection_frames
        )
        projection_pane_samples = sum(
            header[0] for header, _panes in projection_frames
        )
        projected_pane_samples = sum(
            header[1] for header, _panes in projection_frames
        )
        max_projection_width_delta = max(
            (header[2] for header, _panes in projection_frames), default=0
        )
        max_projection_scale_error_ppm = max(
            (header[4] for header, _panes in projection_frames), default=0
        )
        previous_source: dict[int, tuple[int, int]] = {}
        projected_source_switches = 0
        native_source_updates = 0
        regressive_source_updates = 0
        native_update_frame_panes: list[set[int]] = []
        for _header, panes in projection_frames:
            changed_panes: set[int] = set()
            for pane, _sequence, source_width, source_height, target_width, target_height in panes:
                source = (source_width, source_height)
                target = (target_width, target_height)
                if pane in previous_source and previous_source[pane] != source:
                    native_source_updates += 1
                    changed_panes.add(pane)
                    previous_error = sum(
                        abs(value - target_value)
                        for value, target_value in zip(previous_source[pane], target)
                    )
                    current_error = sum(
                        abs(value - target_value)
                        for value, target_value in zip(source, target)
                    )
                    if source != target:
                        projected_source_switches += 1
                    if current_error > previous_error:
                        regressive_source_updates += 1
                previous_source[pane] = source
            native_update_frame_panes.append(changed_panes)
        moving_source_panes = set().union(*native_update_frame_panes)
        native_update_frames = sum(bool(panes) for panes in native_update_frame_panes)
        skewed_native_update_frames = sum(
            bool(panes) and panes != moving_source_panes
            for panes in native_update_frame_panes
        )
        fatal_resource = any(
            "lost a referenced resource" in line for line in segment
        )
        promoted = [
            line
            for line in segment
            if "resize-present" in line and "phase=promoted" in line
        ]
        final_epoch = None
        if promoted:
            match = re.search(r"epoch=(\d+)", promoted[-1])
            final_epoch = int(match.group(1)) if match else None
        stale_final_reuse = bool(
            final_epoch is not None
            and switcher_pane is not None
            and any(
                f"accepted plugin content reuse pane={switcher_pane} resize_epoch={final_epoch}"
                in line
                for line in segment
            )
            and int(run["start_x"]) != int(run["target_x"])
        )
        completion_us = None
        for line in reversed(segment):
            match = re.search(
                r"completed synchronized layout actor=session-switcher .* duration_us=(\d+)",
                line,
            )
            if match:
                completion_us = int(match.group(1))
                break

        observed = []
        for sample in (item for item in samples if int(item["run"]) == run_number):
            try:
                width, height, raw = load_rgb(args.root / sample["file"])
                detected = detect_divider(
                    width,
                    height,
                    raw,
                    int(sample["start_x"]),
                    int(sample["target_x"]),
                )
                clear_slices = clear_slice_columns(
                    width,
                    height,
                    raw,
                    int(sample["start_x"]),
                    int(sample["target_x"]),
                )
            except (OSError, subprocess.CalledProcessError, ValueError):
                detected = None
                clear_slices = []
            elapsed_ms = (int(sample["captured_ns"]) - int(sample["drag_started_ns"])) / 1e6
            captured_ns = int(sample["captured_ns"])
            if motion_timestamps:
                expected = sum(timestamp <= captured_ns for timestamp in motion_timestamps) / len(
                    motion_timestamps
                )
                during_gesture = motion_timestamps[0] <= captured_ns <= motion_timestamps[-1]
            else:
                # Compatibility for retained artifacts from before native
                # input timestamps were recorded.
                expected = clamp((elapsed_ms - 50.0) / float(sample["duration_ms"]))
                during_gesture = True
            if detected is None:
                actual = None
            else:
                distance = int(sample["target_x"]) - int(sample["start_x"])
                actual = clamp((detected - int(sample["start_x"])) / distance) if distance else 1.0
            observed.append(
                {
                    "sample": int(sample["sample"]),
                    "elapsed_ms": round(elapsed_ms, 3),
                    "expected_progress": round(expected, 4),
                    "detected_x": detected,
                    "actual_progress": None if actual is None else round(actual, 4),
                    "during_gesture": during_gesture,
                    "clear_slice_columns": clear_slices,
                    "file": sample["file"],
                }
            )

        valid = [
            item
            for item in observed
            if item["actual_progress"] is not None and item["during_gesture"]
        ]
        if valid:
            rmse = math.sqrt(
                statistics.fmean(
                    (item["actual_progress"] - item["expected_progress"]) ** 2
                    for item in valid
                )
            )
            unique_ratio = len({item["detected_x"] for item in valid}) / len(valid)
            direction = 1 if int(run["target_x"]) > int(run["start_x"]) else -1
            movements = [
                (second["detected_x"] - first["detected_x"]) * direction
                for first, second in zip(valid, valid[1:])
            ]
            movement_ratio = (
                sum(delta > 0 for delta in movements) / len(movements) if movements else 0.0
            )
            # One native motion step and one physical divider pixel are below
            # the observation precision of an asynchronous compositor capture.
            tracking_error = max(0.0, rmse - max(1.0 / max(1, motion_events), 1.0 / 196.0))
            visual_score = 50.0 * (
                0.55 * clamp(1.0 - tracking_error / 0.35)
                + 0.20 * unique_ratio
                + 0.25 * movement_ratio
            )
        else:
            rmse = 1.0
            unique_ratio = 0.0
            movement_ratio = 0.0
            visual_score = 0.0

        protocol_efficiency = clamp(1.0 - supersessions / max(1, motion_events))
        protocol_score = 20.0 * protocol_efficiency
        clear_slice_frames = sum(bool(item["clear_slice_columns"]) for item in observed)
        max_clear_slice_columns = max(
            (len(item["clear_slice_columns"]) for item in observed), default=0
        )
        final_error = abs(int(run["final_x"]) - int(run["target_x"]))
        alive = bool(run["client_alive"])
        correctness_score = (
            5.0 * alive
            + 5.0 * (final_error <= 3)
            + 5.0
            * (
                not stale_final_reuse
                and not fatal_resource
                and render_geometry_ok
                and bool(projection_frames)
                and regressive_source_updates == 0
                and skewed_native_update_frames == 0
                and clear_slice_frames == 0
            )
        )
        if completion_us is None:
            latency_score = 0.0
        else:
            completion_ms = completion_us / 1000.0
            latency_score = 15.0 * clamp(1.0 - max(0.0, completion_ms - 50.0) / 100.0)

        score = visual_score + protocol_score + correctness_score + latency_score
        if not alive or fatal_resource or int(run["drag_result"]) != 0:
            score = 0.0
        if not alive:
            hard_failures.append(f"run {run_number}: client exited during the Cua drag")
        if fatal_resource:
            hard_failures.append(f"run {run_number}: accepted frame lost a referenced resource")
        if final_error > 3:
            hard_failures.append(
                f"run {run_number}: final divider error is {final_error}px"
            )
        if stale_final_reuse:
            hard_failures.append(
                f"run {run_number}: final epoch reused stale session-switcher content"
            )
        if not render_geometry:
            hard_failures.append(
                f"run {run_number}: no render-time surface/pane/layout geometry samples"
            )
        elif not render_geometry_ok:
            hard_failures.append(
                f"run {run_number}: render-time geometry invariant failed "
                f"(missing surfaces {render_missing_surfaces}, area mismatches "
                f"{render_area_mismatches}, missing painted extents "
                f"{render_missing_painted_extents}, geometry mismatches "
                f"{render_geometry_mismatches})"
            )
        if not projection_frames:
            hard_failures.append(
                f"run {run_number}: no render-time native-content projection samples"
            )
        elif regressive_source_updates:
            hard_failures.append(
                f"run {run_number}: {regressive_source_updates} late native relayouts "
                "moved content farther from the current target"
            )
        if skewed_native_update_frames:
            hard_failures.append(
                f"run {run_number}: {skewed_native_update_frames} presented native "
                "update frames advanced only part of the resize cohort"
            )
        if clear_slice_frames:
            hard_failures.append(
                f"run {run_number}: renderer-clear background slices appeared in "
                f"{clear_slice_frames} captured frames (maximum width "
                f"{max_clear_slice_columns}px)"
            )
        if not element_samples:
            hard_failures.append(
                f"run {run_number}: no presentation-rate new-session-button geometry samples"
            )
        elif unlocked_element_samples:
            hard_failures.append(
                f"run {run_number}: new-session-button moved or stretched in "
                f"{unlocked_element_samples}/{len(element_samples)} presented frames "
                f"(maximum position error {max_element_position_error:.3f}px, "
                f"maximum size error {max_element_size_error:.3f}px)"
            )
        run_metrics.append(
            {
                **run,
                "motion_events": motion_events,
                "transactions": transactions,
                "supersessions": supersessions,
                "stale_actor_frames": stale_frames,
                "stale_region_responses": stale_regions,
                "render_geometry_samples": render_geometry_samples,
                "render_geometry_deferred": render_geometry_deferred,
                "render_geometry_panes": render_geometry_panes,
                "render_geometry_verified": render_geometry_verified,
                "render_missing_surfaces": render_missing_surfaces,
                "render_missing_painted_extents": render_missing_painted_extents,
                "render_area_mismatches": render_area_mismatches,
                "render_geometry_mismatches": render_geometry_mismatches,
                "render_geometry_ok": render_geometry_ok,
                "projection_frames": len(projection_frames),
                "projected_frames": projected_frame_count,
                "projected_frame_ratio": round(
                    projected_frame_count / max(1, len(projection_frames)), 5
                ),
                "projection_pane_samples": projection_pane_samples,
                "projected_pane_samples": projected_pane_samples,
                "projected_pane_ratio": round(
                    projected_pane_samples / max(1, projection_pane_samples), 5
                ),
                "projected_source_switches": projected_source_switches,
                "native_source_updates": native_source_updates,
                "regressive_source_updates": regressive_source_updates,
                "native_update_frames": native_update_frames,
                "skewed_native_update_frames": skewed_native_update_frames,
                "skewed_native_update_frame_ratio": round(
                    skewed_native_update_frames / max(1, native_update_frames), 5
                ),
                "max_projection_width_delta_px": max_projection_width_delta,
                "max_projection_scale_error_pct": round(
                    max_projection_scale_error_ppm / 10_000.0, 3
                ),
                "clear_slice_frames": clear_slice_frames,
                "max_clear_slice_columns": max_clear_slice_columns,
                "resize_element": {
                    "name": "new-session-button",
                    "samples": len(element_samples),
                    "unlocked_samples": unlocked_element_samples,
                    "max_position_error_px": round(max_element_position_error, 5),
                    "max_size_error_px": round(max_element_size_error, 5),
                    "present_interval_us_p50": nearest_rank(element_intervals, 0.50),
                    "present_interval_us_p95": nearest_rank(element_intervals, 0.95),
                    "present_interval_us_p99": nearest_rank(element_intervals, 0.99),
                    "present_interval_us_max": max(element_intervals, default=None),
                    "frames": element_samples,
                },
                "resize_pipeline": resize_pipeline,
                "fatal_resource_loss": fatal_resource,
                "stale_final_reuse": stale_final_reuse,
                "final_error_px": final_error,
                "completion_us": completion_us,
                "tracking_rmse": round(rmse, 5),
                "unique_position_ratio": round(unique_ratio, 5),
                "moving_sample_ratio": round(movement_ratio, 5),
                "scores": {
                    "visual_tracking": round(visual_score, 2),
                    "protocol_efficiency": round(protocol_score, 2),
                    "correctness": round(correctness_score, 2),
                    "release_latency": round(latency_score, 2),
                    "total": round(score, 2),
                },
                "samples": observed,
            }
        )

    overall = min((item["scores"]["total"] for item in run_metrics), default=0.0)
    passed = bool(run_metrics) and overall >= args.minimum_score and not hard_failures
    metrics = {
        "schema": 5,
        "workload": "production session-switcher + terminal, native Wayland Cua drag",
        "score": round(overall, 2),
        "grade": grade_letter(overall),
        "minimum_score": args.minimum_score,
        "passed": passed,
        "hard_failures": hard_failures,
        "rubric": {
            "visual_tracking": 50,
            "protocol_efficiency": 20,
            "correctness": 15,
            "release_latency": 15,
        },
        "runs": run_metrics,
    }
    args.metrics.write_text(json.dumps(metrics, indent=2, sort_keys=True) + "\n")

    summary_rows = "\n".join(
        f"| {run['run']} | {run['scores']['total']:.2f} | {run['motion_events']} | "
        f"{run['transactions']} | {run['supersessions']} | {run['tracking_rmse']:.3f} | "
        f"{run['projected_frames']}/{run['projection_frames']} | "
        f"{run['native_source_updates']} | {run['regressive_source_updates']} | "
        f"{run['skewed_native_update_frames']} | {run['final_error_px']} | "
        f"{run['resize_element']['unlocked_samples']}/{run['resize_element']['samples']} | "
        f"{run['resize_element']['max_position_error_px']:.3f} | "
        f"{run['resize_element']['max_size_error_px']:.3f} | "
        f"{'yes' if run['render_geometry_ok'] else 'no'} | "
        f"{'yes' if run['client_alive'] else 'no'} |"
        for run in run_metrics
    )
    pipeline_stage_labels = (
        ("event_to_dispatch_us", "Resize event → actor dispatch"),
        ("guest_draw_us", "Guest draw"),
        ("host_validation_us", "Host validation/lowering validation"),
        ("accepted_to_graphics_ready_us", "Accepted → graphics ready"),
        ("graphics_ready_to_composition_us", "Graphics ready → composition"),
        ("composition_to_submission_us", "Composition → GPU submission"),
        ("submission_to_present_us", "Submission → present call returned"),
        ("event_to_present_us", "Resize event → present"),
    )
    pipeline_rows = "\n".join(
        f"| {run['run']} | {label} | {stage['samples']} | "
        f"{stage['p50_us']:.1f} | {stage['p95_us']:.1f} | "
        f"{stage['p99_us']:.1f} | {stage['max_us']:.1f} |"
        for run in run_metrics
        for key, label in pipeline_stage_labels
        if (stage := run["resize_pipeline"]["stages"][key])["samples"]
    )
    preparation_rows = "\n".join(
        f"| {run['run']} | {label} | {stage['samples']} | "
        f"{stage['p50_us']:.1f} | {stage['p95_us']:.1f} | "
        f"{stage['p99_us']:.1f} | {stage['max_us']:.1f} |"
        for run in run_metrics
        for key, label in (
            ("queue", "Preparation queue"),
            ("run", "Preparation worker"),
            ("completion_hop", "Worker completion → event loop"),
        )
        if (stage := run["resize_pipeline"]["preparation"][key])["samples"]
    )
    failures = "\n".join(f"- {failure}" for failure in hard_failures) or "- None"
    args.report.write_text(
        f"""# Cua divider resize grade

Overall: **{overall:.2f}/100 ({grade_letter(overall)})** — {'PASS' if passed else 'FAIL'}

| Run | Score | Motions | Transactions | Superseded | Tracking RMSE | Projected frames | Native updates | Regressive updates | Skewed update frames | Final error px | Unlocked element frames | Max position error px | Max size error px | Render geometry | Survived |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|:---:|:---:|
{summary_rows}

## Hard failures

{failures}

## Resize pipeline timing

All values are microseconds. Each resize event is joined by actor and exact
event watermark to the first presented render token covering that state.

| Run | Stage | Samples | p50 | p95 | p99 | Max |
|---:|---|---:|---:|---:|---:|---:|
{pipeline_rows}

Preparation is asynchronous and is joined to its exact pane/scene sequence;
its sub-stages overlap the accepted-to-graphics-ready interval above.

| Run | Preparation sub-stage | Samples | p50 | p95 | p99 | Max |
|---:|---|---:|---:|---:|---:|---:|
{preparation_rows}

The grade uses real native Wayland input from Cua. Projected frames report how often native pane content differs from its current target extent. Native updates count genuine source relayouts; a regressive update is a late relayout that moves source geometry farther from the current target. Skewed update frames show that only part of the live-resize pane cohort changed in one presented frame. The new-session-button oracle projects its source-space rectangle through the exact pane stretch used for every presented frame and compares that result with the rectangle from current target geometry. Any regressive or skewed update, or any element position/size error above 0.25 px, is a hard failure. Fifty points measure visible divider tracking from captured frames, twenty penalize wasted/superseded synchronized transactions, fifteen require survival, correct final content/geometry, exact render-time painted-surface/pane/layout area and geometry, and no renderer-clear slices in captured pixels, and fifteen measure release-to-completion latency. The overall score is the worst run so intermittent failures cannot be averaged away.
"""
    )
    print(f"Cua divider resize: {overall:.2f}/100 ({grade_letter(overall)}) {'PASS' if passed else 'FAIL'}")
    for failure in hard_failures:
        print(f"- {failure}")
    if passed or args.allow_fail:
        return 0
    return 1


if __name__ == "__main__":
    sys.exit(main())
