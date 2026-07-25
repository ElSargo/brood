# 0002: Local MVP Presentation

## Status

Accepted for the MVP prototype.

## Context

The project needs to prove that an extra local compositor/session layer can feel native while preserving managed app state. A separate presentation endpoint would preserve the long-term split, but it adds lifecycle and protocol work before the core risk is proven.

Remote connections are not a current goal. The first question is whether Brood can host a Wayland app through local Wayland/DMA-BUF-compatible surface sharing, close its presentation window, reopen it, and make the managed app feel as though its regular window never went away.

## Decision

The MVP will use a single local `brood` process that owns both the durable session and the local presentation window.

- `brood` owns the nested Wayland compositor for hosted Wayland apps.
- `brood` opens a normal desktop presentation window on the user's current compositor.
- Hosted Wayland apps connect to Brood's Wayland display, not directly to the system compositor.
- Closing the presentation window detaches by default when there is managed app or session state worth preserving.
- Remote presentation, remote networking, and multi-user security are non-goals for this phase.
- A separate `brood-client` process is deferred.

## Consequences

- The MVP can focus on local surface import, synchronization, frame lifecycle, input forwarding, and detach/reopen behavior.
- Renderer or presentation crashes may take down the durable session in the MVP.
- A future split presenter remains possible once the local native-feeling indirection has been proven.
