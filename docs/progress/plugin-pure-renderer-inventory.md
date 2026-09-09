---
status: progress
type: implementation-inventory
authority: implementation
last-updated: 2026-09-06
---

# Pure-renderer migration inventory

Target: [[../targets/systems/plugin-pure-renderer]]

All ten bundled visual producers now use pure facets by default:
coding-agent, diff-viewer, inbox, keybindings, neovim-client, picker,
resize-proof, session-switcher, status-bar, and terminal-ui. The
`wasm-plugin-graphics` example also uses facets by default. Explicit
`--no-default-features` builds retain the legacy verification path.

The actual Nix package validates all eleven visual facet artifacts and seven
supporting modules. Strict rendering timing acceptance passes on that package;
the persistent evidence is [[plugin-pure-renderer-final-report.json]]. The
terminal proof now deliberately pauses the PTY reader and verifies an exact
256 KiB paste followed by a queued key, closing the reproduced truncation
failure. Editor open/edit/save, large file streaming, and terminal/diff/agent
routes pass on the same package. Other native lane evidence and verification
limitations are recorded in [[plugin-pure-renderer]].

The tables below preserve the initial migration inventory as historical context;
their source line numbers describe that checkpoint and may have moved.

## Visual producers and input paths

| Plugin | Current render producer and state | Current input path | Worker/sharing note | Migration dependency |
| --- | --- | --- | --- | --- |
| `coding-agent` | `redraw_ui`/`draw_*` at `plugins/coding-agent/src/lib.rs:2355-3392`; reads `UiState` in global `STATE` | `handle_presentation` at `1088-1269`, including key/text/paste/pointer/image | `STATE: Mutex<RoleState>` (`368`); `worker_run` (`387`) reads shared state | Publish agent/tab/transcript/prompt/image view; stable hit regions and prompt mappings; preserve actor routing and image leases |
| `diff-viewer` | `redraw`/`redraw_with_cache` at `plugins/diff-viewer/src/lib.rs:449-630` | `handle_presentation` (`230-400`) | `STATE: Mutex<ViewerState>` (`189`); worker only runs `collect_project_diff` (`225`), completion dispatcher applies the result | Publish immutable diff document and scroll/selection view; move hit testing into interaction snapshot |
| `inbox` | `redraw` at `plugins/inbox/src/lib.rs:419-645` | `handle_presentation` (`200-348`) | `STATE: Mutex<InboxState>` (`99`); worker is idle (`126`) | Publish bounded item list/selection; stable item identity for activation and close hit regions |
| `keybindings` | `redraw` at `plugins/keybindings/src/lib.rs:570-589` | `handle_presentation` (`355-418`) | `STATE: Mutex<Option<KeybindingsState>>` (`195`); worker idle (`218`) | This pane has editor/capture state and global policy coordination; define facet-to-policy semantic event boundary |
| `neovim-client` | `redraw`/`redraw_with_layouts` at `plugins/neovim-client/src/lib.rs:1483-...` | `handle_presentation` (`488-578`) | `STATE: Mutex<Option<ClientState>>` (`54`); worker paths at `394` and many state locks | Publish editor snapshot and cursor/selection/file-transfer display state; define interaction identity for editor controls |
| `picker` | `redraw` at `plugins/picker/src/lib.rs:1151-...` and spinner (`1414`) | `handle_presentation` (`689-800`) | `STATE: Mutex<PickerState>` (`384`); worker (`404`) | Explicit time is needed for spinner; publish candidates/query and stable selection identities |
| `resize-proof` | `render_with_hint` plus `submit_target`/`submit_recovery` at `plugins/resize-proof/src/lib.rs:832-875,1201-1320` | Render and pointer events in `handle_presentation` (`832-930`) | `STATE: Mutex<ProofState>` (`497`); worker (`523`) | Keep as protocol probe; convert proof to facet invocation and retain its resize/recovery assertions |
| `session-switcher` | `redraw`/`redraw_with_cache` at `plugins/session-switcher/src/lib.rs:4418-4855` | `handle_presentation` (`1414-1568`) | `STATE: Mutex<SwitcherState>` (`811`); worker idle (`1053`) | M3 first vertical slice: immutable session catalogue revision, responsive layout, stable session IDs, paired interaction |
| `status-bar` | `redraw` at `plugins/status-bar/src/lib.rs:145-273` | Only pane create/resize plus render hint (`72-109`) | `STATE: Mutex<StatusBarState>` (`51`); `worker_run` is a no-op (`145`) | Publish status view and treat as noninteractive or define its future input facet explicitly |
| `terminal-ui` | `render_latest_snapshot` at `plugins/terminal-ui/src/lib.rs:1350-1822` | `handle_presentation` (`601-908`) for keys/text/paste/pointer/scroll | `STATE: Mutex<Option<UiState>>` (`54`); worker (`561`) and timer blink/resize paths | M4: publish terminal snapshot chunks/rows, explicit blink time, selection/cell hit mapping, PTY semantic events; never share mutable terminal-session state |
| `wasm-plugin-graphics` example | `submit_scene`/`build_scene` at `examples/wasm-plugin-graphics/src/lib.rs:224-451` | `dispatch` (`50-180`) handles all input events | No worker work (`180`) | Probe plugin for paired draw/interaction, resource leases, retries and deterministic target-time rendering |

