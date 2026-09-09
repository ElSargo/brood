#!/usr/bin/env python3
"""Build contact sheets after the native drag workloads finish."""
import json
from pathlib import Path
import subprocess
import sys

root = Path(sys.argv[1])
for lane in ("plugins", "resize-proof", "large-diff", "large-diff-fractional"):
    output = root / lane
    report = output / "drag-results.json"
    if not report.exists():
        continue
    for result in json.loads(report.read_text()):
        samples = result.get("samples")
        if not samples:
            continue
        pictures = [
            result["before"],
            samples[len(samples) // 3]["file"],
            samples[2 * len(samples) // 3]["file"],
            result["after"],
        ]
        command = ["magick", "montage"]
        for label, picture in zip(("before", "during 1", "during 2", "after"), pictures):
            command += ["-label", f"{result['case']} / {label}", picture]
        command += [
            "-font", "DejaVu-Sans", "-pointsize", "14",
            "-tile", "4x1", "-geometry", "384x216+4+4",
            "-background", "#eeeeee",
            str(output / (result["case"] + "-contact.png")),
        ]
        subprocess.run(command, check=True)
