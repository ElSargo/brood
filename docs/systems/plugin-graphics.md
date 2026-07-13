---
status: target
type: system
authority: product
last-reviewed: 2026-07-13
---

# Plugin Graphics

Manifold provides presentation actors with a low-level, backend-independent
graphics protocol and a rich SDK above it. Simple interfaces should require
little code, while custom applications retain access to buffers, textures,
arbitrary shaders, geometry, render targets, and draw commands. Render
submission, validation, resource ownership, and WGPU lowering occur in the
presentation client, so the interactive actor-to-frame path does not cross the
backend connection. See [[presentation-client]] for actor placement and client
lifecycle.

The protocol is WebGPU-like in what it can express, not in who owns the GPU.
Actors describe pane-local rendering. The presentation client owns the physical
device, GPU synchronization, swapchains, final composition, frame pacing, and
recovery. WGPU and WGSL are the target client implementation, but WGPU Rust
objects and backend-native Vulkan, Metal, Direct3D, EGL, or DMA-BUF objects are
never part of the plugin ABI.

## API Layers

The product exposes complementary levels rather than choosing one abstraction
for every application:

1. Host-rendered components for ordinary application UI, native behavior,
   accessibility, theme integration, and text editing.
2. Canvas and retained-scene SDKs for text, images, paths, gradients, clipping,
   transforms, and common effects.
3. Render packets for custom pipelines, buffers, textures, samplers, render
   targets, shaders, and indexed or instanced drawing.
4. Explicitly granted surface and buffer import paths for managed clients and
   specialized producers.

The high-level SDK does not limit the low-level protocol. Components retain
semantic information for the host rather than becoming opaque drawing commands,
but host component rendering and actor custom drawing ultimately share the
same presentation machinery.

## Ownership Boundary

Actors own:

- pane-local visual and application state;
- shader source and pipeline descriptions;
- geometry, image, uniform, and effect data;
- resource creation and update requests;
- the latest submitted visual state for each pane they produce.

The host owns:

- physical adapters, devices, queues, command encoders, and swapchains;
- barriers, fences, hazard tracking, and resources retained by in-flight work;
- pane clipping, final composition, color conversion, and presentation;
- frame scheduling, damage tracking, and replay after presentation reattach;
- shader validation and backend compilation;
- resource limits, device-loss recovery, and diagnostics.

The actor API therefore has draw calls but no queue selection, presentation
operation, explicit barrier, fence wait, or backend framebuffer management.

## Render Packets

A **render packet** is a bounded, self-contained command buffer describing the
latest custom-drawn visual state of one pane. The hot WASM boundary is
conceptually:

```text
render_submit(pane_handle, guest_address, byte_length) -> submit_result
```

The actor writes a packet into a contiguous range of its linear memory, calls
`render_submit`, and continues. Submission never waits for GPU execution.

An accepted submission atomically replaces the pane's previous render packet.
The host owns the accepted bytes and may lower or replay them whenever the pane
must be presented. This means:

- the actor may reuse its guest memory immediately after submission returns;
- the pane can redraw while its actor is dormant;
- detach and reattach do not require the actor to regenerate unchanged content;
- intermediate packets may be superseded by a newer generation;
- an invalid packet cannot partially change pane content.

The guest address is valid only for the duration of the host call. The host
must copy or synchronously consume the range before returning and must never
retain a pointer into ordinary guest memory. The default contract copies the
packet because immediate guest-memory reuse is more valuable than speculative
zero-copy complexity. A future borrowed-memory mode would require explicit
release completion and is a separate contract.

A pane handle carries authority to update a particular pane. It is not a
forgeable numeric pane ID. The host verifies that the submitting actor
generation owns or has been granted that pane's graphics producer role.

## Packet Encoding

The wire format is Manifold-owned, explicitly versioned, and independent of
Rust or C structure layout. It uses fixed-width little-endian fields and
relative offsets within the submitted range. Commands must not contain host
pointers or additional mutable guest pointers.

A packet contains:

```text
header
  magic and ABI version
  total byte length and command count
  pane-frame generation
  feature flags

commands
  opcode
  encoded command length
  payload

data section
  inline vertices, indices, uniforms, strings, and other small values
```

Length-tagged commands permit structural traversal without relying on native
layout. Version and feature negotiation determines whether an unknown command
can be skipped; otherwise the complete packet is rejected. Every offset,
length, alignment, enum, resource type, and arithmetic operation is checked
before acceptance.

