---
status: progress
type: verification-record
authority: implementation
---

# Terminal Plugin Progress

The checked-in terminal receives committed text, launches the configured
allowlisted `sh`, and keeps the boot terminal pair available across presentation
replacement. Shortcut-created terminal actors survive presentation disconnect,
but their inventory is not replayed to a replacement client and they remain
headless.

The Nix-backed nested proof drives a real presentation client and PTY. It covers
shell interaction, ANSI styles and Unicode, function and modified-key bytes,
application-cursor negotiation, Helix, alternate-screen resize, scrollback,
presentation replacement, a managed Ghostty descendant, sustained output,
`Ctrl+Alt+T` terminal creation, child exit, and resource cleanup. Screenshots
and structured latency, mailbox, stream, packet, and text-churn metrics provide
the observable evidence.

The target product boundary is [[../targets/systems/terminal-plugin]]. Remaining
design-level gaps are in [[../frictions/terminal-plugin]].

## 2026-08-31 interaction polish

The presentation actor now paints a selection across every selected cell,
including spaces and gaps between styled runs, rather than limiting the visible
highlight to cells which happen to carry a text run. Its cursor uses the shared
Ashen accent and blinks on a single-flight 530 ms worker while the pane is
focused. Input immediately restores the visible phase; losing focus leaves a
steady cursor and stops scheduling blink workers.

The Nix-backed focused terminal UI suite passed 10 tests, and the crate passed
Clippy with warnings denied. The packaged nested terminal proof remains the
graphical acceptance gate for the interaction as a whole.

## 2026-09-11 Pure renderer terminal stability and render loop fixes

Fixed a crash and unbounded render loop where the terminal visual facet actor either
failed after a few seconds of operation or entered an infinite retry loop.

Root causes and resolutions:
1. `plugin_graphics.rs` hardcoded `MAX_RESOURCES_PER_PANE = 1_024` and `MAX_RESOURCE_BYTES_PER_PANE = 64 MiB`,
   whereas `actor_runtime.rs` configured terminal views with `max_resources = 16_384` and up to 4 in-flight
   invocations (`max_invocations: 4`). Dense terminal layouts and cursor blinks across frames exceeded 1,024
   live resources, returning `PresentationError::LimitReached`. Limits were raised to 65,536 resources and 256 MiB per pane.
2. [P2] In `facet_resources.rs:262`, backend allocation failures returning `LimitReached` must not set
   `allocation_deferred`. `LimitReached` covers permanent conditions (oversized text/image, per-pane ceilings,
   hard limits), while only `AdmissionDeferred` represents transient retryable pressure. Setting `allocation_deferred`
   on `LimitReached` misclassified permanent limits as temporary pressure, causing the host to discard even valid
   responses and retry forever. `facet_resources.rs` now strictly inspects for `PresentationError::AdmissionDeferred`.
3. [P1] In `plugin_facets.rs:495`, empty render output was previously treated as transient `DiscardedRender`.
   Because `retire_render_opportunity()` marks the facet dirty again, deterministic errors (e.g. packet encoding limits,
   exceeding `MAX_COMMANDS`, or malformed input) immediately repeated in an infinite loop.
   `plugin_facets.rs` now only yields `DiscardedRender` when `lease.allocation_deferred()` explicitly records
   transient pressure. Non-deferred empty outputs fail immediately with an explicit error.
4. In `pure_renderer.rs`, dense terminal snapshots containing many runs (up to 16,384 runs plus background
   and cursor commands) exceed `brood_graphics::MAX_COMMANDS` (16,384), causing `packet.encode()` to return
   `GraphicsError::LimitReached`. Only `GraphicsError::LimitReached` falls back to visible error placeholder content
   (`placeholder_content`). Fatal errors (`Unavailable`, `InvalidPane`, `InvalidToken`, `HostFailure`) are
   preserved and propagated, returning an empty response that reaches the host's explicit failure path rather
   than silently leaving the terminal frozen on a placeholder frame.
5. In addition, theme decoding safely defaults via `unwrap_or_default()`, and fallback responses are encoded
   if response wire encoding limits are exceeded.

Verification:
- `cargo test --manifest-path crates/brood-wasm/Cargo.toml` passed (including test verifying backend `LimitReached` does not set `allocation_deferred`).
- `cargo test --manifest-path clients/brood-client/Cargo.toml` passed (all 363 tests).
- `cargo test --manifest-path plugins/terminal-ui/Cargo.toml` passed (all 27 tests, including `command_overflow_snapshot_falls_back_to_placeholder_instead_of_empty_output`).
- `nix develop --command bash tools/test-terminal-plugin-nested.sh` executes headless nested compositor verification with pure renderer.
