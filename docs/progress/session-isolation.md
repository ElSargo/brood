---
status: progress
type: implementation-log
authority: implementation
last-reviewed: 2026-08-21
---

# Session Isolation Implementation Progress

This page tracks delivery of [[../targets/systems/session-isolation]]. It
records the foundation present in the working tree and does not imply that
ordinary Brood sessions are isolated yet.

## Milestone status

The public status model, durable volume-record store, reflink-first managed
worktree provisioning, and a capsule broker foundation now exist. They are not
yet connected into one session-creation or process-routing path.

Current sessions remain explicit `direct` sessions. They publish no volume or
capsule identity, do not start `brood-capsuled`, and continue to launch through
the existing host-native paths. Isolated creation is not the default and there
is no silent direct fallback.

## Implemented foundations

### Public session model and catalogue propagation

- The transport-neutral model defines isolated and direct profiles, stable
  128-bit volume IDs, the four target volume lifecycle states, capsule
  lifecycle and failure status, and structured preparation, runtime, and
  cleanup failure stages.
- Volume IDs use a fixed-width hexadecimal wire form so JSON consumers do not
  lose integer precision.
- Session isolation status crosses the local shared catalogue, remote
  catalogue transport, client session state, bundled backend catalogue, and
  component-plugin session ABI.
- Older JSON catalogue entries without the new status field deserialize as
  direct sessions without a volume or capsule. The changed Postcard catalogue
  layout advances the presentation protocol version from 22 to 23.
- Network and bundled-catalogue validation bound capsule failure text.

### Durable session-volume records

- `broodd::session_volume` owns a schema-versioned JSON manifest store rooted
  at a caller-selected directory. It generates nonzero random 128-bit IDs and
  writes manifests through a synced temporary file, atomic rename, and
  directory sync.
- Manifests record creation, last-use, retained, graveyard, and purge
  timestamps, lifecycle, pin state, isolation profile, and working directory.
- The validated transition graph supports close, reopen, discard, restore, and
  forward retention transitions. Timestamp updates are monotonic even if the
  wall clock moves backwards.
- Startup discovers valid records, moves malformed, unsupported, or mismatched
  entries into a recoverable quarantine, and writes durable structured
  diagnostic sidecars. A stale `active` record from an earlier daemon run is
  recovered as `retained`.
- Read-only ordered list, lookup, and startup-diagnostic APIs are available for
  later session integration. An injected clock drives retention tests.
- Automatic policy advances an unpinned retained record after seven days and
  an unpinned graveyard record after 30 further days. Pinned records do not
  advance automatically.

The store is not opened by the daemon startup path yet. Storage-manager policy
ownership has not moved to the daemon-init actor, and current direct sessions
are intentionally not assigned synthetic volume records.

`Purged` is currently a durable manifest transition only. No native deletion
executor removes the volume directory or its data, so the current code must
not present that state as proof that bytes were physically erased.

### Reflink-first managed worktree provisioning

- Provisioning now carries the actual requesting worktree and resolves its
  `HEAD`. A new branch starts there; an existing branch retains its own head.
- The common same-HEAD path registers a provisional `--no-checkout` worktree,
  enumerates the requesting worktree's top-level roots while excluding `.git`,
  makes one recursive reflink-only copy directly into the provisional target,
  builds the index, normalizes dirty tracked paths from Git, prunes unselected
  source-only paths in bounded batches, and verifies a clean result.
- When source and target heads match, ignored seeding uses ignore rules from
  the target commit. It excludes non-ignored untracked paths, `.git`,
  submodule contents, and special files.
- A provisional marker makes interrupted acquisition repairable. Any required
  reflink failure rolls the Git registration back before ordinary `git
  worktree add` fallback; it does not byte-copy the tree.
- Asynchronous provisioning completion immediately publishes the new active
  session catalogue before notifying the session switcher. The switcher keeps
  the returned backend/session identity if that completion arrives before its
  catalogue lane, so the new worktree row appears and becomes active without
  an unrelated DBus request forcing a refresh.
- The header `New` action now presents Git worktrees as first-class creation
  choices before backend-directory choices. Selecting one returns focus to the
  switcher's branch editor instead of the workspace, and the project-row
  shortcut is labelled `+WT` rather than sharing the header's ambiguous `+`.
