# Apps

Manifold apps are intended to be sandboxed, capability-scoped modules. WASM is the preferred direction for built-in and user-facing apps that do not need native GPU or process integration.

## App Shape

A WASM app should own its own local state machine and emit scene patches. The native renderer executes the fast draw path.

Potential lifecycle:

- `init`
- `update(event)`
- `render -> scene patch`

The exact ABI, runtime, and component model are undecided.

## Host APIs

Possible host APIs:

- `panes.*`
- `editor.*`
- `git.*`
- `files.*`
- `commands.*`
- `ui.*`
- `agent.*`
- `storage.*`

APIs should be capability-scoped. Apps should request explicit access instead of receiving ambient filesystem, editor, or shell power.

## Intended Built-In Apps

- git status, diff, commit, and tree tools
- agent panels and inspectors
- settings
- dashboards
- project tools
- file and symbol explorers

## Non-Goals For V1

- raw GPU access from arbitrary WASM apps
- browser implementation in WASM
- terminal PTY implementation in WASM
- unrestricted filesystem or command execution
