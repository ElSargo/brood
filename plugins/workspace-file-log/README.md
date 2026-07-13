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

An explicit `rescan` record means the recursive native registration window
closed or the bounded host event queue overflowed. A stateful consumer must
reconcile its state through the granted filesystem when that marker appears.

Run `tools/test-workspace-file-log-plugin-nested.sh` from the repository root.
The test uses Manifold's offscreen nested test topology and leaves the observed
workspace under the printed path. Inspect
`.manifold/workspace-file-log.log`: its `initial`, `created`, `modified`, and
`removed` records are the proof, rather than a plugin-generated success value.
