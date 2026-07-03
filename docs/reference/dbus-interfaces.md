---
status: current
type: reference
authority: implementation
last-reviewed: 2026-07-03
code-links:
  - crates/manifoldd/src/control.rs
---

# DBus Interface Reference Pointers

Exact DBus method signatures and tuple shapes are defined in `crates/manifoldd/src/control.rs`.

Current interface names:

- `org.manifold.Session.Control`
- `org.manifold.Manager`
- `org.manifold.Extension`
- `org.manifold.Plugin`
- `org.manifold.Agent`
- `org.manifold.Status`
- `org.manifold.Compositor`

Related:

- [[../systems/dbus-control]]
- [[../systems/extension-host]]
- [[../systems/plugin-runtime]]
