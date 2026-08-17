---
status: target
type: workflow
authority: implementation
last-reviewed: 2026-08-17
---

# Computer Use Implementation Tracker

This page tracks delivery of [[../systems/computer-use]]. It records completed
work only when there is code and proportionate verification evidence.

## Project state

- Worktree: `/home/sargo/projects/brood-computer-use`
- Branch: `codex/computer-use-plan`
- Baseline: `147c83eb54a740479b136395e4d88222f9e744a4`
- Current milestone: Phase 5, semantic scripting
- Implementation status: Phase 4 is complete. A packaged, capability-empty
  QuickJS/WASM isolate executes persistent scripts through the authoritative
  server action path, including observer-visible approval checkpoints and
  bounded queued-call teardown. The presentation client can spectate changing
  server observations without gaining automation-layout authority.

Checklist meanings:

- `[ ]` not started or not proven;
- `[~]` code exists but the phase exit criterion is not proven;
- `[x]` implemented and linked to evidence below.

## Agreed decisions

| Decision | State |
| --- | --- |
| Control only applications hosted inside Brood sessions | Accepted |
| Keep the host desktop outside the computer-use boundary | Accepted |
| Run capture, input, settling, scripting, and the control loop server-side | Accepted |
| Closely model Codex direct computer actions and persistent code execution | Accepted |
| Use a fixed 1440 by 900 server automation viewport initially | Accepted |
| Keep the human presentation client an observer, approval, and takeover surface | Accepted |
| Exclude client-local plugin panes from initial pixel control | Accepted |
| Start with one daemon-wide lease because the compositor seat is shared | Accepted |
| Isolate generated code with explicit capabilities and hard resource limits | Accepted |

## Phase 0: Contract and fixtures

- [x] Record product scope, ownership, and non-goals.
- [x] Record the direct action vocabulary and persistent `exec_js` shape.
- [x] Record the initial viewport and lease decisions.
- [x] Add transport-neutral computer-use domain types.
- [x] Define task and lease lifecycle transitions.
- [x] Define versioned direct-action and observation schemas.
- [x] Define script, approval, cancellation, and error schemas.
- [x] Define defaults and hard ceilings for runtime limits.
- [x] Add golden encode/decode fixtures.
- [x] Add a provider-free fake adapter for contract tests.
- [x] Amend presentation-client documentation for the automation-render target.

Phase gate: all lifecycle messages have fixtures, stale generations are
specified, and no external adapter owns domain semantics.

## Phase 1: Server capture and automation layout

- [x] Separate reusable buffer import/readback from debug capture.
- [x] Support DMA-BUF and SHM source buffers.
- [x] Add the fixed automation viewport and deterministic layout.
- [x] Add active-window selection by stable identity.
- [x] Encode original PNG observations with complete metadata.
- [x] Add monotonic observation and frame sequence markers.
- [x] Bound capture memory, cadence, queue depth, and consumer backpressure.
- [x] Prove capture with no presentation client connected.

Phase gate: real nested DMA-BUF and SHM applications produce correct changing
screenshots and metadata from the server alone.

## Phase 2: Direct actions and lease

- [x] Add daemon-wide lease acquisition, renewal, expiry, and revocation.
- [x] Validate task scope and lease generation for every batch.
- [x] Execute ordered action batches on the compositor event loop.
- [x] Implement click, double-click, move, and drag.
- [x] Implement scroll, type, keypress, wait, and screenshot.
- [x] Track and clean up held keys, modifiers, and buttons.
- [x] Implement causal settle detection and bounded timeouts.
- [x] Add pause, cancellation, and human-input takeover policy.
- [x] Publish auditable action and result events.
- [x] Prove stale-lease, cross-session, and out-of-bounds rejection.

Phase gate: a real nested GUI workload is completed using every direct action,
with application-visible results and screenshot evidence.

## Phase 3: Server-side task loop and adapters

- [x] Add the computer-use task supervisor to `brood-sessiond`.
- [x] Stream observations and status independently of clients.
- [x] Add server-owned approval checkpoints.
- [x] Add pause, resume, stop, and replacement-observer behavior.
- [x] Implement a Codex-shaped direct computer-call adapter.
- [x] Expose equivalent MCP direct-action tools.
- [x] Add client observer and approval protocol messages.
- [x] Prove operation across presentation-client disconnect and replacement.

