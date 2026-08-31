---
status: progress
type: verification-record
authority: implementation
---

# bmux Progress

The initial implementation adds a separately packaged `bmux` binary and
Brood-owned control operations for terminal inventory, creation, input, focus,
and teardown. Session operations reuse the typed Brood session API. Dynamic
terminal presentation continues to be paired by the existing session and
client init actors.

Unit coverage records target encoding, format expansion, named/literal/hex key
encoding, and isolated server-name mapping. Strict Clippy passed for the native
bmux/server/client crates and both changed WASM plugins. The bmux,
session-switcher, and terminal-session suites passed 55 unit tests in total.

The full Nix-backed aggregate build completed, and `nix build path:.#brood`
produced a package containing the wrapped `bmux` executable. An isolated real
DBus/server/PTY workload created a named session, observed its initial pane,
created a second first-party terminal, sent literal and named keys, listed
tmux-style session and pane formats, removed the pane, renamed the session, and
removed the session. The output included stable `$1`, `%4294967297`, and
`%4294967298` identifiers. The supported nested picker-consumer behavior lane
also passed after the plugin changes.

The broader pre-existing terminal proof progressed through rendering,
byte-exact input, application-cursor keys, and layout checks, then stopped at
its inbox-shortcut screenshot assertion. Logs showed the synchronized inbox
layout request, so this is retained as test-harness timing evidence rather than
treated as bmux acceptance evidence.

The target contract is [[../targets/systems/bmux]].
