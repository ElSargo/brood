---
status: friction
type: design-input
authority: implementation
---

# Portable Package Resolution

The checked package manifest and Nix assembly path are implemented, but the
assembler does not yet provide a portable package digest and artifact-kind
inspection at the resolver boundary.

This leaves content identity and artifact validation partly dependent on the
Nix store path. The target package resolver needs to decide where portable
digest calculation, kind inspection, and rejection belong without duplicating
the manifest validator or runtime capability checks.

Current implemented behavior is documented in
[[../reference/plugin-package-manifest]].
