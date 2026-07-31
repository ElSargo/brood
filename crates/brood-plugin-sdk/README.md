# Brood plugin SDK

`brood-plugin-sdk` is the common guest layer for Component Model plugins. It
owns the generated `brood-plugin` bindings and the bounded mechanisms that
plugins would otherwise reproduce.

The SDK is intentionally not a framework and does not own plugin policy.
Plugins implement the generated `Guest` trait directly and keep control of
their actor topology, mailbox protocols, supervision, state machines, and
presentation layout.

## Target use

Import the canonical bindings, implement the actor, and export it with the SDK
module as the generated type root:

```rust,ignore
use brood_plugin_sdk::{
    bindings::{self, brood::plugin::host},
    exports::brood::plugin::actor::{Event, Guest},
};

struct Plugin;

impl Guest for Plugin {
    // ...
}

bindings::export!(Plugin with_types_in bindings);
```

The public modules cover:

- `actor`: explicitly addressed, nonblocking actor command constructors;
- `graphics`: bounded text options, packet submission, and safe resource
  retirement after replacement submission;
- `input`: stable button and modifier constants;
- `runtime`: worker-only notification, timer, stream, child, and subprocess
  helpers that clean up temporary handles and drain both pipes concurrently;
- `text`: cheap Unicode-safe label bounding before authoritative host shaping.

Use runtime helpers only from `Guest::worker_run`. Dispatcher code must remain
completion-driven and return commands rather than waiting.

For the exact host and actor surface, read
`crates/brood-wasm/wit/brood-plugin.wit`; it is the parser and ABI source of
truth.
