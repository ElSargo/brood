# Managed Session Isolation Plan

This document is the concrete implementation plan for Manifold's `managed`
session profile. It records the intended build order, interfaces, and
verification targets. The target behavior is described in
[Session Environments](session-environments.md); this page is about how to get
there.

## Target

`manifold enter --managed` should create or reuse a Manifold session whose programs
see the normal project and system paths, but whose regular writes, runtime IPC,
DBus names, and loopback services stay inside the session unless explicitly
bridged or promoted.

The first useful result is not hostile-code containment. It is repeatable local
agent work without accidental cross-session or host contamination.

## Concepts

- `SessionIsolationProfile`: `plain`, `managed`, `strict`, or `host-write`
- `ManagedPath`: a view path, lower path, upper path, class, and policy
- `ManagedRuntime`: session-owned runtime directory, temp directory, and socket
  roots
- `ManagedNetwork`: session network namespace metadata and bridge status
- `ManagedBus`: session DBus bus or proxy address and host bridge grants
- `ManagedEffect`: recorded filesystem, runtime, DBus, network, or process
  effect with enough provenance to explain and clean it up

Effect classes:

- `project`: source tree writes, promotable through git-aware diff/apply
- `external`: durable non-project writes, promotable only path by path
- `runtime`: sockets, locks, pid files, temporary files, and caches
- `bridge`: explicit host service access
- `denied`: blocked paths or operations

## Backend Shape

The filesystem implementation should be backend-neutral from the control layer.
The first backend can be a simple managed upper tree rooted at:

```text
~/.manifold/worktrees/<session-id>/
```

Suggested layout:

```text
project/<project-key>/       captured project tree
absolute/<escaped-path>/     captured absolute-path writes
runtime/                     XDG_RUNTIME_DIR, temp roots, socket roots
meta/session.json            profile, lower roots, backend version
meta/effects.jsonl           append-only effect log
exports/                     patches and bundles created by export commands
```

The backend should prefer reflink materialization for project trees when it is
available, then fall back to a normal copy. Later backends may use overlayfs,
FUSE, user namespaces, or other path-interception mechanisms without changing
the command model.

## Implementation Phases

### Phase 1: Session Metadata And CLI Surface

- Add an isolation profile to session state.
- Add `manifold enter --managed` and a parser-level spelling for `--plain`,
  `--strict`, and future profile selection.
- Record the project view path, lower path, upper root, and profile in session
  metadata.
- Export public environment variables for diagnostics, including the profile,
  upper root, runtime root, and session DBus address when present.
- Add typed status output for the active session's isolation state.

Verification:

- `manifold enter --managed env` prints the session id, profile, upper root, runtime
  root, DBus address, and Wayland display.
- `manifold show`, `hide`, and regular control commands still target the entered
  session.

### Phase 2: Project Write Capture

- Build the first managed worktree backend.
- Materialize the project upper tree lazily on first project write.
- Prefer reflink copy, then fall back to ordinary copy.
- Keep the view path as the original project path inside the session.
- Track changed project files and expose `manifold worktree status` and
  `manifold worktree diff`.

Verification:

- Two managed sessions in the same project can modify the same file without
  changing each other or the host lower tree.
- Diffs are reported relative to the lower project path.
- Deleting and recreating files is represented accurately enough for promotion.

### Phase 3: Absolute Path Capture

- Generalize path capture from the project root to arbitrary regular absolute
  paths.
- Classify captured paths as `project`, `external`, `runtime`, `bridge`, or
  `denied`.
- Add a default denylist for special and sensitive paths.
- Record every captured external write with lower path, upper path, class, and
  operation metadata.
- Extend status output to show external writes separately from project writes.

Verification:

- A managed command that writes `/etc/whatever` sees its own changed file inside
  the session while the host file is unchanged.
- Writes to denied paths fail with explainable errors and are visible in logs.
- External writes appear separately from project diffs.

### Phase 4: Runtime Directory And Filesystem IPC

- Create a session runtime directory under the managed upper root.
- Set `XDG_RUNTIME_DIR`, `TMPDIR`, and related environment variables for entered
  commands and managed apps.
- Redirect or mount selected runtime path prefixes so filesystem sockets,
  pid files, and lock files stay session-local.
- Classify these effects as `runtime` and exclude them from normal promotion.

Verification:

- Two sessions can run an app that creates the same runtime socket name without
  colliding.
- Runtime files are visible in diagnostics but omitted from project/external
  promotion by default.

### Phase 5: DBus And Host Bridges

- Use the existing session-local DBus bus/proxy path for managed clients.
- Make `DBUS_SESSION_BUS_ADDRESS` point at the session bus in managed sessions.
- Add bridge metadata for allowed host services.
- Keep host bus access explicit, logged, and capability-scoped.

Verification:

- Same-session clients can own names and exchange messages through the session
  bus.
- Cross-session DBus access fails unless bridged.
- Bridged host notifications or portal calls are visible in the effect log.

### Phase 6: Network And IPC Namespaces

- Add a session network namespace with its own loopback interface.
- Launch entered commands and managed apps inside that namespace.
- Add outbound connectivity through a later bridge, NAT, `pasta`, or
  `slirp4netns` path.
- Add IPC namespace support where needed for abstract Unix sockets and SysV/POSIX
  IPC.

Verification:

- Two managed sessions can bind the same `127.0.0.1` port.
- A service started inside one session is not reachable from another session
  unless bridged.
- Abstract Unix socket collisions are isolated when IPC namespace support is
  enabled.

### Phase 7: Export, Promote, And Cleanup

- Add `manifold worktree export`, `promote`, and `discard`.
- Promote project changes through git-aware patch application with conflict
  checks.
- Promote external changes only through explicit path-by-path operations.
- Treat runtime effects as discard-by-default.
- Make session deletion preserve enough metadata to understand what was thrown
  away or exported.

Verification:

- Project promotion detects lower-tree conflicts instead of overwriting them.
- External promotion requires explicit paths.
- Discard removes captured writes and runtime files without touching host lower
  paths.

## Open Decisions

- Which path-interception backend should become the first transparent
  same-path implementation: overlayfs in a user namespace, FUSE, `proot`-style
  interception, or a smaller launcher-specific prototype?
- What is the exact default denylist for `managed`?
- Which host bridges are needed first: Wayland, PipeWire, portals,
  notifications, clipboard, credentials, or browser/devtools?
- How much of the effect log belongs in the in-memory session state versus
  append-only files under the managed upper root?
