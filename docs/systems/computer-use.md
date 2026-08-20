---
status: target
type: system
security-boundary: process-and-capability
runtime-boundary: server
authority: product
last-reviewed: 2026-08-17
code-links:
  - crates/broodd/src/frame_export.rs
  - crates/broodd/src/input.rs
  - crates/broodd/src/control.rs
  - crates/broodd/src/bin/brood-computer-use-mcp.rs
  - agents/brood-agent/src/computer-use.mjs
  - crates/broodd/src/debug_dump.rs
  - crates/brood-wasm/wit/brood-plugin.wit
  - plugins/coding-agent/src/lib.rs
---

# Computer Use

## Objective

Brood computer use lets an agent observe and control applications hosted inside
Brood sessions. The complete observe, decide, act, and verify loop runs beside
the session compositor in `brood-sessiond`; it does not depend on a connected
presentation client.

The public interaction model deliberately follows OpenAI Codex computer use.
It provides both direct computer actions and persistent code execution so a
model can perform a short action interactively or express a long sequence as a
compact script.

Host-desktop control is out of scope. Client-local plugin panes are also absent
from the initial visual computer because they do not exist in the server's
Wayland scene. Plugins may later expose structured tools without changing this
boundary.

## Product invariants

- Computer use can observe and control only managed applications authorized for
  its Brood session.
- Capture, input injection, action ordering, settling, and script execution are
  server-side operations.
- The facility remains usable with zero presentation clients connected.
- A presentation client may observe progress, request approval, pause, or stop
  a run, but it is not in the control loop.
- Direct actions use the Codex-compatible vocabulary: `click`, `double_click`,
  `scroll`, `type`, `wait`, `keypress`, `drag`, `move`, and `screenshot`.
- Scripts use a persistent task-scoped runtime and call the same underlying
  capture and input operations as direct actions.
- Generated code has no ambient filesystem, network, process, DBus, or host
  desktop authority.
- Every externally visible action has a task identity, sequence number, target
  session, lease generation, timestamps, and result.

## Architecture

```mermaid
flowchart LR
  M["Model or agent adapter"] --> L["Computer-use task loop"]
  C["Presentation client observer"] -. "approve, pause, stop" .-> L
  L --> X["Persistent script isolate"]
  L --> A["Ordered action executor"]
  X --> A
  A --> I["Smithay input injection"]
  I --> W["Session-managed Wayland apps"]
  W --> R["Server automation renderer"]
  R --> O["Sequenced observation"]
  O --> L
  O -. "status and frames" .-> C
```

`brood-sessiond` owns the task loop, the lease, and the automation renderer.
Adapters translate an external agent protocol into Brood's internal task and
action types. No adapter is authoritative for action semantics.

### Relationship to presentation

The automation viewport is not a human presentation surface. It is a fixed,
server-owned projection used only for machine observation, coordinate mapping,
and deterministic input. The presentation client continues to own human
layout, final composition, and platform-native interaction. Managed-window
size authority changes with the live computer-use lease:

- without a lease, the presentation client sends the logical size selected by
  its human layout;
- with a lease, the server publishes `server_preferred` size authority and its
  preferred logical size in the managed-window catalogue;
- when the lease ends, presentation authority returns and the client applies
  its current layout once.

Catalogue authority, not screenshot arrival, controls this transition. The
client aspect-fits each server-sized managed frame inside that pane's existing
presentation rectangle. Letterbox space uses presentation-owned generic
background chrome; it never changes the pane allocation. Passive capture and
debug dumps never configure an application's size.

The existing presentation design says the server never creates a native or
offscreen presentation target. Implementing computer use requires narrowing
that invariant: the server still does not create a *human presentation target*,
but it does create an isolated automation render target. The presentation
protocol must not acquire automation-layout authority as a side effect.

Automation is a first-class managed-window catalogue state. Every automated
pane has a translucent blue border and the automation pointer is projected
into its aspect-fitted content rectangle. It enters interaction-locked: human
input addressed to that pane is ignored by both presentation and server, and a
translucent overlay says `press esc to unlock`. Escape changes only the
interaction lock; it does not resize the window, release the lease, or stop the
task. Once unlocked, human and automation input may reach the pane while the
automation state remains visible. When the lease ends, the pane returns to the
normal state and all automation chrome disappears.

State/history and the newest compressed observation still travel as separate
bounded observer updates when needed, and slow clients retain only the newest
frame. The presentation observer does not select automation windows, change
machine coordinates, or request a new capture cadence. A paused task has no
size authority unless it reacquires a lease.

## Automation viewport

The first viewport profile is a fixed 1440 by 900 logical-pixel canvas. It
contains only server-managed Wayland applications from the authorized session.
The server owns its active window, z order, clipping, coordinate transform, and
focus independently from every connected client.