At the initial inventory checkpoint, these producers called `render_with_hint`; none accepted concrete geometry,
an immutable application revision, or returns an interaction snapshot. Their
input handlers reconstructed hit testing from mutable state, which is exactly the
coupling the target removes.

## Nonvisual and compatibility plugins

These have no canvas producer and are not candidates for pure rendering by
themselves: `component-runtime-io` (`plugins/component-runtime-io/src/lib.rs:15-163`),
`component-workspace-watch` (`:15-83`), `neovim-files` (actor/files protocol,
`plugins/neovim-files/src/lib.rs:481-526`), `terminal-session` (PTY/screen
application state, `plugins/terminal-session/src/lib.rs:392-526`),
`worktree-manager` (workers and presentation replies,
`plugins/worktree-manager/src/lib.rs:141-187,427-519`), and
`workspace-file-log` (Preview 1 worker/watch proof,
`plugins/workspace-file-log/src/lib.rs:473-522`). They remain actor/worker
producers of immutable views or semantic messages consumed by visual facets.

`default-workspace` is a Preview 1 global key/layout policy guest, not a canvas
plugin (`plugins/default-workspace/src/lib.rs:420-850,1603-2219`); its mutable
layout/pointer policy buffers are host ABI state and require a separate policy
contract. `keybindings` is visual and also participates in this policy boundary.

## Nonvisual compatibility verification

The following Nix-backed checks were run on 2026-09-05 after the typed facet
input/WIT update. These are compatibility checks only; they do not establish
that any visual plugin has migrated to a pure renderer.

| Plugin | Build/componentization evidence | Existing tests | Result |
| --- | --- | --- | --- |
| `component-runtime-io` | `tools/brood-build.sh --build-wasm-one plugins/component-runtime-io`; Component Model artifact emitted | `cargo test --manifest-path plugins/component-runtime-io/Cargo.toml --locked` (0 tests) | passed |
| `component-workspace-watch` | `tools/brood-build.sh --build-wasm-one plugins/component-workspace-watch`; Component Model artifact emitted | `cargo test --manifest-path plugins/component-workspace-watch/Cargo.toml --locked` (0 tests) | passed |
| `default-workspace` (Preview 1 policy) | `tools/brood-build.sh --build-wasm-one plugins/default-workspace` | `cargo test --manifest-path plugins/default-workspace/Cargo.toml --locked` (5 passed) | passed; policy guest remains Preview 1 |
| `neovim-files` | `tools/brood-build.sh --build-wasm-one plugins/neovim-files`; Component Model artifact emitted | `cargo test --manifest-path plugins/neovim-files/Cargo.toml --locked` (13 passed) | passed |
| `terminal-session` | `tools/brood-build.sh --build-wasm-one plugins/terminal-session`; Component Model artifact emitted | `cargo test --manifest-path plugins/terminal-session/Cargo.toml --locked` (17 passed) | passed |
| `worktree-manager` | `tools/brood-build.sh --build-wasm-one plugins/worktree-manager`; Component Model artifact emitted | `cargo test --manifest-path plugins/worktree-manager/Cargo.toml --locked` (6 passed) | passed |
| `workspace-file-log` | `tools/brood-build.sh --build-wasm-one plugins/workspace-file-log`; Preview 1 WASM artifact emitted (no componentization) | `cargo test --manifest-path plugins/workspace-file-log/Cargo.toml --locked` (0 tests) | passed |

