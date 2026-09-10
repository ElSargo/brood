# Unified build implementation

The accepted target is [Unified executable and build variants](../targets/systems/unified-build.md).

Baseline inspection on 2026-09-10 found a 137 GiB local host target directory.
Existing debug executables included a 776 MiB command entry point, 972 MiB
presentation client, 766 MiB session entry point, and 617 MiB runner-exec.
These are existing artifacts, not controlled build-time measurements.

Implementation consolidates core roles behind one executable while retaining
library boundaries and separate processes. Legacy names remain launch aliases.
The server feature selection excludes the native presentation dependency.
Development line tables replace full debug information; release artifacts drop
debug information while the profiling profile retains it.

Verification and measurements will be recorded after the build migration.

Initial verification:

- Nix-backed full host build succeeded after the linker/profile transition in
  1m38s (partially warm environment, not a controlled clean-build comparison).
- The full and server Nix executable packages built successfully. Initial
  stripped release sizes were about 95 MiB and 68 MiB respectively.
- The server dependency tree excludes brood-client, WGPU, Winit, and cosmic-text.
- Executable integration tests cover explicit component modes, legacy argv[0]
  aliases, capability reporting, and omitted presentation support.
- The four MCP adapter tests pass. Full-host Clippy passes with warnings denied.
- A repeated complete host/guest build took 1.528 seconds, with zero component
  encodings. Guest outputs and default client package were checked and assembled.

The first guest build was interrupted by editing the running builder script;
its completed artifacts were retained, and a subsequent complete build passed.
This was a verification workflow error, not a guest compiler failure.

The default Nix distribution reuses its full executable in deployment bundles,
so it does not implicitly compile a second server variant. Explicit specialized
backend packages still use the server build. Dependency-cache derivations do not
carry whole-checkout provenance; the final executable does. Guest source filtering
excludes host implementation and documentation edits from plugin rebuild inputs.

Additional verification and limitations:

- Full and server executable-mode integration tests both pass. Server Clippy
  also passes. The default full build is restored after server verification.
- The checkout-hidden, network-isolated knowledge proof passed with 192 indexed
  entries, validating compressed-bundle reconstruction and generated schemas.
- MCP stdio initialization and direct capsule broker socket readiness/termination
  passed. The MCP smoke environment supplied the required native-tool socket
  setting; initialization does not invoke a native tool.
- The offscreen native-client capture lane produced changing SHM and DMA-BUF
  screenshots, inspected visually. Its initial metrics recorded ten capture
  attempts, zero capture failures, and four accepted DMA-BUF imports.
- Earlier runs under concurrent compilation failed. The terminal proof exposed a startup
  focus race, then terminal guest traps in styled text rendering: an epoch
  interrupt in the development build and a guest panic in the release build.
  Screenshots confirmed the retired terminal pane. Artifacts are under
  `/tmp/brood-unified-nested-retry` and `/tmp/brood-unified-nested-release`.
- The computer-use proof exposed discovery before the first buffer committed.
  After waiting for usable geometry, it progressed to the action batch but still
  failed there. Captures and diagnostics are under
  `/tmp/brood-unified-computer-use-retry`. The initial run had no baseline comparison; the follow-up results below
  distinguish the remaining verification limit.

The two startup waits now observe the required state rather than assuming that
an earlier layout or window announcement implies readiness. Guest rendering and
computer-use execution semantics were not changed to make these gates pass.
Existing proof builders now use the aggregate host build to obtain compatibility
launchers; profiling invokes the unified profiling mode.


Final verification:

- Both default (including desktop/runtime packaging) and server Nix outputs built.
  Packaged component modes passed, and the default closure contains no server
  variant. The packaged native executables measured 98,883,048 bytes (full) and
  71,201,328 bytes (server), approximately 94.3 MiB and 67.9 MiB.
- Warm Cargo build: 0.322 seconds. Touching only the executable entrypoint and
  rebuilding/relinking: 3.312 seconds. Complete host-plus-plugin refresh: 1.574
  seconds with no component encoding. These are local warm-cache measurements,
  not controlled before/after clean-build benchmarks.
- The final full Rust checks and executable/MCP tests pass. Updated build and
  proof scripts pass ShellCheck with the repository's exclusions, following
  sourced helper files; modified Nix files pass formatting checks.
- After compilation finished, the packaged terminal proof passed at
  `/tmp/brood-unified-idle-terminal`: real pointer/keyboard input, styled Unicode
  output, 256 KiB clipboard delivery, and ordering of a subsequent key. Screenshots
  were inspected, and the client recorded about 801 MiB peak RSS. The cached
  pre-migration debug executables also passed the same proof. Earlier failures
  appear load-sensitive; no runtime deadline or rendering policy was changed.
- The supplemental computer-use gate still stops at its action batch, after
  successful native-client captures. It fails at the same point with the cached
  pre-migration binaries and the assembled unified package when run without
  concurrent compilation. This remains a separate verification limitation.
