"""Validate chronological publication/commit authority from the real host log."""
import json
import re
import subprocess
import sys
from pathlib import Path

log, out, status_snapshot = sys.argv[1:]
active = 1
published = []
commits = []
inflight_invalidations = 0
for line in Path(log).read_text(errors="replace").splitlines():
    if "THEME_RENDER_DEMAND" in line and "inflight=true" in line:
        inflight_invalidations += 1
    match = re.search(r"THEME_PUBLISHED revision=(\d+) name=(\S+)", line)
    if match:
        active = int(match[1])
        published.append({"revision": active, "name": match[2], "time_ns": int(re.search(r"time_ns=(\d+)", line)[1])})
    if "facet native pair committed" in line:
        match = re.search(r"actor=(\S+).*snapshot=(\d+) theme_revision=(\d+)", line)
        assert match, line
        assert int(match[3]) == active, f"superseded theme committed: {line} active={active}"
        commits.append({"actor": match[1], "snapshot": int(match[2]), "theme": active, "time_ns": int(re.search(r"time_ns=(\d+)", line)[1])})
assert inflight_invalidations > 0, "no in-flight theme work was exercised"
assert len(published) >= 5, published
assert published[-1]["name"] == "github_light_high_contrast", published[-1]
assert {"status-bar", "session-switcher", "picker", "terminal-ui"} <= {item["actor"] for item in commits}
# Export actual pixel differences, and require a substantial light/dark change.
result = subprocess.run(["magick", "compare", "-metric", "RMSE", f"{out}/theme-ashen.png", f"{out}/theme-github-light.png", "null:"], capture_output=True, text=True)
match = re.search(r"\(([0-9.eE+-]+)\)", result.stderr)
assert match and float(match[1]) > 0.2, result.stderr
publication_times = {item["revision"]: item["time_ns"] for item in published}
for item in commits:
    item["since_theme_publication_ms"] = round((item["time_ns"] - publication_times[item["theme"]]) / 1e6, 3)
Path(out, "theme-results.json").write_text(json.dumps({"publications": published, "commits": commits, "idle_status_application_revision": int(status_snapshot), "light_dark_rmse": float(match[1]), "superseded_commits": 0, "inflight_invalidations": inflight_invalidations}, indent=2)+"\n")
