---
status: current
type: system
authority: implementation
last-reviewed: 2026-07-11
code-links:
  - crates/manifoldd/src/compositor.rs
  - crates/manifold-wasm/examples/io_rube_goldberg.rs
  - nix/packages.nix
  - tools/audit-v1-capabilities.sh
  - tools/smoke-v1-full.sh
  - tools/test-wasm-io-rube-goldberg.sh
---

# Testing

The supported behavior-test path is the built-in `manifold test` CLI, run through the Nix development shell for checked-in workflows. It owns nested topology policy and avoids creating a host OS window by default.

Nested command readiness belongs to the exact hosted child returned by the
spawn transition. By default the supervisor returns after that child's pane is
mapped and presented, which supports persistent graphical clients. Controller
workloads can instead select owned process completion and wait for that exact
child to exit. In either policy an unrelated window or hosted-child exit cannot
complete the command. The supervisor owns normal inner/outer shutdown, so
controller proofs do not need `--keep-running` or manual daemon quit calls. The
real Weston terminal workload in `tools/test-nested-command-ownership.sh`
exercises this boundary and preserves both its rendered screenshot and
completion transcript.

Use product runtime commands such as `manifold run` for runtime/manual use, not automated behavior validation.

The core-WASM harness controller owns one checked child identity, lifecycle
event stream, cancellation path, and join result. The focused real proof is
`tools/test-wasm-harness-lifecycle-nested.sh`: its harness plugin starts an
animated outer EGL client and interactive terminal beside the nested
presentation, while the built-in offscreen test topology captures all three
panes. One run
delivers a named signal and records a passing guest completion plus an ordinary
join; a second run leaves the guest blocked in the signal import and proves
that supervisor shutdown wakes, interrupts, and joins that exact child without
fabricating a guest result. The composed screenshots and daemon lifecycle
transcripts are the acceptance evidence, not the guest's integer return alone.

`tools/test-wasm-actor-lifecycle-nested.sh` is the observable core-actor
replacement and routing proof. A real subscribed plugin actor changes a live
terminal and animated EGL client from columns to rows across same-name hot
reload. Between those generations, an invalid two-actor candidate is rejected;
a real mailbox probe and a third column screenshot prove that the old actor set
remains owned and responsive. The module also contains a real unsubscribed poison actor which would
spawn a third client on an incorrectly broadcast host event; the compositor
dumps and both screenshots prove that only two panes remain.

## Built-In Nested Tests

`manifold test` defaults to `manifold test nested`.

Outside Manifold, the default topology starts an outer supervisor with offscreen presentation, then starts the subject compositor inside it. Inside an existing Manifold session, the test command detects `manifold_session` and starts only the subject compositor unless `--force-outer` is used.

The outer supervisor is offscreen-attached unless `--outer-headful` is passed.

Nested tests render surface-local fiducial markers in hosted surfaces before final composition. Debug dumps include the composed framebuffer and matching marker IDs in `state.json`, so image-based checks can verify that the captured pixels and Manifold's expected pane/layout metadata agree.

The sandboxed graphical E2E path is `checks.software-e2e`. It forces Mesa software rendering, starts its own DBus session bus, runs an offscreen outer presentation, runs a normal inner presentation inside it, spawns real Weston clients, applies a split layout, and exports inner and outer debug dumps. This check is intended to cover real composition and capture without depending on a host GPU or host-visible presentation window.

`tools/test-safe-boundaries-nested.sh` is the focused boundary proof. A real
workspace actor visibly splits a terminal and animated Weston SHM
client. The managed terminal sends a substantive desktop notification through
its isolated realm bus and records the host notification ID returned through
the same wire path. A debug bundle records both live surfaces and the composed
offscreen Winit/EGL frame. The proof deliberately does not claim that a later
DBus request can read Smithay's already-consumed transient buffer assignment;
the SHM mapping and pixel boundary instead has deterministic malformed,
stride, zero-size, overflow, and alpha unit coverage.

