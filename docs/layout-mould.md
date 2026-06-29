# Resolved Layout

Manifold layout policy should be reloadable, but presentation execution remains
native-owned.

The target boundary is:

```text
pane inventory + presentation constraints + layout event
    -> WASM layout policy
    -> candidate resolved layout
    -> native validation
    -> native render, hit testing, focus, and input delivery
```

WASM may use any internal policy model it wants. It can model splits, stacks,
tabs, floating panes, docks, hidden panes, collapsed panes, or workspace-specific
rules. Native code should not require that internal tree shape. Native code only
accepts the final resolved layout data.

## Native Responsibilities

The native host owns presentation execution:

- imported surfaces and native windows
- final composition and rendering
- pointer capture, keyboard focus, and hit testing
- coordinate and transform math
- validation and canonicalization of candidate resolved layouts
- input delivery to panes or layout items
- fallback behavior when the layout module is missing, slow, trapped, or invalid

The host does not ask WASM to render pixels or expose Smithay, EGL, DMA-BUF,
renderer objects, Wayland resources, or native surface handles.

## WASM Responsibilities

A layout module receives pane inventory, constraints, and an event or reason for
layout. It returns a candidate resolved layout.

The module may omit panes that should not participate in the current
presentation. Hidden, collapsed, minimized, detached, or inactive-session panes
remain in the workspace/session model, but they are not present in the resolved
layout unless they are currently visible or hittable.

The module should emit explicit items for visible empty regions or controls. A
drop target, placeholder, resize handle, panel background, or overlay is
presentation behavior, so it belongs in the resolved layout even when no pane is
inside it.

## Resolved Layout

Resolved layout is a host-validated flat presentation description. It is not a
native split tree and it is not the WASM module's persistent state.

Candidate Rust shape:

```rust
struct ResolvedLayout {
    items: Vec<ResolvedLayoutItem>,
}

struct ResolvedLayoutItem {
    id: LayoutItemId,
    kind: LayoutItemKind,
    z: f64,
    transform: LocalTransform,
    input: InputBehavior,
}

enum LayoutItemKind {
    Pane(PaneId),
    Placeholder,
    Handle,
    Chrome,
    Overlay,
}
```

`Pane` items reference real panes from the pane inventory. Other item kinds use
stable layout item IDs so native input can report later pointer or keyboard
events back to the layout policy without needing to understand the policy's
internal tree.

## Coordinates

Resolved layout coordinates are normalized presentation coordinates. The useful
coordinate domain is `-1.0..=1.0` on each axis, with conversion to output pixels
performed by native code after validation.

Candidate Rust shape:

```rust
struct LocalPoint {
    u: f64,
    v: f64,
}

struct LocalTransform {
    /// Top-left normalized presentation coordinate.
    offset: LocalPoint,
    /// Normalized width and height. A full-content item uses `2.0, 2.0`.
    size: LocalPoint,
}
```

Native code should provide the transform algebra: containment, intersection,
parent-to-child projection, child-to-parent projection, clipping, and
normalized-to-pixel conversion. That keeps coordinate behavior consistent across
rendering, input, capture, and diagnostics.

In the current host contract, `offset=(-1.0, -1.0), size=(2.0, 2.0)` covers the
full content presentation area. `offset=(0.0, -1.0), size=(1.0, 2.0)` covers the
right half. Native-owned presentation chrome such as reserved panels can still
occupy space outside that content area before the transform is projected to
pixels.

## Hit Testing

Hit testing is derived from the accepted resolved layout. It should not be a
method on the WASM layout policy or on the policy's source tree.

Candidate native API:

```rust
impl ResolvedLayout {
    fn hit_stack(&self, point: LocalPoint) -> Vec<HitResult>;
    fn top_hit(&self, point: LocalPoint) -> Option<HitResult>;
}

struct HitResult {
    item: LayoutItemId,
    kind: LayoutItemKind,
    local_point: LocalPoint,
}
```

The native implementation can walk items in reverse paint order or by sorted
`z`, returning every item that contains the point. Pointer forwarding can use
`top_hit`, while debugging, drag/drop, scroll pass-through, and focus policy can
use the full hit stack.

## Validation

Native validation must reject or repair candidate layouts that would make the
presentation unsafe or incoherent.

Validation should check:

- finite coordinates, transforms, and `z` values
- normalized rects with `min <= max`
- item IDs that are stable and unique within the resolved layout
- pane references that exist in the current pane inventory
- invalid duplicate pane placement unless the item kind explicitly allows it
- input behaviors that match the item kind
- items outside allowed bounds unless overflow is explicitly supported
- excessive item counts or oversized serialized responses

Validation failures should be observable in diagnostics. The host should fall
back to a deterministic native resolved layout rather than applying invalid
candidate data.

## Events

Native layout events should describe why a new resolved layout is needed without
dictating the policy's internal model.

Candidate events:

```rust
enum LayoutEvent {
    PaneAdded { pane: PaneId },
    PaneRemoved { pane: PaneId },
    PresentationResized { constraints: LayoutConstraints },
    PointerPressed { hit: Option<HitResult> },
    PointerDragged { item: LayoutItemId, from: LocalPoint, to: LocalPoint },
    PointerReleased { hit_stack: Vec<HitResult> },
    Command { command: LayoutCommand },
}
```

A split resize is not a native primitive in this model. It is a pointer event on
a resolved handle item. WASM can interpret that handle as a split divider, tab
bar, floating window edge, or anything else and return a new resolved layout.

## Fallback Rule

Manifold must always have a native fallback that can produce a valid resolved
layout from pane inventory and presentation constraints.

The fallback should be simple and deterministic. It does not need to model every
WASM policy feature; it only needs to keep panes visible enough for recovery and
preserve session usability when policy code fails.

The native fallback may keep a small session-local content layout so users can
recover without a layout module. In that fallback, panes still live in the
session inventory and split slots are pointers into that inventory. Removing or
cycling a split changes the slot state, not pane ownership, and switching surface
sessions restores that session's own slot layout.