Phase gate: a model adapter completes a multi-step task with the presentation
client disconnected, and a replacement client can observe its state.

## Phase 4: Persistent code execution

- [x] Select and package the QuickJS-in-WASM runtime.
- [x] Add one persistent isolate per computer-use task.
- [x] Expose `computer`, `display`, `console`, and `askUser` globals.
- [x] Marshal all host calls through typed task commands.
- [x] Preserve variables and helper functions across `exec_js` calls.
- [x] Return console output, images, values, exceptions, and metrics.
- [x] Enforce CPU, wall-time, memory, output, image, and host-call limits.
- [x] Deny ambient filesystem, network, process, DBus, and host-desktop access.
- [x] Cancel queued calls and destroy the isolate when its task ends.
- [x] Compare scripted and direct-action throughput on the same workload.

`computer`, `display`, and `console` are live, including task-scoped metadata
from `computer.windows()` and `computer.activeWindow()`. `askUser` creates a
typed observer-visible approval checkpoint; an exact scoped proof resumes the
already-evaluated plan without repeating guest side effects. Denial and task
stop remove pending checkpoints, approvals, isolates, and held input. Every
task limit configures the host isolate or typed-call plan and has a distinct
nested rejection proof.

Phase gate: a persistent script completes and verifies a long GUI sequence,
while adversarial scripts demonstrate every denied capability and limit.

## Phase 5: Semantic scripting

- [ ] Discover and scope AT-SPI providers for managed native applications.
- [ ] Expose revisioned accessibility snapshots and actions.
- [ ] Bind authorized Chromium DevTools targets to managed applications.
- [ ] Expose task-scoped `page` helpers.
- [ ] Invalidate handles on window, document, and tree replacement.
- [ ] Deny cross-session targets and stale handles.
- [ ] Keep screenshot and direct actions as the universal fallback.

Phase gate: native and browser tasks use semantic queries successfully, with
negative proofs for stale and unauthorized handles.

## Phase 6: Product UX and hardening

- [ ] Add visible automation, approval, pause, takeover, and stop UX.
- [ ] Add audit history and screenshot redaction policy.
- [ ] Add authentication and authorization for remote adapters.
- [ ] Add crash recovery and explicit non-recovery semantics for script state.
- [ ] Add slow-consumer and observation backpressure tests.
- [ ] Add prompt-injection checkpoints for consequential actions.
- [ ] Add telemetry for latency, settling, capture cost, and action failures.
- [ ] Run sustained nested workload and resource-exhaustion campaigns.
- [ ] Complete a threat-model review.

Phase gate: product acceptance covers safety, observer churn, failure recovery,
resource exhaustion, and remote authorization.

## Open design items

| Item | Resolve by | Notes |
| --- | --- | --- |
| Automation window layout beyond one active window | Phase 1 | Keep coordinates deterministic; do not inherit client layout. |
| QuickJS distribution and Wasmtime integration | Resolved | Package `rquickjs`/QuickJS-NG as a `wasm32-wasip1` core module and instantiate it through Wasmtime with an empty WASI context and bounded typed host-call plans. |
| AT-SPI bus ownership inside session environments | Phase 5 | Must not expose unrelated host or session applications. |
| Chromium target-to-managed-window binding | Phase 5 | Identity must be proven, not inferred from titles. |
| Human input pause versus immediate revoke defaults | Resolved | Default to pause; revoke remains an explicit task policy. Both revoke the daemon-wide lease before forwarding primary human input. |
| Independent leases per session | Post Phase 6 | Requires independent compositor seats and focus domains. |
| Structured control of client-local plugin panes | Post Phase 6 | Separate tool protocol; not part of pixel computer use. |

## Evidence log

Add one row for every phase-gate attempt, including failures that change the
design or test workload.

