# Workspace file log plugin

This Preview 1 compatibility plugin watches the workspace mounted at `.` and
appends changed file names to `.manifold/workspace-file-log.log`.

The actor handles its `start` mailbox message by creating a capability-rooted
host watch and spawning a worker. The worker performs one ordinary WASI scan
for `initial` records, then sleeps in the watch notification's host pollable.
Native watch batches record `created`, `modified`, or `removed`; no periodic
rescan is used. `.git`, `.direnv`, `.manifold`, and `target` are excluded so
repository metadata, the log itself, and build output do not feed the watcher.

An explicit `rescan` record means the bounded host event queue overflowed. A
stateful consumer must reconcile its state through the granted filesystem when
that marker appears.
