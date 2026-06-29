# Agent Instructions

## Reference Code

When implementing features, check the code in `reference/` for relevant examples before designing a new approach from scratch.
Refer patterns that fit Manifold's own architecture and terminology. Treat the reference repositories as examples, not source to copy wholesale.

## Documentation

Document the target design by default, not the current implementation snapshot.

Only describe the current state when it materially affects usability, operator workflow, or verification.

Do not duplicate CLI argument lists in prose. For exact flags, subcommands, and parser behavior, point readers to the code that defines them.

## Development Environment

The repo has a Nix flake for development-shell use, but agents should not assume it is required. Prefer the normal checked-in Cargo and script entry points unless the task specifically needs the flake or the local environment is missing dependencies.

## Testing

This repo is in a highly volitile and experimental state. Existing tests are to be questioned rather than trusted. If a test enforces a behavior that goes against the intention of the request, or it is anyway not fit for purpose, modify or remove it.
Bad tests are worse than no tests.

Whenever possible prefer to test the changes with a "real" workload. Open the real app and send real keystrokes, clicks and scrolls and record metrics from the nested client as well as manifoldd. Use screenshots to verify any graphical changes.

For agent-driven behavior tests, use the built-in test CLI rather than starting
a host-visible Manifold presentation yourself. Prefer `cargo run -- test` or
`target/debug/manifoldd test nested --inner-command '<client command>'` after a
build. This starts the outer supervisor headlessly by default and avoids
creating a new OS window in the current desktop session.

Do not use `tools/dev-nested-manifold.sh` for automated agent verification
unless the task explicitly asks for a host-visible manual development harness.
That script starts a stable Manifold presentation on the current desktop and is
not the no-host-window behavior-test path.

Do not use plain `cargo run`, `manifoldd run`, or `manifoldd run --dbus-name
...` as a behavior-test entry point from an existing desktop session. `run` is
the product/service runtime path; `test` owns test-time nesting policy.
