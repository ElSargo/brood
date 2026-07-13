# Workspace file log plugin

This Preview 1 compatibility plugin watches the workspace mounted at `.` and
appends changed file names to `.manifold/workspace-file-log.log`.

The actor handles its `start` mailbox message by creating a capability-rooted
host watch and spawning a worker. The worker performs one ordinary WASI scan
for `initial` records, then sleeps in the watch notification's host pollable.
Native watch batches record `created`, `modified`, or `removed`; no periodic
rescan is used. `.git`, `.direnv`, `.manifold`, and `target` are excluded so
repository metadata, the log itself, and build output do not feed the watcher.

The built module declares a 512 MiB shared-memory maximum. That is a
fault-containment ceiling rather than an expected working set and proves that a
real Preview 1 actor plus its workers can operate under the runtime's aggregate
generation memory limit.

The worker's native watch events exercise the notification lane. A `snapshot`
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
closed or the bounded host event queue overflowed. A stateful consumer must
reconcile its state through the granted filesystem when that marker appears.

Run `tools/test-workspace-file-log-plugin-nested.sh` from the repository root.
The test uses Manifold's offscreen nested test topology and leaves the observed
workspace under the printed path. Inspect
`.manifold/workspace-file-log.log`: its `initial`, `created`, `modified`, and
`removed` records are the watch proof. The controller then sends `snapshot` and
checks that `.manifold/workspace-file-log.snapshot` is a nonempty byte-for-byte
prefix of the append-only log containing the observed create, modify, and remove
records. Those filesystem artifacts are the proof, rather than a
plugin-generated success value.

This plugin is also the user-observable completion-write proof: the snapshot is
created through the same host completion engine whose cancellation and
replacement ownership is tested with a deliberately blocked filesystem
mutation in `completion_io::tests`. The plugin demonstrates the real guest and
daemon path and leaves inspectable bytes; the deterministic host test supplies
the timing barrier which an ordinary regular-file write cannot reliably expose.