- The first asynchronous backend catalogue can contain an empty active-backend
  row after the live session snapshot has already arrived. The switcher now
  retains that newer snapshot until a non-empty catalogue replaces it, and
  combines Git capability across every session in a project. This keeps `+WT`
  present on the first rendered project row regardless of message or row order.
- Failed creation keeps the branch editor and proposed name available for a
  retry, restores the action button from its progress state, and renders a red
  `Worktree creation failed` reason. Missing error details get an explicit
  fallback, stale completions cannot overwrite a newer operation, and an
  unexpected backend-init actor stop fails every outstanding request instead
  of leaving the switcher waiting indefinitely.
- A live Brood-checkout acquisition exposed a usability and latency problem:
  reflinking 3.5 GiB across roughly 10,000 paths took about 24 seconds, the
  switcher showed only an ellipsis, and the typed DBus bridge timed out after
  15 seconds even though provisioning subsequently succeeded. The per-path
  copy loop was replaced by the bulk transaction above. After the verification
  builds grew the checkout to 21 GiB apparent size and 27,058 entries, its raw
  recursive reflink completed in 1.35 seconds on the same Btrfs storage. The
  switcher explicitly reports that creation and reflinking are in progress,
  and only the managed-worktree DBus operation has a 120-second deadline.

This is temporarily a bounded, shell-free subprocess-backed mechanism inside
the worktree-manager plugin. Git and filesystem operations, including
`cp --reflink=always`, cross the existing plugin subprocess API. The common
path now uses a bounded number of processes plus bounded classification and
pruning batches, while the different-HEAD compatibility path remains
per-file. The target native storage-manager mechanism boundary and daemon-init
policy ownership are not yet implemented.

### Capsule protocol, broker, and plan foundation

- The packaged `brood-capsuled` crate defines a bounded versioned Postcard
  protocol over Unix `SOCK_SEQPACKET`, including bounded descriptor passing
  with `SCM_RIGHTS` and pidfd responses.
- The implemented request set covers ping, spawn, signal, wait, and shutdown.
  Spawn requests carry byte-safe program, arguments, cwd, environment delta,
  three descriptor-selectable standard streams, disposition, and launch
  metadata.
- The broker owns child process groups, reports exact exit or signal status,
  keeps blocking waits from blocking later signal requests, reaps children,
  and performs bounded graceful then forced termination. Capsule-init mode
  refuses to run unless it is PID 1; direct mode supports current foundation
  tests without claiming isolation.
- Pure plans describe the required namespaces, explicit mount entries,
  OverlayFS constraints, cgroup delegation, accounting, service identity, and
  termination grace. Invalid or unsafe plans are rejected before preparation.
- The host adapter builds an argument-safe transient `systemd-run --user`
  service around Bubblewrap, validates its executable closure and private
  broker runtime directory, waits for service state plus a protocol `Ping`, and
  owns bounded broker shutdown, service stop, failure reset, and socket cleanup.
- The binary is included in the workspace and Nix package outputs.

The adapter builds pasta as a separately supervised, no-inbound-forwarding
plan but does not launch it yet. A prepared capsule therefore has a private
network namespace without the target outbound connectivity. No daemon path
starts or connects to the broker, and no session process uses it yet.

The broker protocol does not yet implement PTYs, terminal resize, TCP connect,
listen, or accept. Those are target requirements rather than implemented
capabilities.

## Verification evidence

