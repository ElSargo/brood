---
status: target
type: design
authority: target
---

# Unified executable and build variants

Brood ships one core executable with component-specific modes. The default
Cargo build and Nix package include all core capabilities. A server build
excludes the native presentation client and its dependencies. Additional
variants require a concrete deployment need. Component crates retain their
code boundaries; runtime roles retain separate processes, isolation, and
independent lifetimes. Compatibility command names may select modes without
linking additional executables.

Build capabilities are discoverable from the executable. Requesting an omitted
capability produces an explicit explanation. The same supported capabilities
have the same command semantics in every variant.

The complete development build refreshes the executable and all guest plugins
from current sources, validates component artifacts, and assembles the default
client package. Fresh means dependency-aware rebuilding and package assembly,
not discarding usable compilation caches. Guest artifacts remain separately
loadable and replaceable; one executable does not mean embedding every plugin
or external runtime into it.

Optimization priorities are supported build variants, incremental development
and reliable complete builds, ordinary Cargo and Nix build time, then executable
size. Runtime performance and profiling usability remain constraints. Release
size reductions must not silently disable required runtime capabilities.
