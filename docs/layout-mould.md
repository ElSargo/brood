# Layout Mould

Manifold layout should be split between a host-owned renderer and a reloadable
layout policy layer.

The layout policy layer may be implemented by WASM, but it should produce data,
not pixels. Native code remains responsible for rendering, hit testing, input
capture, animations, surface ownership, focus, and final constraint enforcement.

## Responsibilities

### Native Host

The native host is the authority for presentation execution.

It owns:

- native windows and imported surfaces
- rendering and composition
- pointer capture, keyboard focus, and hit testing
- animation and preview drawing
- actual bounds calculation from validated layout data
- persistence and migration of accepted layout state
- fallback behavior when a policy module fails
- validation of all module-provided layout data

The host may ask a layout policy module for decisions, but it commits only a
validated workspace or presentation transaction.

### `LayoutMould`

`LayoutMould` is the top-level layout policy concept.

It decides the logical arrangement of the major presentation regions:

- center area
- left side panel area
- right side panel area
- bottom dock
- overlays or transient regions

It does not render those regions. It returns a layout data structure describing
which regions exist, their visibility, relative priority, and sizing policy.

### Split Layout

The center area may contain recursive splits. Splits should be represented as a
logical tree with axes, children, and flex weights.

The split model should not store native surface handles or raw renderer objects.
It should refer to panes, panels, and regions by stable logical IDs.

Example shape:

```rust
struct LayoutSpec {
    regions: Vec<RegionSpec>,
    center: SplitTree,
}

struct RegionSpec {
    id: RegionId,
    kind: RegionKind,
    visible: bool,
    size: SizePolicy,
    min_size: Option<f32>,
    priority: u32,
}

enum RegionKind {
    Center,
    LeftDock,
    RightDock,
    BottomDock,
    Overlay,
}

enum SplitTree {
    Pane(PaneId),
    Split {
        axis: Axis,
        children: Vec<SplitTree>,
        flexes: Vec<f32>,
    },
}
```

This mirrors the useful part of Zed's approach: persistent pane topology is a
tree, while actual geometry is computed by the native presentation layer.

## WASM Boundary

A WASM layout module should behave like a pure policy engine:

```text
input: current layout snapshot + event + host constraints
output: layout patch, replacement layout model, rejection, or abstain
```

It should operate on logical IDs and typed constraints. It must not receive
native object references, renderer internals, Smithay objects, Wayland resources,
EGL objects, or DMA-BUF handles.

The host validates every response before committing it.

Validation should reject or repair:

- duplicate pane, panel, or region IDs
- missing or invalid center area
- split flex counts that do not match child counts
- empty split groups
- negative, NaN, or unbounded sizes
- impossible min/max constraints
- panel placements that the panel does not support
- references to panes or panels outside the module's authority

If validation fails, the host should use a deterministic native fallback and
record diagnostics.

## Fast Path

Layout policy belongs in the synchronous fast lane only when Manifold needs a
decision before committing a workspace or presentation transaction.

Good fast-path hooks:

- classify a newly added window or pane
- choose initial placement for a new managed surface
- answer whether a dragged pane can be dropped on a target
- convert a drop target into a semantic layout operation
- rewrite a split or docking request before commit

The fast path must be bounded. Hooks should have strict time and memory limits,
trap accounting, and automatic disabling after repeated failures.

The host should avoid calling WASM on every pointer-motion frame when dragging.
Preferred drag flow:

1. On drag start, ask policy for valid drop zones, insertion rules, and
   constraints.
2. During drag motion, perform native hit testing against that prepared data and
   render previews locally.
3. On drop, ask policy to turn the selected target into a semantic layout patch.
4. Validate and commit the result, or fall back to native behavior.

This keeps pointer motion smooth even when policy is reloadable.

## Events And Responses

Candidate event shape:

```rust
enum LayoutEvent {
    WindowAdded {
        window: WindowDescriptor,
        source: AddWindowSource,
        preferred_area: Option<RegionId>,
    },
    WindowRemoved {
        window_id: WindowId,
    },
    DragStarted {
        pane_id: PaneId,
    },
    DragDropped {
        pane_id: PaneId,
        target: DropTarget,
    },
    SplitRequested {
        pane_id: PaneId,
        axis: Axis,
        side: Side,
    },
}

enum LayoutResponse {
    Patch(Vec<LayoutPatch>),
    Replace(LayoutSpec),
    Reject { reason: String },
    Abstain,
}
```

The response is a proposal. The native host is the committer.

## Fallback Rule

Manifold must always have a native fallback layout mould.

If the WASM module is missing, slow, trapped, disabled, or returns invalid data,
the native fallback should still place windows predictably, preserve existing
panes, and keep the presentation usable.

Reloadable layout policy should make Manifold more adaptable, not fragile.
