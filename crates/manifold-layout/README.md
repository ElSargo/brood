# Manifold Layout

`manifold-layout` is the portable viewport-layout mechanism shared by
presentation hosts. It has no compositor, window-system, renderer, transport,
or WASM-runtime dependencies.

The crate owns:

- the bounded, validated resolved-layout model accepted from layout actors;
- normalized-to-pixel projection and layout-derived hit testing;
- the deterministic native split-tree fallback and its actions.

Presentation hosts remain responsible for pane inventory, focus, rendering,
input delivery, policy execution, and persistence. This keeps the crate usable
by the current daemon while allowing the same layout behavior to move into
`manifold-client`.
