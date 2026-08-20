---
status: canonical
type: concept
authority: product
last-reviewed: 2026-07-11
---

# Session

A session is a group identity and sandbox for panes, plugin state, environment,
cwd, agents, programs, and resources. It does not decide which panes a
presentation client maps.

Every session also belongs to a [[project]]. Several sessions may share one
project while using different Git worktrees and branches.

Session lifetime is explicit. A session remains present until a user or actor
closes it, even when it has no managed Wayland windows. An empty window
inventory is not evidence that the session is unused: session actors,
presentation-local plugin panes, agents, processes, and managed-worktree
occupation may all still depend on its identity.

Every session-scoped pane and actor carries that session identity directly.
Presentation-global actors, such as the shell and reusable picker, carry no
session identity. Activating another session changes the default visible group;
it neither guesses new ownership from an actor name nor rebinds an existing
actor. Explicitly closing a session stops presentation actors associated with
it and tears down its session-side realm.

The session environment is the unit at which Brood chooses authority. It may
run directly on the local host, inside a container, or on a remote machine. Its
mounts, process namespace, devices, network, credentials, and host bridges
determine what code in that session can reach.

Programs, agents, and plugins inside one session are trusted peers. Actor
address spaces and process boundaries still provide fault isolation, but they
are not treated as security boundaries between hostile components. Access to
another session or back to a presentation host requires an explicit bridge.
Presentation clients may receive window identities from multiple sessions and
compose any authorized subset. That presentation choice does not merge the
sessions' actor, process, resource, or capability boundaries.

A direct local session has the full authority of the local user; its logical
session boundary provides organization and fault containment, not isolation
from the host or another equally trusted local process. Container isolation is
not claimed to be perfect, but it is a clear configurable boundary. Remote
sessions use the same model with execution and authority located on the remote
endpoint; forwarded credentials, sockets, clipboard, files, devices, and host
process launch remain explicit bridges.

Current user-facing operations such as `enter`, `show`, `hide`, `list`,
`switch`, and explicit close are session-oriented. See
[[../workflows/enter-session]].

Related:

- [[workspace]]
- [[project]]
- [[pane]]
- [[../targets/concepts/plugin]]
- [[session-runner]]
- [[../systems/runtime]]
