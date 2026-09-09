#!/usr/bin/env python3
"""Deterministic brood-agent CLI fixture for the real coding-agent worker."""
import json
from pathlib import Path
import subprocess
import sys

PROMPT = "Create .brood/image-proof.txt with red green and .brood/agent-actor-proof.txt with actor-ui-worker-ok"
root = Path.cwd().resolve()
request = json.load(sys.stdin)
assert request.get("agent") == "coding-agent-1", request.get("agent")
assert request.get("prompt") == PROMPT, request.get("prompt")
images = request.get("images")
assert isinstance(images, list) and len(images) == 2, images
colors = set()
for image in images:
    path = Path(image["path"])
    resolved = (root / path).resolve() if not path.is_absolute() else path.resolve()
    resolved.relative_to(root)
    assert resolved.is_file() and resolved.read_bytes()[:8] == b"\x89PNG\r\n\x1a\n", resolved
    histogram = subprocess.check_output(
        ["magick", str(resolved), "-format", "%c", "histogram:info:-"], text=True
    )
    unique_colors = subprocess.check_output(["magick", str(resolved), "-format", "%k", "info:"], text=True)
    assert unique_colors == "1", (resolved, unique_colors)
    if "#FF0000" in histogram.upper():
        colors.add("red")
    if "#00FF00" in histogram.upper():
        colors.add("green")
assert colors == {"red", "green"}, colors
proof = root / ".brood"
proof.mkdir(exist_ok=True)
(proof / "image-proof.txt").write_text("red green\n", encoding="utf-8")
(proof / "agent-actor-proof.txt").write_text("actor-ui-worker-ok\n", encoding="utf-8")
assistant = "Validated the red and green PNG attachments and wrote both requested proof files."
for event in [
    {"type": "status", "text": "Starting Brood agent · model=default · reasoning=medium"},
    {"type": "assistant", "text": assistant},
    {"type": "status", "text": "Turn complete"},
    {"type": "result", "history": [{"user": PROMPT, "assistant": assistant}]},
]:
    print(json.dumps(event), flush=True)
