---
status: current
type: workflow
authority: implementation
last-reviewed: 2026-07-01
---

# Enter And Show Sessions

Session commands are the user-facing route into a Manifold session environment.

Current entry points:

```sh
manifold enter
manifold show
manifold hide
manifold list
manifold switch
```

`enter` creates or reuses a directory-bound session and starts a shell or requested command inside that context. `show` presents a directory-bound or explicitly selected session. `hide` detaches presentation. `list` reports sessions. `switch` uses a selector or direct query to activate an existing session or create a new one.

Exact flags are defined in `crates/manifoldd/src/lib.rs`.

## Project Environment Activation

When a [[../concepts/session-runner]] is configured or detected, Manifold activates the project environment before starting the session server or entered command.

Related:

- [[../concepts/session]]
- [[../concepts/session-runner]]
- [[../systems/runtime]]
- [[../systems/configuration]]
