---
status: target
type: system
authority: product
last-reviewed: 2026-07-21
---

# Future Advanced Plugin Graphics

This page records the future escape hatch for plugins that cannot be expressed
efficiently through the base canvas display list in [[plugin-graphics]]. It is a
separate protocol direction, not part of the initial graphics ABI and not a
requirement for ordinary SDK components.

Candidate workloads include shader-heavy visualizations, games, custom geometry,
specialized image processing, and effects that need explicit intermediate
targets. The protocol should be designed only after the presentation client's
WGPU renderer and the base canvas path have been proven with real applications.

## Relationship To The Base Canvas

Canvas remains the default pane graphics contract. Advanced graphics produces a
pane-local image or layer which the client composes through the same clipping,
layout, color, scheduling, and recovery machinery. Adding advanced support must
not change the meaning of existing canvas packets or require canvas SDKs to
understand GPU resources.

An application may use either protocol or compose an advanced layer with canvas
content. The exact composition handle and synchronization rule remain to be
proven; it should not be represented by silently adding GPU commands to the
canvas opcode table.

## Intended Capability

A future advanced profile may expose host-managed descriptions of:

- user-defined vertex and index geometry;
- immutable and dynamic buffers;
- textures, texture views, and samplers;
- WGSL shaders and render pipelines;
- bindings and uniform data;
- indexed and instanced drawing;
- pane-local intermediate render targets;
- explicitly negotiated compute, storage, or readback.

This is WebGPU-like in expressive power, not direct WebGPU ownership. The
presentation client continues to own the physical adapter, device, queues,
command encoders, swapchain, barriers, synchronization, pane target, final
composition, frame pacing, and recovery. WGPU Rust objects and native Vulkan,
Metal, Direct3D, EGL, or DMA-BUF objects never become plugin handles.

## Likely Shape

The protocol should retain the useful ownership rules from the canvas path:

- bounded versioned packets copied out of guest memory;
- typed actor-generation resource handles;
- asynchronous shader compilation and substantial resource creation;
- no wait for GPU execution inside actor dispatch;
- atomic latest-state replacement where replay semantics apply;
- explicit limits, diagnostics, teardown, and device-loss behavior.

A replayable first profile should use immutable resource revisions and a single
implicit pane output. It may contain commands such as pipeline and binding
selection, viewport and scissor state, vertex/index bindings, and draw or
draw-indexed operations. Unknown commands reject the complete packet.

## Deliberately Not In The First Advanced Profile

The following features introduce a second, stateful execution model and should
not be inherited merely because WGPU can express them:

- persistent writable targets and feedback loops;
- storage buffers and textures;
- general compute dispatch;
- GPU readback;
- external memory or imported client surfaces;
- guest-visible queues, barriers, fences, or timeline synchronization;
- direct access to another pane's resources or the final compositor target.

If later workloads justify these features, their execute-once, replay, mutation,
loss, cancellation, and resubmission semantics must be specified explicitly.
They must not weaken the deterministic replay behavior of the base canvas or an
immutable advanced profile.

Imported managed-client surfaces are a separate presentation transport concern,
not evidence that arbitrary plugins should receive native graphics handles.

## Open Design Work

Before an ABI is proposed, implementation spikes must determine:

- the smallest resource and draw vocabulary needed by real workloads;
- the portable WGPU/WebGPU feature and limit baseline;
- WGSL validation, compilation isolation, caching, and diagnostics;
- how an advanced output composes with canvas content;
- whether dynamic buffer updates need immutable revisions or an explicitly
  leased upload path;
- frame-demand and host-time semantics for shader animation;
- recovery after actor replacement and WGPU device loss.

## Proof Before Freezing

The protocol should not be frozen from an opcode inventory alone. At minimum,
prove:

- custom indexed or instanced geometry;
- a user-defined WGSL visualization;
- resource creation, replacement, and teardown under churn;
- animation without backend traffic or an unbounded frame queue;
- actor replacement and device recreation with clear recovery output;
- parser fuzzing and replay on the supported native WGPU and web targets.

Related:

- [[plugin-graphics]]
- [[plugin-ui]]
- [[presentation-client]]
- [[plugin-runtime]]
