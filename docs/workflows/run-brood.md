---
status: canonical
type: workflow
authority: product
last-reviewed: 2026-07-24
---

# Run Brood

From the repository root, prefer the checked-in Nix development shell:

```sh
nix develop --command cargo run
```

The user-facing command starts `brood-client`. The client discovers or
starts `brood-sessiond`, completes the presentation handshake, loads its
presentation actors, and opens the WGPU viewport.

The client first reconnects to the configured local control target. When that
backend already exists, it reuses it and imports its complete session
catalogue. It starts a backend only when the target is absent; reopening the
client does not create a replacement backend or session.

The session service may also run independently for service management and
backend development. It never opens a presentation window. Exact runtime,
configuration, and recovery options are defined by the CLI parser and linked
from [[../reference/cli]].

Use [[test-nested]] for automated behavior validation.

Related:

- [[../systems/runtime]]
- [[../systems/configuration]]
