# Workspace file log plugin

This Preview 1 compatibility plugin watches the workspace mounted at `.` and
appends changed file names to `.manifold/workspace-file-log.log`.

The actor handles its `start` mailbox message by spawning a worker. The worker
uses ordinary blocking-looking WASI filesystem and clock calls; the actor does
not wait for it. The initial scan records `initial`, then later scans record
`created`, `modified`, or `removed`. `.git`, `.direnv`, `.manifold`, and
`target` are excluded so repository metadata, the log itself, and build output
do not feed the watcher.
