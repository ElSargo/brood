---
status: target
type: system
authority: product
last-reviewed: 2026-07-25
---

# Plugin Graphics

The base plugin graphics API is a bounded canvas display list. A presentation
actor records pane-local drawing commands into a packet and submits that packet
to the presentation client. The client copies and validates the packet, retains
the latest accepted packet for the pane, and lowers it into its WGPU renderer.
The backend is not on this interactive path.

The base protocol is deliberately not a GPU API. It has paths, rectangles,
images, text, transforms, clips, layers, and compositing. It has no user-defined
geometry, shaders, pipelines, bind groups, render targets, queues, barriers, or
fences. Those belong to the separate future [[plugin-advanced-graphics]]
protocol.

## Boundary

The presentation actor and its SDK own:

- application and widget state;
- component layout, styling, and event handling;
- hit testing within the actor's own UI model;
- packet construction and reusable SDK-side caches;
- the decision to submit a new visual state.

The presentation client owns:

- packet copying, validation, retention, and replay;
- native text shaping, font selection, measurement, and glyph rendering;
- host graphics resources referenced by packets;
- pane clipping, final composition, color conversion, and presentation;
- frame scheduling, damage tracking, limits, diagnostics, and renderer recovery.

The host has no component or widget ABI. An SDK `button`, `list`, or `editor`
function is guest code which updates guest state and emits ordinary canvas
commands. The host sees those commands, not the component that produced them.

## Draw Packets

A **draw packet** is a bounded, versioned sequence of canvas commands describing
the latest visual state of one pane. The stable Component Model API is
conceptually:

```text
submit_draw_packet(pane, packet_bytes) -> submit_result
```

The packet bytes are passed as an owned or borrowed byte list through the
Component Model ABI. The host copies them before returning. A raw guest address,
host pointer, Rust object layout, WGPU object, or pointer retained into guest
memory is never part of the product contract.

An accepted packet atomically replaces the pane's previous packet. This gives
the path latest-state rather than queue semantics:

- the actor may immediately reuse its encoding memory;
- an unchanged pane can redraw while its actor is dormant;
- detach, expose, resize fallback, and renderer recovery can replay accepted
  CPU-side state;
- intermediate submissions may be superseded;
- rejection leaves the previous packet intact.

Acceptance is the final per-packet state, not admission to later validation.
Every command and referenced resource is already valid and renderable when the
packet becomes active. Before replacement, the client performs the same bounded
path tessellation used by the renderer at its finest supported tolerance and
checks the resulting vertex and index budgets. WGPU lowering does not later
reject an individual accepted packet. Device loss, allocation failure, or
another renderer-wide failure enters client renderer recovery and replays the
accepted CPU-side scene; it is not reported as delayed packet rejection.

The host assigns the submission sequence used for diagnostics. An actor may
also include its own view revision for correlating input, drawing, and optional
semantic metadata, but it does not choose the host sequence.

## Packet Encoding

The packet codec is Brood-owned and independent of Rust or C structure
layout. It uses fixed-width little-endian fields, relative offsets, and explicit
lengths. A packet contains a small header, a command sequence, and optional
inline data such as path points, colors, transforms, and short strings.

The host validates the complete packet before acceptance, including:

- total bytes and command count;
- command lengths, offsets, alignment, and arithmetic;
- finite coordinates, transforms, and colors;
- balanced saved state and layers;
- save/restore-aware clip depth and nested layer depth;
- bounded tessellated vertex and index output;
- resource type, generation, ownership, and readiness;
- supported enum values and packet version.

Unknown commands reject the whole packet. Version negotiation chooses a packet
version understood by both SDK and host; the renderer never guesses whether an
unknown state-changing command is safe to skip.

## Canvas Commands

The base vocabulary covers ordinary two-dimensional pane drawing:

```text
save
restore
set-transform
set-opacity
set-blend-mode
clip-rect
clip-path
fill-rect
stroke-rect
fill-rounded-rect
stroke-rounded-rect
fill-path
stroke-path
draw-image
draw-text-layout
begin-layer
end-layer
```

Exact opcodes and field encodings remain implementation details until workload
testing freezes the ABI. The important contract is canvas-like behavior rather
than GPU pipeline state. Drawing commands carry their paint and stroke
parameters directly or reference a simple immutable paint; transforms, clips,
opacity, and compositing use the bounded saved-state stack.
Filled and clipped paths use the canvas-standard nonzero winding rule.

