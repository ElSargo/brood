# Tools

Development tools and supported smoke tests live here.

Experimental backend work that is not part of the supported workflow should live outside this directory so the default tool surface stays focused.

## Primary Workflows

Run `./tools/dev-nested-manifold.sh --keep-running` to start a stable Manifold instance, spawn a development Manifold instance inside it, launch a real client inside the dev instance, and sample both `status-json` streams. See [Test Nested Behavior](../docs/workflows/test-nested.md).

Run `./tools/audit-v1-capabilities.sh` from the repo root for the concise v1 acceptance audit. It runs formatting, offline checking, unit tests, headless compositor checks, supported host-backed smoke tests when a host display is available, and the dmabuf capability checks. Set `MANIFOLD_AUDIT_REQUIRE_NO_SKIPS=1` on a DRM-capable backend to make capability skips fail the audit.

Run `./tools/smoke-v1-full.sh` from the repo root for the full supported v1 proof suite: formatting, offline build/test, dmabuf validator/classifier fixtures, headless Wayland globals, headless native-client mapping, headless input forwarding, headless control-error handling, headless JSON status, headless status parity, headless client lifecycle checks, startup attach fallback, native nesting, detach/reattach, host input forwarding, dmabuf diagnostics, explicit-sync diagnostics, and latency/cadence.

Run `nix build .#checks.$(nix eval --raw --impure --expr builtins.currentSystem).software-e2e` for the sandboxed software-rendered graphical proof. It uses llvmpipe, real Weston clients, nested Manifold presentation, split layout actions, and debug dumps with composed PNG captures.

The full suite and compositor smoke scripts require a host display socket for the Winit presentation endpoint. If preflight fails, the log includes `host_display_diagnostic=begin...end` with socket paths, environment, and directory listings. If an unusual backend is available but these socket checks do not detect it, set `MANIFOLD_SKIP_HOST_PREFLIGHT=1` to let `manifold` attempt startup directly. The headless smoke scripts can still be run individually when no host display is available.

## Focused Checks

Run `./tools/manifold-behavior-probe.sh` to start Manifold with a strict dmabuf-capable test client and emit JSONL behavior telemetry.

Run `./tools/profile-native-scroll.sh` on a native Linux Wayland session to profile the Smithay presentation path with a real browser autoscroll workload.

Run `./tools/smoke-manifoldd.sh` to build `manifold`, launch a real nested Wayland client, verify frame/commit progress, detach the presentation window without closing the nested window, reattach it, and validate dmabuf advertisement diagnostics.

Run `./tools/smoke-input-forwarding.sh` to launch `weston-eventdemo`, inject pointer/key/axis events through Manifold's DBus control endpoint, and assert that the nested Wayland client logs the forwarded events.

Run `./tools/smoke-status-json.sh` to launch a real nested Wayland client, inject targeted pointer input, request both `status` and `status-json` over the DBus control endpoint, and validate attached schema parity, visible input-to-frame telemetry, core runtime invariants, and dmabuf diagnostic invariants.

Run `./tools/smoke-dmabuf-diagnostics.sh` to compare Manifold's dmabuf status fields with a nested client's `WAYLAND_DEBUG` trace.

Run `./tools/smoke-explicit-sync-diagnostics.sh` to inspect Manifold's nested Wayland globals with `wayland-info` after presentation attach.

Run `./tools/diagnose-dmabuf-backend.sh` to classify the current presentation backend for DMA-BUF validation.

Run `./tools/validate-dmabuf-positive.sh` on a backend with importable dmabuf formats to prove the positive path. Set `MANIFOLD_DMABUF_REQUIRE_EXPLICIT_SYNC=1` to validate fence correctness too.

Run `./tools/smoke-latency-cadence.sh` to launch a real nested client, inject input, and verify frame cadence, input-to-frame latency, and visible client commit-to-frame latency.

## Headless Checks

The `smoke-headless-*` scripts validate compositor behavior without a host presentation socket. They cover Wayland globals, native-client mapping, input forwarding, control errors, status schema parity, client exit, multi-client lifecycle, attach diagnostics, presentation-close idempotency, and repeated attach-failure handling.

Use these when host presentation is unavailable or when a failure needs to be isolated from the Winit presentation endpoint.
