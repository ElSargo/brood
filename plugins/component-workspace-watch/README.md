# Component workspace watch

This Component Model plugin is the user-observable filesystem-watch proof. It
opens a recursive watch on its configured workspace capability and maintains
`.manifold/component-workspace-watch.md` as a live Markdown table. Creating,
editing, and removing workspace files adds typed `created`, `modified`, and
`removed` rows with relative paths. The initial `rescan` row makes registration
coverage visible instead of treating setup as an invisible success condition.

The guest `FilesystemWatch` wrapper owns the WIT resource with RAII and turns
its generation notification into the ordinary worker pollable used by
`worker-wait-any`. It does not add a watcher-specific wait primitive. Dropping
the wrapper drops the logical subscription, notification, and any projected
pollables with the actor generation.

Run the nested proof from the repository root:

```sh
nix develop --command tools/test-component-workspace-watch-plugin-nested.sh
```

The controller performs real workspace mutations, checks the human-readable
artifact and daemon resource counts, then replaces the Component generation
and observes the old generation's watch counts return to zero.
