---
status: experiment
type: design-experiment
authority: design
last-reviewed: 2026-09-04
---

# Full GPUI Transition Specification

This specification defines the strongest version of Brood's host-managed UI
direction: GPUI owns the presentation application and native UI lifecycle, and
plugins send revisioned semantic UI documents to the host. Brood translates
those documents into host-owned GPUI entities, routes semantic actions back to
actors, and retains escape hatches for plugin-authored graphics.

This is a complete alternative end state and transition plan, not the selected
product target. The selected near-term path remains the
[[../targets/systems/plugin-pure-renderer]]. Keeping this document as an
experiment makes the opportunity testable without silently committing the
plugin ABI to GPUI or weakening the pure-render work.

## Decision Being Tested

The transition deliberately trades some plugin implementation freedom for
host ownership of layout, text, input, focus, accessibility, animation, and
native presentation timing. It succeeds only if those gains are large enough
to justify a new managed-UI protocol and if application surfaces can be
integrated without restoring the resize and copy-path problems the current
architecture was built to avoid.

The end state is not “plugins use GPUI.” Plugins use a stable Brood UI
protocol. GPUI is a replaceable host implementation behind that protocol. No
GPUI Rust type, entity handle, callback, stylesheet, version, or lifecycle
object crosses the Component Model boundary.

## Upstream Assumptions

The experiment begins from these current upstream facts, which must be
revalidated when the spike starts:

- GPUI is a hybrid immediate/retained GPU UI framework whose application owns
  its run loop and windows.
- GPUI is pre-1.0 and warns that breaking changes should be expected.
- Its application context owns global state and entities; foreground contexts
  mutate UI state, while background work returns through framework contexts.
- Linux support includes Wayland and X11 platform paths. A headless/test path
  exists, but it is not proof that Brood's nested production workload works.