Coordinates are pane-local logical units. The host supplies pane size and scale
events, clips every packet to the pane content area, and maps logical drawing to
the physical target. Each retained canvas target has the physical extent of its
accepted pane rectangle, not the extent of the presentation window. A layout
commit or output-scale change replays the retained packet into that target and
delivers the new pane-local logical size to the actor. Composition may translate
and clip the result, but does not independently stretch its axes to fit a pane.

The base profile defines one color and alpha convention so SDKs do not branch on
the active WGPU backend. Packet colors and copied RGBA8 image inputs are
straight-alpha sRGB. The host converts them to linear-light premultiplied values
before filtering or blending; opacity, layer composition, and every blend mode
operate in that linear premultiplied space. Presentation converts the final
linear result to the output color space. Fully transparent colored texels
therefore cannot bleed encoded RGB into filtered edges. Native pane chrome,
menus, recovery UI, and presentation overlays remain outside the packet's
drawable area.

## GPU Lowering

The presentation client lowers an accepted packet to a retained GPU scene. Path
curves and strokes are tessellated to bounded vertex and index data when the
packet changes; this is geometry preparation, not CPU rasterization. Curve
tolerance accounts for the current transform and pane scale, has a bounded
lower limit, and is preflighted against the scene geometry budget before packet
acceptance. Images, native text output, and other immutable resources are
uploaded on first use and reused while retained across replacement packets
which reference the same handles.

Repainting records the scene's ordered render passes, copies, and draw calls into
the presentation frame's WGPU command encoder. Pane drawing, layer composition,
image sampling, clipping coverage, blending, and final composition therefore run
on the GPU and are submitted with the rest of the frame as one command buffer.
The normal repaint path does not allocate a full-size CPU pixel buffer, rasterize
the pane on the event-loop thread, vertically copy it, or upload the whole pane
after every input event.

Canvas state remains a lowering concern rather than protocol-visible GPU state:

- transforms are applied while producing pane-local geometry;
- nested clips use a bounded stencil representation and restore with saved
  canvas state;
- layers reuse transient render targets by active nesting depth, so sequential
  layers do not grow target allocation;
- source-over, additive, and directly representable blend modes use fixed GPU
  blending;
- blend modes which read the destination use a bounded target snapshot and a
  host shader selected by the canvas blend mode;
- user-provided shaders, pipelines, or command buffers are never accepted by the
  base protocol.

Canvas geometry and clip coverage use multisample antialiasing before each
target is resolved for sampling or final composition. This keeps ordinary path
and rounded-rectangle edges canvas-quality without introducing a CPU coverage
buffer.

Static accepted scenes retain their GPU output. Expose and composition-only
frames reuse that output without replaying the packet. A replacement packet or
size/scale change rebuilds the necessary geometry and records a new GPU repaint.
Device recovery recreates GPU resources from the accepted packet and immutable
host resource copies.

## Graphics Resources

Packets may contain small values inline and may reference a small set of
host-managed immutable resources:

- decoded images or copied pixel images;
- reusable paths;
- gradients or paints where retaining them is measurably useful;
- native text layouts.

Handles are typed and scoped to one pane and actor generation. Stale handles
cannot resolve to new resources, actors cannot use another pane's resources,
and one resource type cannot be used as another. Non-text resources referenced
by a packet must already be ready. A missing, failed, stale, wrong-owner, or
wrong-type resource rejects the complete packet rather than skipping one draw.

Native text layouts are allowed to be pending. The client accepts and retains a
packet that references pending text, but does not replace the pane's currently
presented scene until every text dependency of the replacement is ready. The
actor submits the same packet regardless of cache state; readiness and redraw
scheduling are host concerns. A newer accepted packet supersedes an older
pending replacement.

Resource creation copies the source data needed by the host. Updating a
resource creates a new immutable revision or handle; previously accepted packets
continue to see the version they retained. The actor explicitly drops obsolete
handles after submitting the replacement packet. A handle still referenced by
an accepted packet cannot be dropped. Ending the owning actor generation drops
all of its resources without forcing a GPU wait: the client releases internal
objects after accepted packets and in-flight rendering stop referencing them.

The base protocol does not add shared upload arenas or borrowed mutable buffers.
They can be considered only after explicit copying is shown to be a real
bottleneck and a complete reuse, cancellation, replacement, and release protocol
has been demonstrated.

## Native Text Service

Text is the intentional host-level facility in the base graphics model. Correct
font discovery, fallback, shaping, bidirectional layout, line breaking, glyph
caching, and platform text behavior are too complex and platform-specific to
require every plugin SDK to reproduce them.

Text layout is a presentation-client host function outside the draw packet:

```text
layout_text(text, font_intent, bounds, wrapping, alignment, style)
  -> text_layout_handle + width, height, cell_width, line_height
```

