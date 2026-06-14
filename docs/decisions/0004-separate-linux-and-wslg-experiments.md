# 0004: Separate Linux and WSLg Experiments

## Context

Manifold currently has two implementation paths with very different constraints.

The normal Linux path is a Rust nested Wayland compositor with a presentation window that Manifold controls. It is the clean reference implementation for session, pane, detach, input, and rendering behavior.

The WSLg path is a C-first experiment inside WSLg/Weston. WSLg already owns Windows window creation and RDPRAIL presentation, and the current insertion point is not a normal Manifold presentation endpoint. Attempts to force the same runtime implementation across both paths would introduce FFI, lifecycle, ownership, and backend abstraction costs before the WSLg architecture is understood.

## Decision

Keep the Linux/Rust nested compositor and WSLg/C Weston implementation as separate experiments for now.

Unify them through a shared behavior spec rather than shared runtime code. The shared spec should describe Manifold semantics such as pane identity, focus rules, attach/detach, show/hide, input routing, session persistence, and window grouping. Each implementation may satisfy those semantics using platform-specific mechanisms.

## Consequences

- The Linux implementation can continue to move quickly as the Rust reference path.
- The WSLg implementation can stay close to Weston, RDPRAIL, and Windows presentation realities.
- Patch queues and local WSLg reference checkouts remain acceptable during the spike.
- Shared code should be deferred until an implementation boundary is obvious and worth the cost.
- Tests should initially target observable behavior rather than a common internal API.

## Follow-Up

- Write the first shared behavior spec once the WSLg multi-window composition PoC exists.
- Revisit shared code only after both implementations have converged on stable concepts.
- Prefer protocol compatibility over shared renderer or compositor abstractions.
