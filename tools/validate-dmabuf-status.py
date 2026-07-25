#!/usr/bin/env python3
"""Validate Brood dmabuf status telemetry against a client WAYLAND_DEBUG trace."""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import sys
from typing import Any


DMABUF_GLOBAL_OR_BIND_RE = re.compile(
    r'(wl_registry@\d+\.global\([^)]*, "zwp_linux_dmabuf_v1"|'
    r'wl_registry@\d+\.bind\([^)]*, "zwp_linux_dmabuf_v1")'
)
SYNCOBJ_GLOBAL_OR_BIND_RE = re.compile(
    r'(wl_registry@\d+\.global\([^)]*, "wp_linux_drm_syncobj_manager_v1"|'
    r'wl_registry@\d+\.bind\([^)]*, "wp_linux_drm_syncobj_manager_v1")'
)
DMABUF_V3_DISABLED_REASONS = {
    "v3_no_render_node",
    "v3_egl_device_error",
    "v3_default_feedback_error",
}
EXPLICIT_SYNC_DISABLED_REASONS_WITH_DMABUF_V4 = {
    "drm_render_node_path_unavailable",
    "drm_render_node_open_failed",
    "drm_syncobj_eventfd_unsupported",
}
EXPLICIT_SYNC_DISABLED_REASONS_WITH_DMABUF_V3 = (
    DMABUF_V3_DISABLED_REASONS | EXPLICIT_SYNC_DISABLED_REASONS_WITH_DMABUF_V4
)


class SkipValidation(Exception):
    """Raised when the backend lacks dmabuf formats but the disabled path is coherent."""


def has_dmabuf_global_or_bind(trace: str) -> bool:
    return DMABUF_GLOBAL_OR_BIND_RE.search(trace) is not None


def has_syncobj_global_or_bind(trace: str) -> bool:
    return SYNCOBJ_GLOBAL_OR_BIND_RE.search(trace) is not None


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def require_int_field(data: dict[str, Any], key: str) -> int:
    value = data.get(key)
    require(
        isinstance(value, int) and not isinstance(value, bool),
        f"{key} should be an integer: {data}",
    )
    return value


def validate_disabled_path(data: dict[str, Any], trace: str) -> None:
    errors: list[str] = []
    if data.get("dmabuf") != "none":
        errors.append(f"dmabuf should be none when dmabuf_formats=0: {data.get('dmabuf')}")
    if data.get("dmabuf_disabled") != "no_dmabuf_texture_formats":
        errors.append(
            "dmabuf_disabled should be no_dmabuf_texture_formats when "
            f"dmabuf_formats=0: {data.get('dmabuf_disabled')}"
        )
    if data.get("explicit_sync") != "none":
        errors.append(
            f"explicit_sync should be none when dmabuf is disabled: {data.get('explicit_sync')}"
        )
    if data.get("explicit_sync_disabled") != "no_dmabuf_texture_formats":
        errors.append(
            "explicit_sync_disabled should be no_dmabuf_texture_formats when "
            f"dmabuf_formats=0: {data.get('explicit_sync_disabled')}"
        )
    for key in (
        "dmabuf_attempts",
        "dmabuf_accepted",
        "dmabuf_rejected",
        "dmabuf_explicit_sync_release_points",
        "dmabuf_explicit_sync_blockers",
        "dmabuf_implicit_blockers",
        "dmabuf_blocker_failures",
    ):
        if data.get(key) != 0:
            errors.append(f"{key} should be 0 when dmabuf is disabled: {data.get(key)}")
    if data.get("dmabuf_last_import") != "none":
        errors.append(f"dmabuf_last_import should be none: {data.get('dmabuf_last_import')}")
    if data.get("dmabuf_last_error") != "none":
        errors.append(f"dmabuf_last_error should be none: {data.get('dmabuf_last_error')}")
    if data.get("dmabuf_last_blocker_error") != "none":
        errors.append(
            f"dmabuf_last_blocker_error should be none: {data.get('dmabuf_last_blocker_error')}"
        )
    if has_dmabuf_global_or_bind(trace):
        errors.append(
            "client WAYLAND_DEBUG trace saw a zwp_linux_dmabuf registry global/bind "
            "despite dmabuf_formats=0"
        )
    if has_syncobj_global_or_bind(trace):
        errors.append(
            "client WAYLAND_DEBUG trace saw a wp_linux_drm_syncobj registry global/bind "
            "despite dmabuf_formats=0"
        )
    if errors:
        raise ValueError("disabled dmabuf path is inconsistent:\n" + "\n".join(errors))
    raise SkipValidation(
        "SKIP renderer reported no importable dmabuf formats; "
        f"dmabuf_disabled={data.get('dmabuf_disabled')} egl_wl_bind={data.get('egl_wl_bind')}"
    )


