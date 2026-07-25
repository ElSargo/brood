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

The session environment is the unit at which Manifold chooses authority. It may
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

Current user-facing operations such as `enter`, `show`, `hide`, `list`, and `switch` are session-oriented. See [[../workflows/enter-session]].

Related:

- [[workspace]]
- [[pane]]
- [[plugin]]
- [[session-runner]]
- [[../systems/runtime]]