Representative commands include:

```text
clear
set-pipeline
set-viewport
set-scissor
set-bindings
set-vertex-buffer
set-index-buffer
set-push-data
draw
draw-indexed
draw-text
draw-text-layout
begin-layer
end-layer
```

This list describes the command families, not a frozen opcode table. Exact
encodings belong in the eventual ABI specification and implementation.

## Submission And Replay Semantics

A render packet is a replayable pane display list, not a physical GPU command
buffer. It must not depend on the contents of mutable guest memory after
submission. Referenced persistent resources have defined snapshot and lifetime
semantics so later actor writes cannot race a replay.

The baseline replayable profile has no lasting draw side effects except its
final pane image. Buffers and textures referenced by draw bindings are
read-only, intermediate render targets are packet-local, and the final pane
target is replaced rather than loaded incrementally. Host-provided frame values
such as presentation time may vary during replay, allowing shader animation
without waking the actor. Persistent writable targets, feedback effects,
compute, and storage resources require a separately negotiated stateful profile
with execute-once, retained-output, loss, and resubmission semantics; they must
not silently change baseline replay behavior.

Submission performs bounded copying and structural validation. Expensive
shader compilation, backend pipeline creation, upload, and lowering happen
outside actor dispatch. The immediate result distinguishes at least:

```text
accepted(sequence)
rejected(structural_error)
queue-full
pane-unavailable
```

Runtime resource failures are delivered through actor completion or diagnostic
lanes. Presentation may continue using the last valid packet when a replacement
is rejected. Referencing a pending or failed resource never blocks submission;
the host follows a documented fallback, normally skipping affected draws while
retaining a diagnostic.

Packets carry a monotonically increasing pane-frame generation. The host may
coalesce accepted packets and render only the newest applicable generation,
but diagnostics retain enough sequence information to explain dropped,
rejected, and presented updates.

## Resources

Persistent resources avoid resending large or static data in every packet:

- buffers;
- textures and texture views;
- samplers;
- shaders and pipelines;
- bind groups or equivalent immutable binding sets;
- packet-local render-target descriptions and any negotiated stateful targets;
- text layouts, font selections, and image resources;
- immutable blobs used for substantial upload data.

Handles identify a resource type, table slot, and generation. A stale handle
cannot silently refer to a new resource, and a handle of one type cannot be
used as another type. Resources are actor-generation scoped unless an API
explicitly creates a broader session- or pane-scoped resource.

Actor replacement invalidates actor-generation graphics handles after in-flight
host work has been retired. Stable pane identity does not imply stable ownership
of the previous actor's GPU resources. The host can preserve the pane shell and
show recovery content while a replacement actor reconstructs its visual state.

Creation that may compile, decode, allocate substantially, or contact platform
resources is asynchronous:

```text
Actor -> Host: create resource -> pending handle or operation
Host -> Actor: resource-ready(handle) | resource-failed(handle, diagnostic)
```

Destruction releases actor ownership promptly but does not force a GPU wait.
The host retains internal objects until all accepted packets and in-flight work
that reference them are retired.

## Uploads And Dynamic Data

Small dynamic vertices, indices, uniforms, push data, and strings may live in
the packet data section. Large textures, static geometry, and repeatedly used
content use persistent resources or immutable upload blobs instead of being
copied with every frame.

The initial API favors explicit copies with simple ownership. Host-owned upload
arenas, shared-memory borrowing, or other zero-copy paths may be added only
with a complete reuse, cancellation, actor-replacement, and release protocol.
No optimization may allow the host to read guest-owned bytes after the actor is
permitted to mutate them.

## Shaders And Pipelines

Custom shaders are a supported escape from the standard component and canvas
SDKs, not an accidental backend leak. The portable ABI uses a validated,
backend-independent resource model and WGSL shader source. The required
baseline is the common Manifold WGPU/WebGPU profile; optional features are
negotiated through Manifold feature identifiers rather than native backend
names.

Pipelines declare shader stages, vertex layouts, bindings, blend behavior,
raster state, and target requirements without exposing a native driver object.
The host validates resource access and compiles pipelines asynchronously.

Pane-local drawing and packet-local transient render targets form the baseline.
Compute, persistent writable targets, storage resources, readback, external
memory, and imported GPU surfaces are separately negotiated features with
explicit execution semantics, limits, and capabilities. Arbitrary shaders
do not grant control over host composition or access to another pane's
resources.

