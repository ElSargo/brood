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

Use product runtime commands such as `manifold run` for runtime/manual use, not automated behavior validation.

## Built-In Nested Tests

`manifold test` defaults to `manifold test nested`.

Outside Manifold, the default topology starts an outer supervisor with offscreen presentation, then starts the subject compositor inside it. Inside an existing Manifold session, the test command detects `manifold_session` and starts only the subject compositor unless `--force-outer` is used.

The outer supervisor is offscreen-attached unless `--outer-headful` is passed.

Nested tests render surface-local fiducial markers in hosted surfaces before final composition. Debug dumps include the composed framebuffer and matching marker IDs in `state.json`, so image-based checks can verify that the captured pixels and Manifold's expected pane/layout metadata agree.

The sandboxed graphical E2E path is `checks.software-e2e`. It forces Mesa software rendering, starts its own DBus session bus, runs an offscreen outer presentation, runs a normal inner presentation inside it, spawns real Weston clients, applies a split layout, and exports inner and outer debug dumps. This check is intended to cover real composition and capture without depending on a host GPU or host-visible presentation window.

The sandboxed plugin I/O path is `checks.wasm-io`, also available as the
`wasm-io-check` package. It builds the Component and Preview 1 guests plus their
host workloads, then proves completion I/O, guest-owned shared-memory
coordination, notification coalescing, worker `wait-any`, cancellation teardown,
ordinary WASI read compatibility, and structured shutdown.

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