These checks validate the checked-in build targets and package compatibility.
The runtime, workspace-watch, and workspace-file-log behavior lanes in
`tools/bundled-plugin-e2e.tsv` remain separate integration workloads and are
not claimed by these package checks.

Examples `wasm-component-actor`, `wasm-init-actor`, `wasm-init-child`, and
`wasm-actor-lifecycle-proof` are actor/lifecycle/worker topology probes;
`wasm-key-handler` is a Preview 1 global key handler; `wasm-preview1-worker`
is a worker/runtime probe. None renders. Their worker entry points are at the
corresponding `src/lib.rs` `worker_run`/`brood_worker_run_v1` definitions.

## Shared-state and worker audit

The visual plugins use process-static mutex state that is touched from dispatch,
input, and worker-completion paths. `coding-agent` is the clearest direct
sharing case: `worker_run` reads `STATE` to copy active images before running a
turn. `neovim-client` also directly shares state with its runner worker:
`take_runner_input_line` drains the shared queue and parser callbacks call
`publish_runner_*`/`publish_runner_update`, which lock and mutate `STATE`.
`neovim-files` workers insert completed file snapshots into `STATE` in
`open_file` and remove transfer state in `save_file`; completion dispatch then
applies the result. `terminal-session` directly mutates screen, PTY handles,
revision, and counters from its PTY worker (`run_terminal_worker` and helpers).
`terminal-ui` consumes session snapshot messages and runs blink/resize timer
workers; its timer worker returns payloads while actor turns mutate UI state.
`diff-viewer`'s worker only runs `collect_project_diff`; completion dispatch
applies the result. `session-switcher`, `inbox`, `picker`, `resize-proof`,
`status-bar`, and `worktree-manager` retain static actor state, with idle or
timer/proof workers where applicable. A render facet must receive a pinned
immutable view and must not lock or read these statics. Worker aliases must be
excluded from published roots, or the storage design must prove immutable
ownership. A shared Wasmtime instance is not proven safe merely because calls
are made through separate exports. The M1 import-free separate-instance probes
now run while the associated actor/worker are busy; the same-artifact mixed
sync/async probe now passes after the user accepted argument lifting before
dynamic denial under the trusted-plugin model. See
[[plugin-pure-renderer]] for accepted evidence and limitations.

## Initial migration plan by bundled lane (historical)

The lanes below are the concrete acceptance workload names from
`tools/bundled-plugin-e2e.tsv`. They record the original migration work items; the current implementation
and verification checkpoint is recorded below.