The initial layout should favor determinism over desktop fidelity:

1. One active managed window fills the usable viewport while preserving its
   configured content size where practical.
2. Switching the active window changes the automation projection, not the
   human client's layout.
3. Window metadata remains available so future tools can select a target by
   stable managed-pane or application identity.

An observation contains the original PNG bytes plus viewport size, task and
sequence identity, active-window identity, visible-window inventory, and settle
state. Capture must work for both DMA-BUF and SHM-backed application buffers.
Screenshots are generated from server-owned scene state; debug-dump paths are
not the production API.

## Direct action execution

An action request contains one or more actions. The server validates the entire
batch against the current lease and then executes valid actions in order on the
session event loop. It returns per-action results followed by a causally newer
observation.

Input reuses the compositor's existing keyboard, pointer, button, axis, and
focus paths. The executor adds automation-specific state for held modifiers,
button cleanup, drag interpolation, coordinate validation, cancellation, and
provenance. Cancellation must release every key and button held by the task.

`wait` is not an arbitrary server sleep. It yields the event loop until the
requested interval and then participates in the same settle-and-observe path.
Settling initially combines a short quiet period with bounded timeouts and
frame/input sequence markers. Later application-specific readiness signals may
improve it without changing the tool contract.

## Code execution

The code tool is named `exec_js`. Each computer-use task receives one
persistent JavaScript global environment so variables and helper functions
survive between calls. A call returns console output, displayed images, a
structured value, execution metrics, and any exception.

The preferred implementation is QuickJS hosted inside a dedicated WASM
isolate. The isolate receives explicit, narrow host capabilities rather than
direct access to Brood internals. Its initial globals are:

- `computer.screenshot()` and `display(image)`;
- direct action helpers matching the action vocabulary;
- `computer.perform(actions)` for ordered batches;
- `computer.windows()` and `computer.activeWindow()`;
- `computer.accessibilitySnapshot()` when an accessibility provider exists;
- `page` when the active application has an authorized Chromium DevTools
  binding;
- `askUser(prompt)` for an approval or information checkpoint;
- `console` and task-scoped `globalThis`.

Every asynchronous helper marshals a typed command to the session event loop.
The JavaScript engine never holds compositor objects or bypasses lease and
policy checks. CPU time, wall time, memory, output, image count, and queued host
calls are bounded. Terminating a task destroys its isolate and cancels its
outstanding calls.

## Semantic adapters

Pixels and input are the universal fallback. Two optional server-side semantic
providers make scripts faster without changing what the session controls:

- AT-SPI exposes native application accessibility trees and actions.
- Chromium DevTools exposes DOM and browser automation for managed Chromium
  applications.

Both providers resolve nodes back to the task's authorized session and active
window. A semantic handle is revisioned and becomes invalid when its backing
window, document, or accessibility tree changes.

## Lease and human interaction

The first implementation uses one daemon-wide computer-use lease because the
current compositor seat and focus state are shared. The lease names the task's
authorized session and window set even though exclusivity is daemon-wide.

A connected presentation client can observe without holding the automation
lease. Automated windows begin interaction-locked; Escape explicitly unlocks
human input without transferring layout authority. Human input elsewhere may
still pause or revoke the lease according to policy. Approval-required actions
block at the server and publish a request to all eligible observers.
The server applies the decision even if the requesting client disconnects.

## Internal protocol

The implementation should define transport-neutral domain types before adding
external adapters:

- `ComputerUseTask`: identity, principal, scope, policy, state, and limits;
- `ComputerUseLease`: generation, authorized session/windows, and expiry;
- `ComputerActionBatch`: task, expected sequence, actions, and approval proof;
- `ComputerActionResult`: ordered status, timing, and failure classification;
- `ComputerObservation`: PNG, metadata, sequence, and settle evidence;
- `ComputerScriptRequest` and `ComputerScriptResult`;
- `ComputerApprovalRequest` and `ComputerApprovalDecision`.

The initial external adapter should model the OpenAI computer-call loop closely
enough that a trained model can use its familiar action and screenshot shapes.
An optional MCP compatibility surface can expose the same direct-action and
`exec_js` tools. DBus supports local diagnostics and observer UX only; built-in
agent computer use does not depend on it and DBus is not a remote streaming
protocol.