## Host Text

Text is a first-class host graphics facility. Plugins should not need to ship a
shaper, discover operating-system fonts, maintain a glyph atlas, or reproduce
platform fallback merely to draw a label.

The host provides:

- font discovery, selection, and fallback;
- shaping, bidirectional text, script handling, and line breaking;
- measurement, wrapping, truncation, and alignment;
- cached text layouts and glyph resources;
- caret, selection, and position-to-text hit testing;
- integration with accessibility and host text-editing primitives.

Simple packets may contain UTF-8 text, bounds, and style data directly. Apps
that need stable measurement or repeated rendering use persistent text-layout
handles. Advanced drawing may use positioned glyph data where supported, but
opaque glyph drawing does not replace the semantic text and editing paths
needed for accessible UI.

Font and text resources follow the same asynchronous and generation-scoped
lifecycle rules as other resources. The SDK exposes convenient synchronous
operations only where the result is already local and bounded; operations that
require host work produce completion events or cached handles.

## Components, Input, And Accessibility

Native components remain the default for conventional application UI. They
carry roles, labels, actions, focus behavior, editable state, and theme intent
that cannot be recovered reliably from triangles.

A custom-drawn pane may submit a companion semantic interaction tree containing
stable element IDs, bounds, roles, actions, and accessibility metadata. The
presentation client owns final pane hit testing and input routing, while the
actor receives semantic or low-level pane input according to its selected API.
Visual packet replacement and semantic-tree replacement use generations so the
client does not silently combine incompatible states.

Native pane chrome remains outside actor rendering. A shader or full-pane draw
cannot overwrite client-owned borders, recovery UI, menus, or overlays unless a
separate presentation capability explicitly allows that composition role.

## Scheduling And Actor Integration

`render_submit` is a bounded actor operation. It does not suspend dispatch,
wait for a worker, compile a shader, upload to the GPU, or wait for a frame.
Workers may prepare packet bytes or resource data in actor-owned shared memory;
the actor performs the authoritative pane submission and then returns.

Graphics submissions use latest-state semantics rather than an unbounded
mailbox backlog. Lifecycle, resource-ready, failure, and presentation-feedback
events use the normal reserved actor lanes. Rendering is not a synchronous
policy provider and cannot run on the immediate key or layout decision path.

## Limits And Diagnostics

Feature discovery reports backend-independent capabilities and explicit limits,
including:

- maximum packet bytes and command count;
- maximum draws, instances, bindings, layers, and clipping depth;
- inline and persistent resource sizes;
- texture formats, dimensions, and sample counts;
- shader stages and optional compute or storage features;
- text, glyph, and semantic-node limits.

Portable SDK behavior targets a required baseline. Optional features are tested
through Manifold feature identifiers rather than backend or driver names.

Diagnostics expose:

- accepted, superseded, rejected, lowered, and presented packet sequences;
- copy, validation, queue, lowering, GPU, and presentation timing;
- packet bytes, command counts, draws, resource references, and damage;
- pending, failed, live, and retained resource counts and memory estimates;
- shader and pipeline diagnostics in actor-source terms;
- fallback use, device loss, replay, and recovery state.

Limits protect scheduler and renderer coherence from bugs and resource
exhaustion. They are not presented as a security boundary against an installed
plugin trusted by the session.

## SDK Expectations

The SDK builds convenient retained and immediate APIs above the protocol:

```text
components: column, list, button, editor
canvas: fill-rounded-rect, path, image, draw-text
render: set-pipeline, bind, draw-indexed
```

The common path batches work in guest memory and crosses the WASM boundary once
per pane update rather than once per draw. SDK allocation, packet encoding,
resource caching, generation tracking, and error reporting should be available
to applications that do not need to manage them directly.

The protocol must be proven with materially different workloads before its
baseline is frozen:

- a settings or session-switcher interface;
- a terminal or code editor with shaping, selection, and scrolling;
- a vector drawing application;
- a shader-heavy visualization or small game.

The first workload must remain concise, while the latter workloads must not
require private renderer access.

Related:

- [[presentation-client]]
- [[plugin-ui]]
- [[plugin-runtime]]
- [[plugin-io-and-resources]]
- [[../concepts/pane]]
- [[../concepts/plugin]]
- [[../concepts/presentation-surface]]