The sandboxed plugin I/O path is `checks.wasm-io`, also available as the
`wasm-io-check` package. It builds the Component and Preview 1 guests plus their
host workloads, then proves completion I/O, guest-owned shared-memory
coordination, notification coalescing, worker `wait-any`, cancellation teardown,
ordinary WASI read compatibility, and structured shutdown.

The Preview 1 unit suite deterministically gates a worker after Store and
reservation preparation, begins shutdown, and then releases the late result. It
also holds a completed worker's queued reservation release across shutdown.
Together they assert cancellation and cleanup without relying on scheduler
timing. `tools/test-wasm-preview1-worker-nested.sh` is the real lifecycle proof:
it replaces a live generation with suspended workers, observes the generation
change, then runs workers in the replacement before structured shutdown. Its
fixed workload produces worker values 90 before replacement and 81 afterward.

`plugins/workspace-file-log` remains a user-observable normal-operation
regression workload. Its real initial scan, live create/modify/remove history,
and completion-backed snapshot prove useful worker and I/O behavior through
filesystem artifacts rather than a plugin success return; it does not itself
exercise generation replacement.

The cross-runtime Rube Goldberg workload composes those mechanisms across
dozens of actors and workers. Its `quick` profile executes more than one
thousand deterministic transitions and is the only profile included in CI:

```sh
nix build .#checks.$(nix eval --raw --impure --expr builtins.currentSystem).wasm-io-rube
```

Use the packaged runner for the larger profiles. Always retain the reported
seed when sharing a failure because it reconstructs the same logical plan:

```sh
nix run .#wasm-io-rube -- --profile stress --seed 5566816691025824836 --out-dir ./rube-stress
nix run .#wasm-io-rube -- --profile soak --seed 5566816691025824836 --out-dir ./rube-soak
```

`stress` performs a much larger finite run. `soak` executes a deterministic
sequence of epochs for long-running resource-leak and stale-generation checks.
They are manual tests rather than required CI checks. The artifact directory
contains `manifest.json`, bounded `trace.jsonl`, `resource-samples.jsonl`, and
`summary.json`; a failed run also writes `failure.json`. Failures report the
seed, contextual failing operation, and shell-safe exact replay command on
stderr so the diagnostic survives a failed Nix build. Resource state is sampled
after every completed epoch as well as after replacement and shutdown.

| Profile | Actors | Workers per actor | Steps | Default bound |
| --- | ---: | ---: | ---: | --- |
| `quick` | 32 | 4 | 1,536 | one epoch |
| `stress` | 48 | 8 | 100,000 | one epoch |
| `soak` | 48 | 12 | 8,192 per epoch | one hour |

The workload source defines the exact override behavior. `--epochs` works for
all three profiles. For soak, an explicit epoch count replaces the default
one-hour bound; an explicit duration bounds the repetitive workload after
one-time actor setup.

The complementary nested-daemon shard exercises the multi-instance registry,
per-actor routing, rejection of a corrupt replacement without losing the old
generation, isolated valid replacement, peer progress, and structured daemon
teardown with eight simultaneously hosted WASM actors. It also proves that the
checked-in legacy workspace plugin still loads through its declared namespace
while rejecting a false hosted `--id`:

```sh
nix develop --command bash tools/test-wasm-io-registry-nested.sh
```

See [[../workflows/test-nested]].

## Manual Host-Visible Harness

`tools/dev-nested-manifold.sh` is for manual development when a host-visible stable presentation is wanted. It is not the default automated agent verification path.

## Smoke Suites

Use `nix develop --command tools/audit-v1-capabilities.sh` for the concise acceptance audit. It runs Rust checks, headless compositor checks, host-backed checks when a host display is available, and DMA-BUF/explicit-sync capability checks.

Use `nix develop --command tools/smoke-v1-full.sh` for the larger full proof suite on a host display.

## Related Pages

- [[../workflows/test-nested]]
- [[runtime]]
- [[wayland-compositor]]
