# Session Environments

This page records the target design for `manifold enter`, `manifold show`,
session-local command context, and optional session isolation. It is not a
statement that these commands or isolation layers exist today.

## Goal

A Manifold session should be more than a group of panes. It should be an
enterable development environment whose shell, GUI apps, agents, local services,
DBus clients, filesystem changes, and control commands all share one explicit
session context.

This should make parallel development agents practical:

- each agent can have its own Manifold session
- each session can have its own entered shell
- each session can run GUI apps and local services without stealing another
  session's panes, ports, DBus names, or filesystem writes
- each session can export, promote, or discard its changes as a coherent unit

## Command Model

`manifold enter` is the shell-facing entry point. It should create or reuse a
session environment when invoked from outside an entered session, set the
session command context, set the GUI launch context, and then start an
interactive shell or a requested command inside that context. The server should
record which sessions are tied to each directory, plus which of those sessions
is the directory default. Repeated `enter` and `show` calls from the same
directory should reuse the default session. When a shell is already inside a
session, the default `enter` path should refuse to nest and should direct the
user toward `mctl show` or `mctl enter --new`.

`mctl enter --new` should always create a fresh session environment, even when
the command is already running inside one, and should print the session's easy
identifier so the session can be shown explicitly later. Creating that extra
session should not move the directory default unless the user asks to set the
default.

The entered environment should provide:

- the Manifold session id and optional display name
- the session DBus control endpoint
- the nested Wayland display used by GUI apps in that session
- the session cwd and inherited environment
- the session DBus address when DBus isolation is enabled
- the session filesystem root or overlay root when filesystem isolation is
  enabled

`manifold show` should be a presentation command. It should front or attach the
session bound to the current directory, or create an empty session when invoked
outside an entered environment and no directory binding exists. An explicit
session id should be able to override that lookup. `mctl show --id <id>` should
target a specific session, and bare `show` in a directory should reuse the last
default session tied to that directory. Showing an explicit session should not
move the directory default unless the user asks to set the default.

`manifold hide` should be the matching presentation command for removing the
session surface from view without closing the session. It should resolve the
same directory-bound or explicit session target as `show`, then detach the
presentation surface if one exists.

`manifold list` should show the live sessions associated with the current
directory. A global listing mode should show all live sessions in the server.

`mctl` should be a thin control alias for Manifold commands in the current
entered context. Its default target should come from the entered session
environment, while explicit targeting remains available for scripts and tests.

Do not freeze exact flags in this document. Exact parser behavior belongs in
the command definitions when the commands exist.

## Environment Contract

The first public environment variables should be stable, uppercase names. The
existing lowercase `manifold_session` marker can remain as a compatibility or
internal marker while the public contract is developed.

Expected public values:

- session identity
- session DBus control endpoint
- presentation or surface identity when relevant
- session Wayland display
- session DBus address
- session filesystem mount or overlay paths when relevant

Commands should prefer the entered context over global defaults. For example,
status, screenshots, input injection, browser control, and future capture
commands should operate on the entered session unless the user explicitly
targets another session.

GUI programs launched from an entered shell must connect to the session's
nested Wayland display by default. Setting only a Manifold session id is not
enough; the GUI launch environment must also point toolkit clients at the
session compositor.

The default entered environment should capture graphical apps into Manifold by
pointing toolkit clients at the session compositor. A non-capturing entered
environment should still export the Manifold command context, but leave GUI
launches on the inherited host display when one is available.

## Isolation Profiles

Isolation should be opt-in or profile-driven at first. A plain session can use
the real host filesystem and network. An isolated session can add any of these
layers:

- mount/filesystem namespace
- managed filesystem copy-on-write view
- network namespace
- PID namespace
- IPC namespace
- session DBus proxy or session-local bus

These layers are meant to prevent accidental cross-contamination between local
development agents. They are not a replacement for internet-safe multi-user
sandboxing.

The useful target profile is `managed`: a coherent session capsule that captures
regular filesystem writes by default, provides a session runtime directory,
routes same-session DBus clients to a Manifold-owned bus or proxy, and gives the
session its own local network namespace. `managed` should be the default
isolated profile once the required pieces exist. More restrictive profiles can
exist for tests or special cases, but the normal agent workflow should not
require predeclaring every path an old tool might try to write.

Initial profile shape:

- `plain`: no filesystem, DBus, IPC, or network isolation beyond the normal
  Manifold command and GUI launch context
- `managed`: capture regular file writes by default, use session-local runtime
  paths, use the session DBus bus or proxy, and use a session network namespace
- `strict`: capture only declared path prefixes, useful for tests and debugging
- `host-write`: explicit escape hatch for commands that really should modify
  the host filesystem

## Filesystem Virtualization

The filesystem target is a per-session copy-on-write view that keeps writes in a
discrete session-owned location while programs still see the original path
names. A process that enters `/home/user/project` should continue to see that
path as its current working directory. If the process writes `/etc/whatever` in
a managed session, it should also continue to see `/etc/whatever`; Manifold
should redirect the modified data into the session backend.

The path model has three names:

- `view path`: the path visible to the process inside the session
- `lower path`: the host path that provides the read-through base
- `upper path`: the session-owned path that stores captured writes

The first storage backend can materialize an upper tree under
`~/.manifold/worktrees/<session-id>/`. Project trees should prefer reflink or
another zero-copy copy-on-write mechanism when the host filesystem supports it,
then fall back to a normal copy. The backend is an implementation detail; the
entered environment should expose the view path as the normal path and expose
the upper path only through diagnostics such as status output.

The useful first scope is:

- the repo or worktree being edited
- regular file and directory writes under arbitrary absolute paths, captured by
  default in the `managed` profile
- selected XDG config, cache, data, and runtime roots
- session-local temporary directories and runtime directories
- selected system paths when an agent or old program writes system-facing config

The session should record enough metadata to explain where writes landed and
which lower paths they came from.

Captured filesystem effects should be classified:

- `project`: writes under the session project or worktree, promotable through
  git-aware diff and apply logic
- `external`: durable writes outside the project, visible and promotable only
  path by path
- `runtime`: sockets, pid files, lock files, temporary files, caches, and other
  session-local state that is normally discarded with the session
- `bridge`: explicit access to a host service path or socket
- `denied`: paths blocked even in managed mode

The default policy should capture writes to ordinary files and directories, not
grant real host writes. Sensitive or special paths should be denied or bridged
explicitly. Examples include `/proc`, `/sys`, `/dev`, `/boot`, `/nix/store`,
`/etc/shadow`, `/etc/sudoers`, and host SSH private keys. Runtime paths such as
`/run` and `/run/user/<uid>` need finer policy because they contain sockets,
locks, pid files, portals, and service state rather than only durable files.

Useful commands and APIs should eventually support:

- list changed files
- diff session changes
- export a session change bundle
- promote selected changes into the lower filesystem or source-of-truth repo
- discard selected changes
- show system-path writes separately from repo writes

This makes system-file edits visible instead of allowing agents to silently
contaminate the host or another agent's run.

Promotion should be explicit. Project changes can be promoted with git-aware
conflict checks against the lower tree. External changes should default to
export or path-by-path promotion because a write to host config has a different
risk profile than a source-file edit.

## Runtime IPC Virtualization

Many troublesome GUI and service-style applications communicate through
filesystem sockets, pid files, lock files, abstract Unix sockets, DBus names, and
loopback ports. The `managed` profile should treat runtime IPC as part of the
same session capsule as filesystem isolation rather than as an unrelated later
feature.

Expected behavior:

- `XDG_RUNTIME_DIR`, `TMPDIR`, and similar runtime variables point to
  session-owned locations
- Unix sockets created in managed runtime paths are session-local
- runtime files are classified as `runtime` and discarded by default
- host sockets and services are reachable only through explicit bridges
- abstract Unix sockets require IPC namespace support or app-specific handling,
  because they are not filesystem paths

This prevents applications that use ad hoc sockets or lock files from leaking
state into another Manifold session or into the host. Host integrations such as
Wayland, PipeWire, portals, notifications, clipboard, and credential helpers can
be bridged later as named capabilities.

## Network Virtualization

A network-isolated session should get its own loopback interface. That lets two
sessions bind the same local ports without colliding.

The important behavior is that "host networking" inside tools and containers
should be relative to the session namespace when the command is launched from an
entered isolated session. A container started with host networking inside one
session should share that session's network namespace, not the real host's
ambient network namespace.

Outbound network access can be added through a bridge, NAT, `pasta`,
`slirp4netns`, or another implementation after the local loopback behavior is
proven.

## DBus Isolation

Each isolated session should have a Manifold-owned session-local DBus address.
The default shape is an in-process bus/proxy realm per session. Same-session
clients can use familiar DBus name ownership and method calls, while Manifold
keeps the routing point in-process so traffic can be logged, bridged, or denied
by later policy profiles.

Expected behavior:

- ordinary same-session apps and agents can talk to each other through the
  session bus
- cross-session DBus communication requires an explicit bridge
- the host session bus is not leaked by default
- allowed host service names are explicit and auditable
- DBus traffic that matters to agent behavior can be logged at the Manifold
  session boundary
- selected host services can be bridged through the same in-process boundary
  when a session policy explicitly allows them

This gives GUI apps and desktop-style services a familiar communication path
without making the global user bus ambient authority for every session.

## Process And Cleanup Model

A PID namespace is useful when Manifold owns cleanup and process accounting for
an entered session. It should include a small init or reaper so orphaned
children do not escape lifecycle tracking.

The session process should be able to report:

- running processes in the session
- pane producers associated with those processes
- local services and ports owned by the session
- exited or crashed producers
- cleanup failures

Closing or deleting a session should have a clear policy for terminating
processes, preserving managed backends, and exporting diagnostics.

## Relationship To Agents

Agents are ordinary same-session participants with extra audit requirements.

For parallel agent work, the target setup is:

```text
agent session
  -> entered shell or command runner
  -> isolated network namespace
  -> DBus proxy or session bus
  -> managed repo/worktree view
  -> Manifold control context
  -> GUI panes and screenshots scoped to that session
```

The useful outcome is that agents can run servers, launch GUI tools, edit files,
use DBus-aware desktop helpers, and capture screenshots without stepping on
each other.

## Things To Avoid

- Do not make session isolation mandatory for every session.
- Do not claim internet-safe hostile-code isolation from these layers.
- Do not let `manifold enter` set only command metadata while GUI apps still
  launch into the host desktop.
- Do not expose the host DBus session bus by default.
- Do not hide writes to system paths; record and report them distinctly.
- Do not make `mctl` a separate product surface with different semantics from
  Manifold control commands.
- Do not require a virtual filesystem before the plain session and command
  context model works.

## Open Questions

- Which isolation profiles should be built in?
- Which paths belong in the first managed filesystem view by default?
- How should managed project changes be promoted back into a dirty Git worktree?
- How should Manifold represent system-path writes that are not owned by a
  project repository?
- How should session-local network namespaces get outbound connectivity?
- Which host DBus names should be allowed through the first proxy profile?
