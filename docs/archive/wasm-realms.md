# WASM Realms

This archived page records an earlier target design for WASM realm kinds, binding rules, and cross-realm interaction.

Use [Plugin](../targets/concepts/plugin.md) and [Plugin Runtime](../targets/systems/plugin-runtime.md) for the current target plugin model. Use [WASM Integration](wasm-integration.md) only as historical context for the broader app and module model.

In this design, a realm is an API and lifetime contract for a WASM module instance. It is not a separate trust boundary and it does not replace the workspace and session object model.

## Realm Kinds

Brood should support three realm kinds:

- `global`
- `session`
- `surface`

These realm kinds differ mainly in:

- which object is implicitly bound for the module instance
- which convenience APIs are available without lookup
- which lifetime the module instance follows

The host may still enforce capability checks, but realm choice is primarily about API shape and lifecycle rather than hard isolation.

## Global Realm

A global realm instance is tied to the lifetime of the session process.

It is suitable for work that does not neatly belong to one session or one presentation surface, such as:

- cross-session coordination
- background automation
- service registries or brokers
- global policy helpers
- system-wide UI or orchestration

Global modules should not receive implicit access to sessions or surfaces. They should look them up explicitly through host APIs. Because global instances can outlive any session or surface, all object access from the global realm must be treated as weak and fallible.

## Session Realm

A session realm instance is tied to one session.

It is the default place for durable per-session behavior such as:

- build managers
- git/status tools
- project automation
- session-local agents
- pane-producing WASM apps

Session realm instances should have implicit access to their bound session through convenience APIs such as `this_session()`. Access to other sessions or to presentation surfaces should be explicit.

One session-targeted module binary may be instantiated many times, once per live session that chooses to run it. Reloading a module should restart all of its live session-realm instances.

## Surface Realm

A surface realm instance is tied to one presentation surface.

It is the place for presentation-local behavior such as:

- view manipulation
- pane arrangement
- session switching within the surface
- surface-local commands or overlays

Surface realm instances should have implicit access to their bound surface through convenience APIs such as `this_surface()`. They should not receive implicit access to arbitrary other surfaces or sessions.

A surface may switch which session it presents without recreating the surface realm instance. In that case the instance remains alive and receives a surface or workspace-switch-style event describing the new attached session. Any API that follows the current attached session should remain fallible because the binding may change or disappear.

## Binding Model

Realm instances are created lazily. The realm is a contract that exists for each supported object kind even when no module is currently loaded into it.

The intended initial rule is one primary bound object per realm instance:

- `global`: no implicit session or surface binding
- `session`: one bound session
- `surface`: one bound surface

Panes are not a separate realm kind. They are ordinary objects that may be created and managed by modules running in any appropriate realm, usually the session realm.

## Module Targeting

Modules should target one realm kind explicitly. The host should define which entry points are valid for which realm kind and instantiate modules accordingly.

That keeps placement simple:

- session-oriented modules target `session`
- presentation-oriented modules target `surface`
- orchestration or broker modules target `global`

If a higher-level module needs work performed in another realm, the preferred direction is host-managed instantiation rather than ad hoc cross-realm ownership. The expected flow is mostly:

- `global -> surface`
- `surface -> session`

This should stay a host-mediated lifecycle action, not a guest-owned object graph.

## Object Access

The base host API should support explicit object-addressed operations such as:

- get a session by `SessionId`
- get a surface by `SurfaceId`
- enumerate sessions or surfaces
- operate on a specific pane, session, or surface by id

Realm-specific convenience APIs should remove the need to pass an id for the bound object:

- `this_session()` in the session realm
- `this_surface()` in the surface realm

The surface realm may also expose a fallible `attached_session()` convenience because the surface can switch what it presents over time.

The bound object of the current realm behaves like a strong reference for lifetime purposes. Everything else is weak. Weak references must be treated as invalidatable at any time.

## Weak Handle Semantics

Cross-realm and out-of-realm object references should be weak by default.

The preferred shape is:

- explicit ids for lookup
- no guarantee that a looked-up object is still alive when used later
- APIs return `result` or `option` when lifetime races are possible

Examples:

- a global module can hold a `SessionId`, but any later operation on that session may fail if the session has exited
- a session module can enumerate surfaces presenting it, but messaging one of them later may fail if the surface closed
- a surface module can cache pane ids, but pane operations may fail if those panes were closed or replaced

By contrast, operations on the bound session itself from the session realm should not fail due to the session itself disappearing, because the module instance shares that lifetime. Calls that traverse from the session to shorter-lived child objects such as panes may still fail.

## Coordination

Brood should provide a small built-in coordination layer rather than forcing all cross-realm interaction through a custom global broker.

The initial built-in primitives should be:

- a registry
- mailbox-style messaging
- plugin-scoped publications for latest-state publication to policy providers

These should prefer instance ids as the default addressing model. Higher-level aliases or named services may be layered on top later through the registry or through a global broker module.

RPC can remain a later addition if real workloads prove it necessary. The first coordination model should stay asynchronous and actor-like, with policy providers reserved for immediate host decisions rather than general RPC.

## Event Shape

Events should follow the bound object of the realm instance:

- global modules may observe everything
- session modules observe their bound session and its child objects
- surface modules observe their bound surface and receive explicit events when the attached session changes

The exact subscription API remains open, but the intent is that realm-local events are easy while broader observation is explicit.

## Storage Scope

Persistent storage should follow realm lifetime:

- global storage is process-global
- session storage is session-scoped
- surface storage is surface-scoped and may be ephemeral unless the host explicitly persists presentation preferences elsewhere

This keeps storage ownership aligned with module lifetime and avoids hiding cross-realm durability behind convenience APIs.

## Open Questions

The following parts remain intentionally unresolved:

- whether the durable object should be named `session`, `workspace`, or another term in the final API
- the exact lifecycle hooks for attach, switch, suspend, resume, and shutdown
- mailbox ordering and synchronization guarantees
- whether named service aliases should be built in or layered on top of the registry