def validate_positive_path(data: dict[str, Any], trace: str, require_explicit_sync: bool) -> None:
    dmabuf = data.get("dmabuf")
    dmabuf_disabled = data.get("dmabuf_disabled")
    explicit_sync = data.get("explicit_sync")
    explicit_sync_disabled = data.get("explicit_sync_disabled")
    dmabuf_formats = require_int_field(data, "dmabuf_formats")
    windows = require_int_field(data, "windows")
    frames = require_int_field(data, "frames")
    dmabuf_attempts = require_int_field(data, "dmabuf_attempts")
    dmabuf_accepted = require_int_field(data, "dmabuf_accepted")
    dmabuf_rejected = require_int_field(data, "dmabuf_rejected")
    dmabuf_blocker_failures = require_int_field(data, "dmabuf_blocker_failures")
    dmabuf_explicit_sync_blockers = require_int_field(data, "dmabuf_explicit_sync_blockers")
    dmabuf_implicit_blockers = require_int_field(data, "dmabuf_implicit_blockers")

    require(dmabuf_formats > 0, f"positive dmabuf path should report formats: {data}")
    require(dmabuf in {"v3", "v4"}, f"expected advertised dmabuf v3/v4: {data}")
    if dmabuf == "v4":
        require(
            dmabuf_disabled == "none",
            f"dmabuf v4 should not report a disabled diagnostic reason: {data}",
        )
    else:
        require(
            dmabuf_disabled in DMABUF_V3_DISABLED_REASONS,
            f"dmabuf v3 should explain why v4/default feedback is unavailable: {data}",
        )
    require(
        explicit_sync in {"none", "v1"},
        f"unexpected explicit_sync value: {data}",
    )
    require(windows >= 1, f"dmabuf client did not map a window: {data}")
    require(frames >= 2, f"dmabuf client did not render frames: {data}")
    require(
        has_dmabuf_global_or_bind(trace),
        "client WAYLAND_DEBUG trace did not see/bind zwp_linux_dmabuf",
    )
    require(dmabuf_accepted >= 1, f"no client dmabuf imports were accepted: {data}")
    require(dmabuf_rejected == 0, f"client dmabuf imports were rejected: {data}")
    require(
        dmabuf_attempts >= dmabuf_accepted + dmabuf_rejected,
        f"dmabuf attempts must cover accepted imports: {data}",
    )
    require(
        data.get("dmabuf_last_import") not in {None, "none"},
        f"accepted dmabuf path should report last import details: {data}",
    )
    require(
        dmabuf_blocker_failures == 0,
        f"dmabuf blocker installation failed: {data}",
    )
    require(
        data.get("dmabuf_last_blocker_error") in {None, "none"},
        f"dmabuf blocker path reported an error: {data}",
    )
    blocker_count = dmabuf_explicit_sync_blockers + dmabuf_implicit_blockers
    require(blocker_count >= 1, f"dmabuf path did not install a readiness blocker: {data}")

    if explicit_sync == "v1":
        require(dmabuf == "v4", f"explicit sync should only be advertised with dmabuf v4: {data}")
        require(
            explicit_sync_disabled == "none",
            f"advertised explicit sync should not report a disabled reason: {data}",
        )
        require(
            has_syncobj_global_or_bind(trace),
            "client WAYLAND_DEBUG trace did not see/bind linux-drm-syncobj",
        )
    else:
        allowed_disabled_reasons = (
            EXPLICIT_SYNC_DISABLED_REASONS_WITH_DMABUF_V4
            if dmabuf == "v4"
            else EXPLICIT_SYNC_DISABLED_REASONS_WITH_DMABUF_V3
        )
        require(
            explicit_sync_disabled in allowed_disabled_reasons,
            "disabled explicit sync path should report a coherent positive-dmabuf reason: "
            f"{data}",
        )
        require(
            not has_syncobj_global_or_bind(trace),
            "client WAYLAND_DEBUG trace saw linux-drm-syncobj despite explicit_sync=none",
        )

    if require_explicit_sync:
        require(
            explicit_sync == "v1",
            f"explicit sync validation requires advertised syncobj v1: {data}",
        )
        require(
            dmabuf_explicit_sync_blockers >= 1,
            f"explicit sync path did not install an acquire-point blocker: {data}",
        )
        dmabuf_explicit_sync_release_points = require_int_field(
            data, "dmabuf_explicit_sync_release_points"
        )
        require(
            dmabuf_explicit_sync_release_points >= 1,
            f"explicit sync path did not observe a release point: {data}",
        )


def validate_status(status_json: str, trace: str, require_explicit_sync: bool) -> None:
    data = json.loads(status_json)
    if data.get("dmabuf_formats", 0) == 0:
        validate_disabled_path(data, trace)
    validate_positive_path(data, trace, require_explicit_sync)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--status-json", required=True, help="Brood status-json payload")
    parser.add_argument("--trace", required=True, type=pathlib.Path, help="WAYLAND_DEBUG trace path")
    parser.add_argument(
        "--require-explicit-sync",
        choices=("0", "1"),
        default="0",
        help="require linux-drm-syncobj evidence and explicit sync counters",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    trace = args.trace.read_text(errors="replace") if args.trace.exists() else ""
    try:
        validate_status(args.status_json, trace, args.require_explicit_sync == "1")
    except SkipValidation as skip:
        print(skip)
        return 77
    except (TypeError, ValueError, json.JSONDecodeError) as err:
        print(err, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
