# Agent Instructions


## Documentation

Document the target design by default, not the current implementation snapshot.

Only describe the current state when it materially affects usability, operator workflow, or verification.

Do not duplicate CLI argument lists in prose. For exact flags, subcommands, and parser behavior, point readers to the code that defines them.

## Development Environment

Prefer the checked-in Nix flake for builds, checks, and behavior-test commands. Use `nix develop --command ...` for local Cargo entry points unless the task explicitly requires testing outside the Nix environment. This keeps compiler, linker, Wayland/EGL, WASM target, and smoke-tool dependencies aligned with CI and avoids host shell drift.

## Testing

This repo is in a highly volitile and experimental state. Existing tests are to be questioned rather than trusted. If a test enforces a behavior that goes against the intention of the request, or it is anyway not fit for purpose, modify or remove it.
Bad tests are worse than no tests.

Whenever possible prefer to test the changes with a "real" workload. Open the real app and send real keystrokes, clicks and scrolls and record metrics from the nested client as well as broodd. Use screenshots to verify any graphical changes.

For agent-driven behavior tests, use the built-in test CLI through Nix rather than starting a host-visible Brood presentation yourself. Prefer `nix develop --command cargo run -- test` or `nix develop --command target/debug/brood test nested --inner-command '<client command>'` after a Nix-backed build. This starts the outer supervisor with offscreen presentation by default and avoids creating a new OS window in the current desktop session.

Do not use `tools/dev-nested-brood.sh` for automated agent verification
unless the task explicitly asks for a host-visible manual development harness.
That script starts a stable Brood presentation on the current desktop and is
not the no-host-window behavior-test path.

Do not use plain `cargo run`, `broodd run`, or `broodd run --dbus-name
...` as a behavior-test entry point from an existing desktop session. `run` is
the product/service runtime path; `test` owns test-time nesting policy.
