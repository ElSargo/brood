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


def detect_divider(path: pathlib.Path, start_x: int, target_x: int) -> int | None:
    try:
        width, height, raw = load_rgb(path)
    except (OSError, subprocess.CalledProcessError, ValueError):
        return None
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
            detected = detect_divider(
                args.root / sample["file"], int(sample["start_x"]), int(sample["target_x"])
            )
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
        final_error = abs(int(run["final_x"]) - int(run["target_x"]))
        alive = bool(run["client_alive"])
        correctness_score = (
            5.0 * alive
            + 5.0 * (final_error <= 3)
            + 5.0 * (not stale_final_reuse and not fatal_resource)
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
        run_metrics.append(
            {
                **run,
                "motion_events": motion_events,
                "transactions": transactions,
                "supersessions": supersessions,
                "stale_actor_frames": stale_frames,
                "stale_region_responses": stale_regions,
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
        "schema": 1,
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
        f"{run['final_error_px']} | {'yes' if run['client_alive'] else 'no'} |"
        for run in run_metrics
    )
    failures = "\n".join(f"- {failure}" for failure in hard_failures) or "- None"
    args.report.write_text(
        f"""# Cua divider resize grade

Overall: **{overall:.2f}/100 ({grade_letter(overall)})** — {'PASS' if passed else 'FAIL'}

| Run | Score | Motions | Transactions | Superseded | Tracking RMSE | Final error px | Survived |
|---:|---:|---:|---:|---:|---:|---:|:---:|
{summary_rows}

## Hard failures

{failures}

The grade uses real native Wayland input from Cua. Fifty points measure visible divider tracking from captured frames, twenty penalize wasted/superseded synchronized transactions, fifteen require survival and correct final content/geometry, and fifteen measure release-to-completion latency. The overall score is the worst run so intermittent crashes cannot be averaged away.
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