| Date | Phase | Commit | Real workload | Artifacts and metrics | Result |
| --- | --- | --- | --- | --- | --- |
| 2026-08-17 | 0 | working tree | Design and tracker setup | [[../systems/computer-use]] | Plan recorded; implementation not started |
| 2026-08-17 | 0 | working tree | Provider-free lifecycle and schema suite | `cargo test -p brood-computer-use`: 6 passed; `cargo clippy -p brood-computer-use --all-targets -- -D warnings`; Nix unavailable | Contract implemented; full `broodd` check blocked by missing host Wayland pkg-config metadata |
| 2026-08-17 | 0 | working tree | Nix-backed inherited-work verification | `cargo fmt --all -- --check`; 6 contract tests; crate clippy with warnings denied; `cargo check -p broodd` | Passed; previous environment limitation did not reproduce under the flake |
| 2026-08-17 | 1 | working tree | Detached `weston-simple-shm`, first attempt | `/tmp/brood-computer-use-phase1c`; two 1440x900 PNGs were captured but their hashes matched | Failed: automation did not independently issue frame callbacks, so changing client content stopped |
| 2026-08-17 | 1 | working tree | Detached SHM and DMA-BUF clients, immediate callback attempt | `/tmp/brood-computer-use-phase1d`; changing SHM PNGs, then DBus `NoReply` under EGL commit pressure | Failed: immediate callbacks could starve the compositor loop; callbacks moved to a bounded timer |
| 2026-08-17 | 1 | working tree | Detached `weston-simple-shm` and `weston-simple-egl` | `/tmp/brood-computer-use-phase1e/proof.md`; frame sequences 3 and 6; 7 captures, 0 failures, 2 accepted DMA-BUF imports; four distinct PNG hashes; presentation `Detached` | Passed |
| 2026-08-17 | 2 | working tree | Detached `foot` batch, first attempt | `/tmp/brood-computer-use-phase2a`; pointer events reached pane 3, then Smithay panicked subtracting 8 from an evdev keycode | Failed: automation keycodes now cross the existing input path in XKB keycode space |
| 2026-08-17 | 2 | working tree | Detached `foot` ordered batch under continuous commits | `/tmp/brood-computer-use-phase2b` through `phase2i`; action execution or DBus response exceeded the 15 second control deadline | Failed: passive capture and idle action callbacks could starve control; active leases now capture on demand and action timers are event sources |
| 2026-08-17 | 2 | working tree | Detached `foot` shell plus SHM and DMA-BUF clients | `/tmp/brood-computer-use-final2/proof.md`; 10 ordered action results, application-created `action-typed.txt`, frame sequence 20, input sequence 213, 75 ms measured quiet settle, 0 capture failures; stale lease, cross-session, and 696x488 bounds rejections | Partial pass: every action executed and negative boundaries are proven; cancellation cleanup, human takeover, and distinct application-visible effects for drag/scroll/double-click still need phase-gate proof |
| 2026-08-17 | 2 | working tree | Headless Sway presentation observer, first takeover attempts | `/tmp/brood-computer-use-phase2-complete-g` and `-i`; presentation transport attached, but the client exited because its required `session-switcher` actor was unavailable | Failed: the synthetic human click arrived only after the automation drag completed; the proof now launches the real presentation actor explicitly |
| 2026-08-17 | 2 | working tree | GTK action fixture, detached capture, and attached presentation takeover | `/tmp/brood-computer-use-phase2-final/proof.md`; application state records 5 clicks, 1 double-click, 250.599 px drag, 1 scroll, `Agent42`, and 1 Ctrl+L; original 1440x900 observation PNG; stop and human cancellation results; expiry and target-loss failures; final held keys/buttons 0; attached observer input count 122 | Passed: every direct action has an application-visible result, stale/scope/bounds rejection remains enforced, held input is released on stop, expiry, target loss, and human takeover, and primary human input pauses without interleaving |
| 2026-08-17 | 2 | working tree | Nix-assembled Brood package running the full GTK and takeover workload | `/tmp/brood-computer-use-phase2-packaged-final/proof.md`; package `/nix/store/1zc6qmxr6g7yzfsagrzpyqnd2lf1hlji-brood`; packaged `brood`, `brood-client`, configuration, and presentation actor | Passed: the phase gate also succeeds from the assembled package graph rather than only development binaries |
| 2026-08-17 | 2 | `4f0abde` plus working tree | Clean-Nix SpeedCrunch menu proof, first named-command attempts | `/tmp/brood-computer-use-speedcrunch-final` through `-final3`; captures showed the initial click opening Help under compact Qt geometry, then the Color Scheme click selecting Standard | Failed: fixed coordinates were recalibrated from the captured 1440x900 menu hierarchy and the chosen scheme gained an exact rendered-color assertion |
| 2026-08-17 | 2 | `4f0abde` plus working tree | Detached SpeedCrunch menu configuration and three calculations | `/tmp/brood-computer-use-speedcrunch-final4/proof.md`; pane 5, lease generation 7, 1440x900 server viewport; Solarized Light `#fdf6e3`; action sequences 5-7 and frame sequences 46-48 show `423`, `1`, and `82822551` | Passed: a real Qt application accepted server-side clicks, keyboard shortcuts, typing, waits, and screenshots with no presentation client connected |
| 2026-08-17 | 3 | working tree | Detached SpeedCrunch through the Codex call envelope, DBus observer replacement | `/tmp/brood-computer-use-phase3-a/proof.md`; event cursor 44; action sequences 1-7; three approved keyboard checkpoints; observation sequences 7-8 | Partial pass: daemon-owned history, approvals, and replacement polling worked; the presentation observer still used only the diagnostic DBus surface |
| 2026-08-17 | 3 | working tree | Replacement presentation actor observer, first attempts | `/tmp/brood-computer-use-phase3-b` and `-c`; typed client update arrived with the running task and two observations | Failed gate assertion: retained Phase 2 audit tasks invalidated a total-task-count assumption, then replacement primary input correctly paused the task before the diagnostic snapshot |
| 2026-08-17 | 3 | working tree | Detached SpeedCrunch Codex loop, replacement presentation observer, and MCP screenshot | `/tmp/brood-computer-use-phase3-e/proof.md`; pause/resume lease generations 7-8; event cursor 48 before replacement; action sequences 1-7 and observations 7-8 detached; replacement client received running sequence 8, then human input paused it; MCP resumed and completed screenshot sequence 8, observation sequence 9, event cursor 53; assembled package `/nix/store/yzy53qh7gq8b4rvhynjm1nm5pg6qc2mb-brood` contains `brood-computer-use-mcp` | Passed: a real Qt task completed through the Codex adapter with no presentation client, server-owned approvals were decided without interleaving, replacement presentation received the preserved state, and the stdio MCP adapter executed the same authoritative action path; the same adapter binary is present in the assembled package |
| 2026-08-17 | 4 | `5269a4c` plus working tree | First packaged persistent-script placement attempts | `/tmp/brood-computer-use-phase4-a` through `-c`; presentation detach did not complete, then attached-client geometry changed SpeedCrunch to 400x600, then restoring 1440x900 exposed an old-buffer GL upload mismatch | Failed: script execution moved before presentation attachment so automation geometry remains server-owned and detached |
| 2026-08-17 | 4 | `5269a4c` plus working tree | Detached persistent SpeedCrunch script, early gate attempts | `/tmp/brood-computer-use-phase4-d` and `-e`; script sequence 1 completed 11 actions and sequence 2 preserved the global, but retained observations displaced Phase 3 evidence and then exact queue-length assertions rejected the new bounded depth | Failed: raised the bounded observation queue to eight and changed the proof to assert the documented range rather than an exact incidental length |
| 2026-08-17 | 4 | `5269a4c` plus working tree | Packaged detached persistent SpeedCrunch script | `/tmp/brood-computer-use-phase4-f2/proof.md`; packaged runtime and daemon; ten planned GUI actions plus screenshot; persistent value 2; displayed 1440x900 PNG | Partial pass: the long real GUI sequence and packaging passed before ambient-authority and runaway-script checks were added |
| 2026-08-17 | 4 | `5269a4c` plus working tree | Live runaway script and capability denial, first attempt | `/tmp/brood-computer-use-phase4-g`; all three successful script sequences completed and the runaway trapped, but the assertion expected a fuel/epoch keyword absent from Wasmtime's propagated WASM backtrace | Failed proof assertion: require the execution trap plus a measured bounded return instead of backend-specific wording |
| 2026-08-17 | 4 | `5269a4c` plus working tree | Full packaged retry after assertion fix | `/tmp/brood-computer-use-phase4-h`; SHM changed but the two early DMA-BUF captures happened to match | Failed before Phase 4: retained as cadence evidence; immediate retry used the same package and workload |
| 2026-08-17 | 4 | `5269a4c` plus working tree | Detached persistent SpeedCrunch script with adversarial capability probes | `/tmp/brood-computer-use-phase4-j/proof.md`; package `/nix/store/672yi6f1q75sq4m4c1g3x45iady1nbw9-brood` contains `runtimes/computer-use-js.wasm`; script performed the three calculations and displayed a 1440x900 PNG; persistent value 2; filesystem, network, process, DBus, and host-desktop globals all `undefined`; runaway trapped in 45 ms; scripted round trip 1314 ms versus 2161 ms direct | Partial pass: packaged persistent execution, real GUI actions, persistence, ambient denial, CPU bounding, and throughput comparison pass; `askUser`, live window context, exhaustive limit attacks, and termination proof remain before the phase gate |
| 2026-08-17 | 4 | `5269a4c` plus working tree | Live managed-window context, first packaged attempt | `/tmp/brood-computer-use-phase4-k`; script reached context setup after all direct SpeedCrunch actions | Failed: QuickJS-NG does not provide browser `structuredClone`; the JSON-only metadata boundary now uses an internal JSON clone |
| 2026-08-17 | 4 | `5269a4c` plus working tree | Development-binary context and limit retries | `/tmp/brood-computer-use-phase4-l` through `-n`; task-scoped window context and all six limit attacks passed in `-m`; local presentation actor availability and immediate human input caused unrelated earlier/later failures | Failed full gate: retained the successful Phase 4 artifacts, then returned to the assembled package path |
| 2026-08-17 | 4 | `5269a4c` plus working tree | Final-package retries with expanded limit tasks | `/tmp/brood-computer-use-phase4-o` through `-q`; first DMA-BUF samples matched in `-o`; placing six retained limit tasks before replacement observation caused presentation churn in `-p`; `-q` passed observer and MCP checks but waited on stale attached status after killing the replacement client | Failed proof ordering and stale-status assertions: moved limit attacks after replacement-client verification and require process termination rather than immediate diagnostic detachment |
| 2026-08-17 | 4 | `5269a4c` plus working tree | Packaged persistent SpeedCrunch script, live context, every limit, and teardown | `/tmp/brood-computer-use-phase4-s/proof.md`; exact current-tree package `/nix/store/4krnxf3q8is7fd8j2h02d4vfv628sjpn-brood`; packaged runtime 1,221,568 bytes; script round trip 1296 ms versus 1948 ms direct; pane 5 `org.speedcrunch.` context at 1440x900; persistent value 2; one displayed 1440x900 PNG; isolate count 1 before stop and 0 after; CPU, wall, memory, output, image, and host-call attacks rejected in 146-172 ms; all ambient globals unavailable | Partial pass: the documented persistent-execution exit criterion and every resource-limit attack pass from the assembled package; only `askUser` checkpoint handling and concurrent queued-call cancellation remain unchecked Phase 4 items |
| 2026-08-17 | 4 | `5269a4c` plus working tree | First spectator and checkpoint package attempts | `/tmp/brood-computer-use-phase4-t` and `-u`; the presentation requested 480x566 before it could spectate, and the combined JSON update exceeded the 16 KiB local actor record | Failed: retained as evidence for enforcing automation-owned resize rejection at the server boundary and splitting compact observer state from its newest frame |
| 2026-08-17 | 4 | `5269a4c` plus working tree | Development spectator visual review | `/tmp/brood-computer-use-phase4-z/proof.md` passed its assertions, but `automation-spectator.png` was vertically inverted | Failed visual acceptance: added explicit scanline orientation normalization plus a top-versus-bottom raster assertion; the static harness result alone was insufficient |
| 2026-08-17 | 4 | `5269a4c` plus working tree | Packaged persistent scripts, checkpoints, cancellation, and live spectator | `/tmp/brood-computer-use-phase4-ae/proof.md`; package `/nix/store/fc69m27nb0ynf4sx3vmrmyq6vczlpl3j-brood`; upright `automation-spectator.png` at 800x600; attached action sequence 8 advanced the observation while the server viewport remained 1440x900; MCP sequence 9 advanced the task to 10; `askUser` resumed with `askRuns=1`; denial and stop reduced checkpoint count from 1 to 0 with zero held keys/buttons | Passed: the presentation displays changing automation output without layout authority, and all remaining Phase 4 interactive-checkpoint and queued-call lifecycle items have packaged nested evidence |

## Immediate next actions

1. Begin Phase 5 by discovering AT-SPI providers for the authorized managed
   SpeedCrunch window without exposing host accessibility state.
2. Define revisioned semantic snapshots and stale-node rejection alongside the
   existing visual observation sequence.
3. Keep the spectator non-authoritative while adding explicit approval, pause,
   stop, and takeover controls in the later product-UX phase.
