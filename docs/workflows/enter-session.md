---
status: current
type: workflow
authority: implementation
last-reviewed: 2026-07-13
---

# Enter And Show Sessions

Session commands are the user-facing route into a Brood session environment.

Current entry points:

```sh
brood enter
brood show
brood show here
brood hide
brood list
brood switch
```

`enter` creates or reuses a directory-bound session and starts a shell or requested command inside that context. `show` presents the current entered session when one is available, otherwise it presents a directory-bound or explicitly selected session. `show here` always uses the command's current directory as the session target. `hide` detaches presentation. `list` reports sessions. `switch` uses a selector or direct query to activate an existing session or create a new one.

An entered command's exit status is the exit status of the `brood enter`
process. Command execution reports that status through the CLI outcome model;
only the binary entrypoint translates the outcome into process termination.

Exact flags are defined in `crates/broodd/src/lib.rs`.

## Project Environment Activation

When a [[../concepts/session-runner]] is configured or detected, Brood activates the project environment before starting the session server or entered command.

Related:

- [[../concepts/session]]
- [[../concepts/session-runner]]
- [[../systems/runtime]]
- [[../systems/configuration]]