| Plugin | Existing lane(s) | Pending pure-facet work |
| --- | --- | --- |
| coding-agent | `coding-agent` | Publish agent/transcript/image view; paired prompt/image hit map; preserve source focus and cancel semantics |
| component-runtime-io | `runtime-io` | No render facet; verify actor remains data producer and cannot become render authority |
| component-workspace-watch | `workspace-watch` | No render facet; verify watcher revisions can feed a separate visual consumer |
| default-workspace | `session-shell` | Separate Preview 1 key/layout policy contract; do not classify its policy buffers as render snapshots |
| diff-viewer | `session-shell` | Publish diff revision; pure scroll/close interaction; retain refresh worker as actor producer |
| inbox | `terminal`, `startup-empty` | Publish bounded notifications; stable source identity and close/focus interaction |
| keybindings | `keybindings-narrow`, `startup-empty` | Publish editor view; define semantic replacement event and global key-policy handoff |
| neovim-client | `editor` | Publish editor snapshot/cursor/layout; preserve runner and host routing |
| neovim-files | `editor` | No render facet; publish file data to client without mutable aliases |
| picker | `session-shell` | Publish candidates/query; target-time spinner; stable selection and backdrop interaction |
| resize-proof | `resize` | Convert proof producer to facet probe; retain epoch/barrier/recovery assertions |
| session-switcher | `session-shell`, `startup-empty` | M3 vertical slice: catalogue revision, responsive layout, stable IDs, paired input |
| status-bar | `startup-empty` | Publish 34px status view; explicitly allow empty interaction facet |
| terminal-session | `terminal` | No render facet; publish immutable terminal view data while retaining PTY ownership |
| terminal-ui | `terminal` | M4 terminal view/interaction facet; explicit blink time, row leases, selection/cell map |
| workspace-file-log | `runtime-io`, `workspace-watch` | No render facet; retain runtime/watch proof and bounded producer state |
| worktree-manager | `worktree` | No render facet; publish manager results to consumers without becoming a pane authority |
| wasm-plugin-graphics | example only | M2 probe for deterministic draw/interaction pairing, resource leases, retry/discard purity |

## Initial SDK/host migration dependencies (historical)

The current SDK owns actor-frame batching, resize epochs, and render pacing
(`graphics.rs:15-218`), while `brood-wasm::component_actor` queues and dispatches
`PresentationEvent` and maps it to generated WIT types (`component_actor.rs:3400-3520,3705-3932`).
The host/client still admits input by actor event queues
(`clients/brood-client/src/actor_runtime.rs:1929-2010`). Migration therefore
requires a new facet invocation and admission path rather than only changing
plugin functions. Existing `ActorFrame` can inform packet/resource ownership,
but its token, actor watermark, and resize retry semantics do not provide the
target's application revision, geometry, paired interaction identity, or
sequence state.

## Migration prerequisites and actual tensions

The following are implementation prerequisites already anticipated by the
target and campaign, rather than contradictions in the target:

1. M1 must measure copied views versus shared immutable chunks/handles, total
   retention under pinned input/render leases, guest transfer/read cost, and
   reclamation before product migration.
2. WIT/SDK work must add the planned render/input facet contracts: application
   revision, concrete geometry/configuration, interaction snapshot, sequence
   state, and bounded semantic-event return data.
3. The planned capability split must be implemented and its instance/failure
   scope proven under busy actor/worker load.
4. Every visual plugin needs a state classification and publication boundary;
   render-only caches may remain, while semantic state must not depend on
   commit/discard/retry.

One genuine migration tension remains: current actor dispatch accepts legacy
`RenderHint` events and can commit `ActorFrame` output, while the target
requires one authoritative facet path per pane generation. The host must define
the cutover/compatibility rule so a pane cannot accept both authorities during
replacement. `default-workspace` is a separate Preview 1 policy/layout concern;
it is not a pure-renderer contradiction. A status bar with no interactive
controls can use an empty interaction snapshot once the facet contract exists.

## Current implementation and verification

All ten visual plugins and the graphics example implement separate render and
input facets. The shared host enforces bounded publication, isolated invocation,
paired native draw/interaction commit, pinned input interpretation, generation
cleanup, resource leases, and exact resize cohorts. All seven nonvisual plugins
retain their data, worker, or policy roles. All ten visual defaults and the graphics example now enable pure rendering.

The 51 plugin/example checks pass for native legacy/pure tests and both WASM
Clippy variants, plus the seven nonvisual native suites. The final combined host passes
628 workspace tests and all-target Clippy;
one existing user-systemd isolation
test is explicitly ignored. Storage/reclamation, facet isolation, resource and
input admission probes, styled terminal Unicode and exact 256 KiB paste,
coding-agent clipboard images, split resize barriers, and the 180 Hz geometry
workload have passing evidence in [[plugin-pure-renderer]].

Editor open/edit/save and the complete populated shell have passed. The strict rendering report with causal key latency, zero-work idle, recovery,
visibility changes, and saturated hot-path limits passes. The assembled default package also passes artifact validation and the strict
packaged gate.
The initial tables above preserve implementation history rather than redefining
the target or describing current source line locations.
