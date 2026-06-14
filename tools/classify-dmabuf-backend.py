#!/usr/bin/env python3
"""Classify Manifold's presentation backend from DMA-BUF status fields."""

from __future__ import annotations

import argparse
import sys


EXPLICIT_SYNC_DISABLED_REASONS_WITH_DMABUF_V4 = {
    "drm_render_node_path_unavailable",
    "drm_render_node_open_failed",
    "drm_syncobj_eventfd_unsupported",
}


def classify(
    dmabuf: str,
    dmabuf_formats: int,
    explicit_sync: str,
    explicit_sync_disabled: str,
) -> tuple[str, str, str]:
    if dmabuf_formats == 0:
        return (
            "fallback_no_dmabuf_import",
            "presentation renderer reported no importable dmabuf formats; "
            "Manifold must not advertise linux-dmabuf or syncobj",
            "Use WSLg for fallback correctness; validate positive dmabuf on native/DRM Linux "
            "with ./tools/validate-dmabuf-positive.sh",
        )
    if dmabuf == "v4" and explicit_sync == "v1":
        return (
            "drm_capable_dmabuf_and_explicit_sync",
            "backend advertises dmabuf v4 and explicit sync; run strict no-skip audit",
            "Run MANIFOLD_AUDIT_REQUIRE_NO_SKIPS=1 ./tools/audit-v1-capabilities.sh",
        )
    if dmabuf == "v4" and explicit_sync == "none":
        if explicit_sync_disabled in EXPLICIT_SYNC_DISABLED_REASONS_WITH_DMABUF_V4:
            return (
                "dmabuf_v4_without_explicit_sync",
                "backend advertises dmabuf v4 but explicit sync is unavailable; "
                "positive dmabuf can run, strict fence validation should skip",
                "Run ./tools/validate-dmabuf-positive.sh; strict explicit-sync validation needs "
                "syncobj/eventfd support",
            )
        return (
            "incoherent_dmabuf_status",
            "backend reports dmabuf v4 without explicit sync but the disabled reason is not coherent",
            "Run ./tools/smoke-dmabuf-diagnostics.sh and ./tools/smoke-explicit-sync-diagnostics.sh",
        )
    if dmabuf == "v3":
        return (
            "dmabuf_import_without_v4_feedback",
            "backend advertises dmabuf v3 but lacks render-node/default-feedback support for v4 "
            "explicit sync",
            "Run ./tools/validate-dmabuf-positive.sh; strict explicit-sync validation needs dmabuf "
            "v4 plus syncobj support",
        )
    return (
        "incoherent_dmabuf_status",
        "backend status is unusual; run dmabuf diagnostics and inspect status/logs",
        "Run ./tools/smoke-dmabuf-diagnostics.sh and ./tools/smoke-explicit-sync-diagnostics.sh",
    )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dmabuf", required=True)
    parser.add_argument("--dmabuf-formats", required=True, type=int)
    parser.add_argument("--explicit-sync", required=True)
    parser.add_argument("--explicit-sync-disabled", required=True)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.dmabuf_formats < 0:
        print("ERR dmabuf-formats must be non-negative", file=sys.stderr)
        return 2
    classification, summary, next_step = classify(
        args.dmabuf,
        args.dmabuf_formats,
        args.explicit_sync,
        args.explicit_sync_disabled,
    )
    print(f"classification={classification}")
    print(f"summary={summary}")
    print(f"next={next_step}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