| Area | Nix-backed command or workload | Result |
| --- | --- | --- |
| Public model | `cargo test -p brood-protocol` | 14 tests passed, including stable-ID encoding, old-catalogue defaults, and structured status round trips |
| Catalogue consumers | Focused checks for `brood-wasm`, `brood-client`, `brood-transport`, and `broodd` | Passed |
| Bundled catalogue actor | WASM check of the session-switcher plugin | Passed with the extended component ABI |
| Network catalogue | `cargo test -p brood-transport` | 13 tests passed |
| Volume store | `cargo test -p broodd --test session_volume` | 3 tests passed: atomic restart recovery, exact fake-clock retention and pinning, and durable quarantine diagnostics |
| Reflink provisioning | `cargo test --manifest-path plugins/worktree-manager/Cargo.toml`, `tools/test-worktree-manager.sh`, focused all-target Clippy, WASM check, a live 21-GiB/27,058-entry bulk-copy benchmark, and real Btrfs extent inspection | 6 unit tests and the daemon-backed manager proof passed; the common path issued exactly one recursive reflink call, the live checkout copy completed in 1.35 seconds, and `filefrag` reported shared extents. Fallback, repair, path-safety, dirty-source normalization, branch-head, ignored-file, symlink, submodule, and special-file cases passed. The sixth test classified all 320 committed-ignored paths whose candidates totalled more than 64 KiB while proving every `check-ignore` request stayed within 16 KiB |
| Session-switcher worktree path | `BROOD_WORKTREE_CREATION_ONLY=1 tools/test-session-switcher-plugin-nested.sh`, `BROOD_WORKTREE_FAILURE_ONLY=1 tools/test-session-switcher-plugin-nested.sh`, and focused unit/Clippy checks | The success path passed through the public header `New` button, first-class worktree choice, restored branch-editor focus, provisioning, catalogue publication, active-session switching, and client-visible rendering without DBus polling before observation. The real failure path submitted an invalid Git branch, received a structured backend failure, returned the action from progress to `Create`, visibly rendered the red failure reason, and created no session. The switcher unit suite passed 33 tests, including empty-initial-catalogue retention, order-independent project Git capability, and failure-state behavior |
| Capsule foundation | `cargo test -p brood-capsuled --lib` | 11 tests passed for plan rejection, host invocation policy, version failure, pidfd and exact-exit round trips, live wait/signal multiplexing, process-group termination, and PID-1 support logic |
| Capsule host adapter | `cargo test -p brood-capsuled --test host_smoke -- --ignored --nocapture` | 1 real Linux user-systemd and Bubblewrap smoke passed: readiness, capsule init as PID 1, spawned probe as PID 2, a distinct mount namespace, private `/tmp`, broker spawn/wait, clean unit and socket teardown, and typed fail-closed rejection of a missing Bubblewrap executable |

Targeted formatting and diff checks pass. The reflink and capsule lanes also
pass their focused all-target Clippy checks with denied warnings. A
repository-wide all-target Clippy invocation is not evidence for this milestone
because unrelated pre-existing targets currently violate denied lints.

## Remaining integration and proof gaps

- Move durable volume and worktree policy ownership into the daemon-init actor,
  select the product storage root, import existing worktree records, and make
  one daemon instance the enforced store owner.
- Version the component WIT package when external plugin compatibility becomes
  a supported contract. The bundled components are rebuilt together today,
  but an independently cached component compiled against the earlier in-place
  session-summary record will not type-match the extended record.
- Prepare a volume and capsule before publishing an isolated session, attach
  their real status to the catalogue, and implement reopen without reusing a
  daemon-generation session ID or stale runtime sockets.
- Integrate the systemd/Bubblewrap adapter with daemon lifecycle, extend host
  proof to delegated controllers, accounting, and whole-cgroup orphan cleanup,
  then launch and supervise pasta plus the required host bridges.
- Extend the spawn protocol with PTYs, resize, and in-capsule TCP operations.
- Introduce the shared process-backend abstraction and route entered commands,
  hosted applications, plugin subprocesses and PTYs, Chromium, and computer-use
  launches through the selected session backend.
- Replace subprocess-backed reflink and filesystem mutation with the intended
  validated native storage mechanism while preserving transactional ordinary
  Git fallback.
- Implement volume inspection, changed-path export, physical purge, restore,
  pin management surfaces, and cleanup-failure retention. There is no volume
  CLI or DBus management API yet.
- Make isolated creation the default only after every creation path can fail
  closed and interactive direct retry is explicit.

The focused host smoke proves the capsule adapter boundary described above; no
end-to-end Brood session isolation acceptance workload has run. The target
proof still needs two actual sessions with same-port, DBus-name, runtime-socket,
and temporary-path collisions; host and sibling filesystem non-contamination;
complete namespace and cgroup process boundaries; orphan cleanup; PTY and
exact-exit behavior; nested rootless Podman; retained-volume reopen; and real
GUI workloads through the supported offscreen `brood test` path.