Position-to-text, text-to-position, selection, and caret queries are also host
functions operating on that immutable layout handle. The packet only uses
`draw-text-layout`; it contains no text-shaping request or alternate inline text
path.

`layout_text` performs only the bounded CPU layout needed to return the handle
and metrics. It records the positioned glyph dependencies, queues cache misses
to persistent raster workers, and returns without rasterizing the complete text
surface. Raster workers retain their glyph caches across layouts. A submitted
draw packet may therefore depend on pending glyphs; the client captures it and
publishes the replacement scene only after all of those dependencies are ready.
Ready glyph images are packed into renderer-wide, incrementally allocated GPU
atlas pages with transparent padding. Text lowering emits atlas UVs, and
adjacent glyphs on the same page and compatible blend state are coalesced into
one indexed GPU draw.

An SDK may expose a convenient `draw_text(...)` function, but that function
creates or retrieves a cached host text layout and emits `draw-text-layout`.
Measurement and rendering therefore always use the same native layout result.

`font_intent` includes an explicit monospace choice. For that intent the client
selects the platform monospace family, disables discretionary shaping features
that would destabilize a cell advance, and reports a stable `cell_width` and
`line_height` measured by the same layout engine used for drawing. Style
includes at least normal and bold weight plus foreground color. A terminal can
therefore group adjacent cells into styled text runs while keeping its grid,
parser, cursor, and cell model in guest code.

Text calls are local to the presentation client and bounded by explicit string,
line, and layout limits. The client may cache native layouts internally, but the
observable handle and metrics remain backend-independent. Font, toolkit, glyph
atlas, and WGPU texture objects never cross into the plugin ABI.

The host text service does not make editors, labels, or text fields host
components. SDK code owns their behavior and uses text metrics and canvas
commands to implement them.

## Components And Input

Components are ordinary SDK functions layered above canvas primitives:

```text
button(...)     -> guest state + rectangles + text layout
list(...)       -> guest layout + clips + rows + input mapping
editor(...)     -> guest document state + text layouts + selection drawing
```

The presentation client delivers pane resize, focus, pointer, keyboard, text,
IME, visibility, and frame events. The actor or SDK maps pane-local input onto
its own component state and submits a replacement packet when the visible state
changes. Brood does not retain a component tree, patch widget nodes, preserve
widget identity, or translate backend widget events.

Accessibility is orthogonal to component implementation. A later or optional
semantic channel may describe roles, bounds, labels, values, and actions for
platform accessibility projection. SDK components can emit that description,
but it is not the render tree and does not make the component itself a host
object.

## Scheduling

Submission is bounded and does not wait for GPU execution. The actor returns
after the host has copied and validated the packet. Rendering and presentation
happen on the client renderer's normal frame path.

Static panes are redrawn from their retained packet only when damaged, exposed,
resized, or recomposed. An actor that needs animation requests a frame event,
advances its state, submits a new packet, and requests another frame if needed.
Hidden panes do not receive an unbounded frame stream. More specialized
host-driven shader animation belongs to [[plugin-advanced-graphics]], not the
base canvas contract.

## Limits And Diagnostics

Feature discovery reports backend-independent canvas and text capabilities plus
explicit limits for packet bytes, command count, path complexity, clip/layer
depth, image dimensions and bytes, text length, and live resources. Limits are
generous fault-containment ceilings, not normal workload targets.

Diagnostics correlate actor view revisions with accepted, rejected,
superseded, replayed, and presented host sequences. They report validation and
render timing, packet and resource sizes, fallback/recovery state, and text or
resource errors in SDK-facing terms.

Text diagnostics include layouts created, dropped, currently live, and peak
live, alongside maximum packet bytes and command count. Run caches must release
obsolete layout handles as revisions change; churn is measured directly rather
than hidden behind an unbounded host cache.

## Proof Before Freezing

The base protocol should be frozen only after real client-side WASM workloads
exercise materially different uses of the same canvas contract:

- a session switcher built from SDK component functions;
- a scrolling terminal or editor using native text layouts and selection;
- a vector drawing workload using paths, transforms, clips, and layers;
- actor replacement and forced renderer recreation while retained packets exist.

The session switcher should remain concise and must not require a host component
tree. Advanced shader or game workloads validate [[plugin-advanced-graphics]],
not this base protocol.

Related:

- [[plugin-ui]]
- [[plugin-advanced-graphics]]
- [[presentation-client]]
- [[plugin-runtime]]
- [[plugin-io-and-resources]]
- [[../concepts/pane]]
- [[../concepts/plugin]]
