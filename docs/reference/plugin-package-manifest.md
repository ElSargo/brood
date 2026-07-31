---
status: target
type: reference
authority: implementation
code-links:
  - crates/brood-package-manifest/src/lib.rs
  - packages/default-client.brood-package.toml
---

# Plugin Package Manifest

Brood packages are immutable artifact trees described by a checked manifest.
The package ID is stable across builds; release version and the package-tree
content digest are independent resolution inputs.

Each package declares one root artifact and any number of member artifacts.
Every artifact declares its runtime container kind, Brood ABI, repository build
output, package-relative placement, required capabilities, and dependencies on
other members. Dependency-first installation order is derived from the graph.
The root is the entry artifact loaded by Brood; member artifacts are available
to it at their declared placements without copying files into another plugin's
Cargo target directory.

The schema rejects unknown fields, unsupported versions, invalid or duplicate
identities, escaping paths, colliding placements, incompatible kind/ABI pairs,
unknown or duplicate dependency edges, self-dependencies, and cycles. Exact
fields and accepted enum values are defined by the deserializers and validation
code in `crates/brood-package-manifest/src/lib.rs`.

Package assembly consumes the validated installation order, installs every
artifact at its placement, and records the manifest and root placement beside
the artifacts. The Nix build validates Component Model artifacts before this
assembly step and gives the resulting package tree a content-addressed store
path. A portable package digest and kind inspection in the assembler remain
part of the target resolver boundary. Runtime configuration resolves a package
identity to the declared root. Capability declarations feed the host grant
policy; they are not ambient permissions or a substitute for runtime
enforcement.

The default-client manifest is the first integrated package. Nix assembles its
root and members under `packages/org.brood.default-client`, and the packaged
runtime configuration loads the recorded root from that directory. Members
retain their existing package-root basenames so guest-relative actor starts
continue to resolve without source changes. Other packages remain on the
generic artifact installation path until they gain manifests.