The built-in agent receives ordinary Responses function tools for discovering
existing windows, launching managed applications, starting tasks, performing
actions, running persistent scripts, and managing the task lifecycle. Window
discovery is presented first and returns only windows from the agent pane's
surface session. Launch guidance directs the agent to reuse a suitable existing
window before creating another one. Their hidden transport is a private,
mode-0600 Unix socket created by the owning `brood-sessiond` and inherited only
by session-scoped children. The socket admits only computer-use, scoped launch,
and launch-status requests. It has no input, capture, lease, or approval
authority of its own. Presentation observers use the bounded actor-message lane
and cursor-addressed updates, so disconnecting one observer never removes task
history or observations from the daemon.

## Built-in agent integration

Every turn launched from a built-in, session-scoped coding-agent pane receives
native computer tools automatically. The pane supplies the current
surface-session identity and stable agent identity; the independent headless
agent binds those values to the inherited native tool channel and rejects tasks
or actions that attempt to cross the bound session. Users do not configure a
tool server or copy task, lease, or session identifiers into their prompt.

The agent's developer instructions explain the observe-act-observe workflow and
lease sequencing. Observation PNGs are returned as native tool image inputs,
with compact metadata as text, so the model can inspect the hosted application
directly. The session-bound native tool can also request that the daemon
launch a GUI application through the normal hosted-child path. The request has
no shell interpretation, is bound to the pane's session, and does not succeed
until the child creates a managed window; an early exit is returned as a tool
error. This lets an agent start from an empty session without acquiring host
desktop authority. The agent owns only tasks created during that turn and
stops them when its input closes; the daemon remains authoritative for leases,
scope validation, approvals, human-input policy, and eventual expiry.

The coding-agent UI is still a client-local presentation pane. It can display
the conversation and spectate automation, but it is not part of the visual
computer, cannot assign automation geometry, and never grants access to the
host desktop or another Brood session.

## Implementation plan

### Phase 0: Freeze the contract

Define the domain types, state machine, action schemas, error taxonomy, limits,
and golden protocol fixtures. Amend the presentation-client invariant to
distinguish automation rendering from human presentation. Add a minimal fake
adapter that can drive contract tests without a model provider.

Exit criterion: action batches, observations, approvals, cancellation, and
script calls have versioned fixtures and explicit lifecycle transitions.

### Phase 1: Build server capture and automation layout

Extract reusable buffer import and readback from frame export and debug capture.
Add the 1440 by 900 automation scene, window selection, deterministic layout,
PNG encoding, metadata, and monotonically sequenced observations.

Exit criterion: a nested real application renders changing DMA-BUF and SHM
content, and server captures prove correct pixels and active-window identity
with no presentation client.

### Phase 2: Execute direct actions

Add the exclusive lease, ordered executor, coordinate validation, full direct
action vocabulary, input cleanup, settle detection, cancellation, and audit
events. Reuse the normal Smithay paths rather than adding an application-side
input channel.

Exit criterion: a nested workload is clicked, typed into, scrolled, dragged,
and shortcut-controlled; screenshots and application-visible results prove each
action, including cancellation and stale-lease rejection.

### Phase 3: Close the server-side control loop

Implement task orchestration, policy gates, observation streaming, approval
requests, pause/resume/stop, and a Codex-shaped direct-computer adapter. Add an
observer surface for clients without making client availability a dependency.

Exit criterion: an adapter completes a multi-step GUI task while the human
client is disconnected, then a replacement client reconnects and observes the
same task state.

### Phase 4: Add persistent `exec_js`

Package the JS-in-WASM runtime, expose bounded host calls, preserve globals
between invocations, implement display and console output, enforce resource
limits, and destroy isolates reliably on task termination.

Exit criterion: one script performs a long action sequence faster than
round-tripping every action, displays verification screenshots, retains state
across calls, and cannot access filesystem, network, processes, DBus, or host
desktop resources.

### Phase 5: Add semantic scripting

Integrate AT-SPI and authorized Chromium DevTools providers behind revisioned
handles. Expose them through `computer` and `page` helpers while keeping pixel
actions available as the fallback.

Exit criterion: native and browser workloads can be queried and manipulated
semantically, with tests proving cross-session and stale-handle denial.

### Phase 6: Product UX and hardening

Add approval and takeover UX, explicit automation indicators, audit history,
redaction controls, telemetry, disconnect recovery, backpressure, protocol
negotiation, and remote authentication. Revisit lease granularity only after
the compositor has independent seats and focus domains.

Exit criterion: threat-model review and sustained nested workloads cover prompt
injection checkpoints, observer churn, slow consumers, crashes, task recovery,
and resource exhaustion.

## Verification strategy

Behavior verification uses Brood's Nix-backed nested test path and real GUI
applications. Each phase records daemon metrics, action and frame sequences,
application-visible state, and screenshots. Unit tests are appropriate for
schemas, transforms, state machines, and sandbox limits, but they do not replace
a nested end-to-end proof.

The implementation tracker is [[../workflows/computer-use-tracker]].