The source assumptions are recorded in GPUI's
[README](https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md),
[context documentation](https://github.com/zed-industries/zed/blob/main/crates/gpui/docs/contexts.md),
and [crate manifest](https://github.com/zed-industries/zed/blob/main/crates/gpui/Cargo.toml).
The implementation must pin an exact audited revision rather than tracking an
unbounded branch or treating the latest published crate as protocol authority.
The audit includes licenses, minimum Rust version, Nix closure, supported Linux
backends, renderer dependencies, startup cost, binary size, and maintenance
surface.

## End-State Ownership

| Concern | Final owner |
| --- | --- |
| Application run loop and presentation window | GPUI presentation client |
| Native window events, frame requests, scale, and text input | GPUI presentation client |
| Sessions, actors, capabilities, routing, and lifecycle | Brood runtime |
| Pane tree, divider policy, and pane identity | Brood layout model |
| Managed component instances and transient interaction state | Brood GPUI adapter |
| Durable plugin application state | Plugin actor |
| Managed component vocabulary and wire semantics | Versioned Brood UI protocol |
| Custom canvas construction | Plugin pure renderer |
| GPU composition and external application surfaces | One GPUI-integrated graphics path |
| External Wayland clients and policy | `brood-sessiond`/Smithay |

The final presentation process has one native UI runtime and one renderer
authority. The existing Winit application loop does not run beside GPUI, and a
second compositor does not independently present into the same window.
Smithay remains in the external session process; this avoids the earlier
in-process lifecycle conflict but does not by itself solve surface import or
frame synchronization.

## Stable Managed-UI Protocol

### Documents, identity, and revisions

Each actor generation may publish one managed document per owned pane. A
document has stable node identities, a document revision, an optional base
revision, and a protocol version. The initial publication is a bounded
snapshot; later publications may be snapshots or atomic patches.

A conceptual Component Model surface is:

```wit
record ui-document {
    pane: u64,
    document-revision: u64,
    root: ui-node,
}

record ui-patch {
    pane: u64,
    base-revision: u64,
    document-revision: u64,
    operations: list<ui-operation>,
}

variant ui-publication {
    snapshot(ui-document),
    patch(ui-patch),
    remove-pane(u64),
}

record ui-action {
    pane: u64,
    document-revision: u64,
    node: u64,
    interaction-revision: u64,
    value: ui-action-value,
}
```

This is a semantic sketch, not frozen WIT. The checked-in interface becomes
the exact ABI authority only after the experiment proves the model.

Node identity is actor-generation scoped and stable across patches. A patch is
accepted only when its base revision equals the host's complete accepted
revision. Patches are validated and applied atomically; partial application is
forbidden. A mismatch requests a new snapshot and leaves the previous complete
document visible.

The host retains one accepted document and at most one latest pending
publication per pane. Publication transport, reconciliation, and diagnostics
are bounded. Intermediate revisions may be coalesced unless the plugin marks a
semantic action boundary that the protocol explicitly preserves.

### Initial component vocabulary

The first version is intentionally capable but finite:

- row, column, stack, grid, spacer, scroll region, divider, and virtual list;
- text, icon, image, badge, progress, and separator;
- button, toggle, radio group, menu, selectable row, link, and command target;
- text field, multiline editor, search field, and explicit text-editor bridge;
- tooltip, popover, dialog, context menu, and drag payload/target;
- canvas, external-surface, and terminal-view integration nodes.

Every node declares semantic properties rather than GPUI implementation
objects. Layout uses a versioned Brood value model. Color, typography, spacing,
radius, elevation, motion, and interaction appearance use host theme tokens
with bounded plugin overrides. Raw GPUI styles, arbitrary host closures, and
guest-provided shaders are outside the managed protocol.

The vocabulary evolves additively where possible. Capability negotiation lets
the plugin select a supported fallback. Unknown required nodes reject the
publication; unknown optional decoration is omitted predictably. Removing or
changing established semantics requires a protocol-version transition, not a
silent reinterpretation after a GPUI update.

### State boundary

Plugins own durable semantic state: selected session, query text used for
application behavior, model contents, command availability, and domain
decisions. The host owns transient framework state: hover, pressed state,
focus rings, pointer capture, scroll momentum, composition state, animation
progress, measurement caches, and virtualization bookkeeping.

Interactive values declare one of three modes:

- **controlled**: the actor's accepted document is authoritative;
- **host-transient**: the host owns the value for the node's lifetime and
  reports semantic completion;
- **hybrid**: the host updates immediately and the actor may confirm or replace
  the value with a later revision.

The mode is part of protocol semantics. It prevents every keypress or divider
pixel from requiring an actor round trip while still making correction and
recovery deterministic. Hybrid nodes carry an interaction revision so a stale
actor publication cannot overwrite newer host interaction accidentally.

### Actions back to actors

GPUI callbacks produce Brood semantic actions, not guest callbacks into a Wasm
instance. Actions enter the actor mailbox with pane, actor generation,
document, node, and interaction revisions. They obey ordinary actor ordering
and capability rules.

Visual pressed, hover, focus, scroll, text-composition, and resize feedback may
be rendered by the host before the actor reduces an action. Domain changes
which require actor authority remain pending or optimistic according to the
node's declared state mode. A rejected, stopped, or replaced actor generation
cannot receive or confirm actions from its old document.

## GPUI Application Integration

The presentation binary starts and remains inside GPUI's application
lifecycle. Brood services communicate with foreground-owned entities through
bounded channels or framework-supported background contexts. Background work
never mutates GPUI entities directly, and rendering does not synchronously wait
for actor I/O.

A Brood-owned adapter crate contains every dependency on GPUI. It maps stable
protocol nodes to entities/elements, maintains the identity/revision tables,
owns transient interaction state, and emits semantic actions. The core actor,
session, transport, and protocol crates do not depend on GPUI.

Brood's recursive pane model remains authoritative for pane identity,
split/stack topology, divider constraints, focus routing between pane classes,
and persistence. GPUI performs the native layout and painting for the current
committed pane tree. There is one geometry revision joining Brood layout,
managed nodes, custom canvas targets, and external surfaces.

### External application surfaces

Managed Wayland application content is the most important feasibility gate.
The preferred path imports sessiond-owned DMA-BUFs or another explicit GPU
resource into the one presentation renderer, associates acquire/release
synchronization and damage with the exact surface revision, and displays it in
an `external-surface` node without a CPU readback.

The spike must demonstrate that GPUI's renderer can accept this integration
through a maintained public extension, a small upstreamable abstraction, or a
bounded Brood patch. A shared-memory upload may be used to debug the protocol,
but it is not evidence for accepting the transition. If zero-copy or an
equivalent bounded GPU path requires a permanent fork across broad GPUI
internals, the full transition stops for redesign.

Surface buffers are never exposed to plugins. Brood owns lifetime, clipping,
transform, color-space handling, occlusion, synchronization, and recovery.
Sessiond remains responsible for Wayland client policy and buffer production.

### Plugin-authored graphics

The full transition retains the pure-render contract as the custom graphics
escape hatch. A `canvas` node identifies a pane-local or node-local render
target, and the host calls the actor's pure renderer with its exact allocated
extent, scale, clip, theme, and predicted presentation time. The returned
Brood canvas packet is lowered by the GPUI-integrated graphics path.

Plugins receive neither GPUI handles nor raw WGPU device access. Canvas output
participates in the same geometry and document transaction as managed nodes.
This preserves unusual visuals and incremental migration without making two
window loops or two independent presentation clocks.

## Reconciliation And Presentation

An accepted managed publication is reconciled into a candidate host tree.
Reconciliation may prepare measurements or resources asynchronously, but the
visible tree changes atomically at a document revision. Failure keeps the
previous complete tree and reports a bounded diagnostic to the actor and
operator.

Native GPUI layout means fixed-size managed nodes are recalculated and painted
for the current window geometry in one host frame. Actor publication is not on
the resize-critical path unless the plugin has declared a semantic breakpoint
or custom canvas whose output depends on the allocated extent.

One presentation candidate records:

- pane-tree and geometry revision;
- managed document revision per pane;
- custom render token and actor-state watermark per canvas;
- external-surface buffer revision and synchronization state;
- theme, scale, renderer, and actor generations.

The coordinator presents only a coherent candidate. Resize-sensitive canvases
use the same exact/repeat policy as the pure-render target. Managed host nodes
do not stretch an old whole-pane raster: GPUI lays them out for the candidate
geometry. External surfaces follow their explicitly negotiated resize policy
and are diagnosed separately.

## Input, Focus, Text, And Accessibility

GPUI receives native pointer, keyboard, touch, scale, and text-input events.
The Brood pane model first determines the target pane and modal scope; the GPUI
tree then resolves the target managed node. Canvas nodes receive normalized
Brood pane-input events and may return host-understood cursor, capture, focus,
and accessibility requests through their stable protocol.

The host owns focus traversal, pointer capture, hover, cursor selection,
keyboard routing, input method composition, clipboard gestures, drag/drop,
and accessibility tree construction for managed nodes. Plugins supply labels,
roles, values, relationships, commands, and domain descriptions, but cannot
inject arbitrary accessibility objects.

Focus restoration is keyed by actor generation, pane, document revision, and
stable node identity. When a focused node disappears, deterministic Brood
policy selects the next focus target. Actor replacement clears stale capture,
composition, menus, and pending actions before the replacement document is
made interactive.

## Failure, Recovery, And Limits

Malformed, oversized, unsupported, or stale publications never partially
mutate visible UI. The host keeps the previous complete document and returns a
structured diagnostic. Repeated protocol failure may replace the pane with a
host-owned failure surface without stopping unrelated actors or sessions.

Each actor generation negotiates limits for nodes, depth, text, patch
operations, images, retained resource bytes, outstanding actions, publication
rate, canvas packets, and per-reconciliation work. The host also bounds
diagnostic history and transient framework state. A plugin cannot construct an
unbounded host tree or create an unbounded callback queue.

GPU/device recovery rebuilds managed host state from the accepted document,
reimports current external surfaces, and replays accepted CPU-side canvas
packets where possible. It does not ask actors to reconstruct state solely
because GPUI lost renderer resources. Application/window failure, actor
replacement, and protocol recovery have explicit independent generations.

Diagnostics correlate native input, semantic actions, actor reductions,
document publications, reconciliation, layout, canvas rendering, external
surface readiness, composition, submission, and presentation. They include
coalescing, revision mismatch, stale action/publication, limit rejection,
fallback use, and instrumentation loss.

## Transition Plan

### Phase 0: dependency and ownership audit

Pin GPUI, build it in the checked-in Nix environment, enumerate the platform
and renderer dependency graph, and verify licensing and supported toolchains.
Map every current presentation-client responsibility to either GPUI, the Brood
adapter, sessiond, or removal. No product path changes in this phase.

### Phase 1: independent GPUI presentation spike

Create a separate experimental binary which opens under the real nested test
harness, renders instrumented fixed geometry, receives scale/input changes,
and exposes deterministic frame timing. It must not share a native window with
the current Winit client or become the default binary.

### Phase 2: composition feasibility gate

Display a real sessiond-managed Wayland client and a pure-render canvas inside
the GPUI window. Prove buffer import, synchronization, damage, clipping,
resize, device recovery, input mapping, and bounded memory under a sustained
real workload. This is the major go/no-go gate; failure returns the experiment
to protocol/renderer design rather than layering GPUI over the old compositor.

### Phase 3: shell and pane topology

Recreate Brood's recursive splits, stacks, divider constraints, focus policy,
overlays, and persistence through the adapter. Compare geometry and interaction
against the current client with the resize oracle and nested application
workloads.

### Phase 4: managed-UI protocol

Implement snapshots, atomic patches, stable identities, actions, state modes,
capability negotiation, bounds, recovery, and diagnostics. A conformance actor
exercises every node, stale revision, replacement, failure, input, text, and
accessibility path before bundled UI migrates.

### Phase 5: representative migrations

Migrate, in order, a small host status surface, the session switcher, the
session picker, inbox/overlay interactions, and the terminal shell. Preserve
the canvas bridge for custom content. At least one third-party-style actor must
be built against only the public Brood protocol to expose accidental coupling
to bundled plugins.

### Phase 6: default cutover

Run both presentation clients against the same runtime protocol long enough to
compare resize fidelity, latency distributions, resource use, failure
recovery, accessibility, input behavior, and plugin authoring cost. Select the
GPUI client only after the acceptance gates pass. Keep an explicit rollback
selection through one compatibility window.

### Phase 7: removal

After the compatibility window, remove the Winit application loop, duplicate
composition path, and legacy host UI implementations. Keep the pure renderer,
canvas protocol, actor/session contracts, external session process, and
instrumentation that remain part of the final design. Archive transition-only
code and promote this experiment into canonical target documents only if the
design decision has actually been made.

## Acceptance Gates

The transition is eligible to become a product target only when a real nested
workload proves all of the following:

- GPUI owns the sole native application/window lifecycle with no event-loop
  interposition or second presentation clock.
- A real sessiond application surface reaches the window through a bounded GPU
  path with correct synchronization, damage, resize, recovery, and no steady
  CPU readback.
- The session picker passes the 180 Hz fixed-element geometry oracle at least
  as well as the pure-render path.
- Managed text input, IME, clipboard, focus, pointer capture, keyboard routing,
  drag/drop, and accessibility work across actor replacement and pane changes.
- The versioned protocol can represent the bundled UI without GPUI types or
  privileged plugin-specific host branches.
- An intentionally unsupported design can use the canvas bridge without
  compromising geometry atomicity or input routing.
- Publications, patches, actions, trees, resources, and diagnostics remain
  bounded under adversarial rates and malformed input.
- Device loss, actor failure, stale revisions, and sessiond reconnect preserve
  or restore one coherent complete UI rather than partially reconciling state.
- Measured latency, CPU/GPU time, memory, binary/startup cost, and maintenance
  burden are acceptable relative to the pure-render baseline.

## Stop Conditions

The experiment does not proceed to product migration if any of these remains
true after the relevant spike:

- external surfaces require regular CPU readback or an unbounded private GPUI
  fork;
- GPUI cannot be the sole event-loop owner in the supported presentation
  topology;
- the protocol must expose GPUI types or arbitrary host callbacks to express
  ordinary plugin UI;
- actor round trips are required for native resize, hover, pressed, focus,
  scrolling, or text composition feedback;
- framework updates routinely force wire-semantic changes;
- the canvas escape hatch behaves as a second independent compositor rather
  than one node in the coherent frame;
- accessibility or input behavior regresses without a bounded repair path;
- the measured benefit over the pure-render target does not justify the lost
  flexibility and permanent protocol surface.

## Relationship To The Pure Renderer

The two specifications are alternatives for ordinary plugin UI ownership, but
the work is not mutually disposable. The pure renderer remains useful in a
GPUI end state for canvas nodes, difficult visualizations, incremental plugin
migration, and as a performance/control baseline. Its exact geometry,
latest-wins scheduling, bounded output, transactional validation, and causal
instrumentation requirements carry directly into the custom graphics bridge.

What changes is the default abstraction. In the pure-render target, plugins
own component implementation and send finished draw packets. In the full GPUI
end state, plugins normally send semantic UI state and the host owns component
implementation. The experiment must quantify this flexibility trade rather
than assume that smoother resize alone decides it.

Related:

- [[../targets/systems/plugin-pure-renderer]]
- [[../targets/systems/plugin-ui]]
- [[../targets/systems/plugin-runtime]]
- [[../targets/systems/plugin-graphics]]
- [[../targets/systems/presentation-client]]
- [[../targets/systems/render-scheduling]]
- [[../frictions/resize-element-stability]]
