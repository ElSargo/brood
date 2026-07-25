# Component workspace watch

This Component Model plugin is the user-observable filesystem-watch proof. It
opens a recursive watch on its configured workspace capability and maintains
`.brood/component-workspace-watch.md` as a live Markdown table. Creating,
editing, and removing workspace files adds typed `created`, `modified`, and
`removed` rows with relative paths. The initial `rescan` row makes registration
coverage visible instead of treating setup as an invisible success condition.
Paths arrive as lossless native bytes. The report leaves simple ASCII names
readable and renders every other byte as an unambiguous `\xNN` escape.

The guest `FilesystemWatch` wrapper owns the WIT resource with RAII and turns
its generation notification into the ordinary worker pollable used by
`worker-wait-any`. It does not add a watcher-specific wait primitive. Dropping
the wrapper drops the logical subscription, notification, and any projected
pollables with the actor generation.

The acceptance workload performs real workspace mutations, checks the
human-readable artifact and server resource counts, then replaces the Component generation
and observes the old generation's watch counts return to zero. Its mutations
include two Unix names whose different invalid UTF-8 bytes both lossy-render as
the same replacement character; distinct `\xFF` and `\xFE` rows make preserved
identity visible in the artifact.
