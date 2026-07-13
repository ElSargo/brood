# Workspace file log plugin

This Preview 1 compatibility plugin creates two logical watches of the workspace
mounted at `.`. Independent workers append the same observable changes to
`.manifold/workspace-file-log.log` and
`.manifold/workspace-file-log.mirror.log`. The primary worker also maintains the
human-readable, sorted workspace model
`.manifold/workspace-file-index.tsv`.

The actor handles its `start` mailbox message by creating a capability-rooted
host watches and spawning one worker for each. Each worker performs one ordinary WASI scan
for `initial` records, then sleeps in the watch notification's host pollable.
Native watch batches record `created`, `modified`, or `removed`; no periodic
rescan is used. `.git`, `.direnv`, `.manifold`, and `target` are excluded so
repository metadata, the log itself, and build output do not feed the watcher.
The line-oriented Preview 1 batch keeps ordinary ASCII paths readable and
represents all other native path bytes with reversible `%HH` escapes.

Normal batches update the index incrementally. A batch containing `rescan` or
a backend-generic `changed` event is ambiguous: the worker discards the whole
batch, recursively scans the granted workspace with ordinary WASI directory
iteration, writes a complete replacement index, and only then appends a
`reconciled` record. Index rewrites happen only when relevant state changes, so
the plugin's own excluded artifacts cannot create a watch feedback loop.

The built module declares a 512 MiB shared-memory maximum. That is a
fault-containment ceiling rather than an expected working set and proves that a
real Preview 1 actor plus its workers can operate under the runtime's aggregate
generation memory limit.

Both workers' native watch events exercise independent notification lanes and
bounded logical queues. `status-json` exposes two subscriptions backed by one
native registration, while the two real logs demonstrate that both consumers
remain useful; the proof also preserves those live counts in
`.manifold/workspace-file-log.watch-status.json` for inspection after the
offscreen daemon exits. This is the observable deduplication proof. A `snapshot`
mailbox message separately makes the actor submit a completion read of the live
log. Its read-completion handler submits the returned bytes as a completion
write to `.manifold/workspace-file-log.snapshot`. The actor verifies the write
completion while its long-lived watcher worker remains active. This deliberately
uses actor completion I/O rather than the worker's ordinary WASI filesystem.

The `probe-watch-teardown` action creates and immediately drops an additional
non-recursive host watch. The nested proof repeats that action while the primary
recursive watcher remains active, verifies that only the primary notification
resource remains, then creates `after-watch-churn.txt`. Its corresponding log
record demonstrates that bounded teardown churn did not strand resources or
damage the user-facing watcher.

An explicit `rescan` record means the recursive native registration window
closed or the bounded host event queue overflowed. The index demonstrates the
required reconciliation rather than merely logging that marker.

Run `tools/test-workspace-file-log-plugin-nested.sh` from the repository root.
The test uses Manifold's offscreen nested test topology and leaves the observed
workspace under the printed path. Inspect
both workspace file logs: their `initial`, `created`, `modified`, and `removed`
records are the watch proof. The controller also checks the live daemon's one
native registration and two logical subscriptions, then sends `snapshot` and
checks that `.manifold/workspace-file-log.snapshot` is a nonempty byte-for-byte
prefix of the append-only log containing the observed create, modify, and remove
records. It creates two invalid-UTF-8 names that lossy conversion would conflate;
distinct `preview1-collision-%FF.txt` and `preview1-collision-%FE.txt` records in
both logs and in the incremental index make lossless compatibility encoding
observable. Rust's Preview 1 directory iterator cannot enumerate those invalid
UTF-8 host names, so the proof removes them through two distinct watch events
before exercising full reconciliation; this limitation does not permit lossy
or colliding index keys.

The controller pauses the primary worker after a file notification while
leaving its notification and watch alive. Its manifest narrows the generation
to three logical watches and 64 queued events per watch. The controller
creates eighty paced files while the mirror drains, then waits for the mirror's
distinct final-path record. That barrier proves every create was routed while
only the paused primary retained more than its queue capacity. Status records
the configured capacities and monotonic overflow increase. The primary log contains a
new `rescan` and no incremental `overflow/` records. Its replacement index is
then compared exactly with an independent walk of the real final workspace,
including the first and last overflow files, a newly created file, a removed
file, and all documented exclusions. These filesystem artifacts are the proof,
rather than a plugin-generated success value.

This plugin is also the user-observable completion-write proof: the snapshot is
created through the same host completion engine whose cancellation and
replacement ownership is tested with a deliberately blocked filesystem
mutation in `completion_io::tests`. The plugin demonstrates the real guest and
daemon path and leaves inspectable bytes; the deterministic host test supplies
the timing barrier which an ordinary regular-file write cannot reliably expose.

Run `tools/test-workspace-file-log-plugin-session-rebind-nested.sh` for the
workspace-grant lifecycle proof. It creates two real surface sessions whose
working directories contain different files, activates each in turn, and checks
the live session ID and working directory through the control API. Each
activation starts a new plugin generation under the same configured actor ID.
The active workspace gains its own log, mirror, and index plus records for
mutations made while it is active; byte hashes prove the inactive workspace's
log remains unchanged. The preserved directories printed by the test make the
absence of cross-workspace files and log records directly inspectable.

The same workload is the typed lifecycle proof. Each start or stop hook captures
the structured host context, delegates an append to a worker with ordinary WASI
filesystem access, and reports lifecycle completion only after that worker
finishes. `.manifold/workspace-file-lifecycle.log` records generation, reason,
related session or generation, instance ID, guest root, and sorted manifest
data. After the nested daemon exits the active workspace ends with
`reason=shutdown`; stable hashes after exit prove the joined generation cannot
append afterward.

The Preview 1 artifact also carries the compatibility half of the runtime-I/O
proof. `tools/test-runtime-io-plugins-nested.sh` sends a dedicated worker a real
loopback peer address and preserves
`.manifold/preview1-runtime-io-transcript.md`. The report contains the
PATH-selected command, pipe stdin and transformed stdout, exit 7, PTY terminal
checks and interaction, the peer response, and distinct timer wake order. The
peer independently records both request and response in
`.manifold/runtime-io-peer.log`.
