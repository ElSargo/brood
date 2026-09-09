---
status: current
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
path. Runtime configuration resolves a package identity to the declared root.
Capability declarations feed the host grant policy; they are not ambient
permissions or a substitute for runtime enforcement. The missing portable
package digest and assembler kind inspection are tracked as a design friction
instead of being presented as implemented reference behavior.

The default-client manifest is the first integrated package. Nix assembles its
root and members under `packages/org.brood.default-client`, and the packaged
runtime configuration loads the recorded root from that directory. Members
retain their existing package-root basenames so guest-relative actor starts
continue to resolve without source changes. Other packages remain on the
generic artifact installation path until they gain manifests.

Manifests may also carry the optional self-description model defined in
`crates/brood-package-manifest/src/extension.rs`. The local planning/application
API in `src/installation.rs` snapshots build inputs, verifies output artifacts,
installs their knowledge together, and computes portable content digests. Its
CLI adapter is defined in `crates/brood-cli/src/package.rs`. Selection in the
configured default store is consumed by package-aware daemon, session, and
presentation init actors. Immutable installation records describe pending activation; live
endpoint observations are required to establish the actual outcome. The legacy
assembler rejects this optional extension instead of omitting its knowledge.
Coverage and remaining target work are recorded in
`docs/progress/self-describing-extensions.md`.

Extension dependencies name other packages and source-relative manifests.
Plans capture the whole installation graph; applying a plan builds dependencies
first and preserves all selections if a build fails. Activation requires each
exact dependency installation to remain selected, enabled, and compatible with
its retained state. Disabling or removing a dependency therefore makes dependent
actors pending until the requirement is satisfied again. This graph does not
implicitly expose dependency artifacts to build commands or grant service access.

For a required service, `disconnect = "stop"` makes the configured init policy
retire a previously bound consumer when that service becomes unavailable. It
remains pending through teardown and can start again when a compatible provider
returns. `pending` leaves the consumer alive with an unbound service. Remote
service observations expire after a lease, so an open transport alone does not
keep an unresponsive endpoint's bindings ready.

The common introspection request and response types live in
`crates/brood-package-contract/src/introspection.rs`. Its discovery collection
publishes the versioned envelope schemas and supported collections. The package
CLI adapter and connected control handler use the same request format. Local
queries verify installed packages without starting plugin code; live queries
require a connected endpoint. Each collection is paginated independently with
a byte bound and observation revision. A changed revision rejects continuation
instead of combining inconsistent pages. Remote records retain their reported
authority and connection status.

Package guests use the same introspection request through the versioned service
mailbox contract. Replies carry a caller-supplied correlation ID. The host binds
the request to the actual actor generation and filters local records to its
realm and scope before computing pagination or revision identities. Installed
package descriptions are limited to packages represented in that scope. A
binding may describe its actual peer, but local package authority does not grant
an endpoint-wide remote inventory. That inventory requires an operator query.

Runtime I/O resource observations include resources, streams, children, and
timers, with their own observation timestamp. Guest queries sample the host
runtime immediately; periodic service observations refresh connected reports.
Samples are matched to actor generation. Retired instances clear these live
measurements, and changes to the observation timestamp alone do not invalidate
pagination revisions.

Optional diagnostic declarations give package-namespaced IDs, positive schema
versions, and JSON Schema source files. Those files are captured and installed
with the package knowledge. Guests can report a value or an explicit diagnostic
failure through the service mailbox. The host checks the reporting generation,
installation, declaration, version, schema, and size bounds. Diagnostic records
identify plugin authority, host validation outcome, schema digest, and report
time. A rejected value produces a failed diagnostic without changing actor
lifecycle. Declared diagnostics begin as not reported, and the common query API
can page diagnostics independently of standard instance facts.

Packages can declare acceptable versions of named host interfaces independently
of artifact container ABI. The portable contract defines the host interfaces
implemented by this build, and introspection discovery exposes them. Planning
reports incompatibility without discarding captured source inputs. An immutable
installation may be retained on an incompatible host, but activation remains
pending with the unsupported interface or version as its reason. The same gate
applies through installed dependencies; a root cannot bypass it by resolving
the selected artifact directly.

Build recipes include a timeout in their resolved manifest contract. Unix builds
run in an owned process group; apply retires that group on failure or timeout
and before validating successful outputs. Produced artifacts must be regular
files and fit the installation output budget. Timeout and output failures keep
the previous selections and preserve attempt logs. This controls the declared
build process group; build code retains the local user's authority.

Stdout and stderr are drained concurrently into separate preserved logs. Each
stream has a declared byte limit; exceeding it fails the build and retains the
captured prefix. Capture errors also stop the build. Unix readers use
nonblocking pipes so a detached helper retaining a pipe cannot indefinitely
hold apply open after the declared build has ended.

Store mutations use OS-managed file locks. The persistent lock file is part of
the store and must not be removed while clients are using it. Graph apply records
a publication journal only after all builds succeed. If publication is
interrupted, a later mutation or explicit recovery completes that recorded graph
after re-verifying its artifacts, dependency order, and local build receipts.
Catalogue readers reject an unfinished publication; live hosts retain their last
verified catalogue until recovery completes. Legacy directory-based lock markers
are reported for operator inspection rather than silently removed.

Each activation declaration may include expected resource use using the portable
contract's resource IDs and units. These expectations are retained in plans,
knowledge, and live actor declarations. Instance observations compare measured
use with the expectation and explicitly identify unsupported measurements.
Expectations support sizing and diagnosis; capability grants and enforced host
limits remain separate policy inputs.

A manifest can offer ordered prebuilt alternatives, each with an identity, host
interface requirements, and a complete mapping from artifact IDs to local paths
and SHA-256 digests. Planning chooses the first compatible alternative whose
files are available, captures its bytes along with documentation and schemas,
and records the choice in the plan identity. Missing or incompatible alternatives
fall back to the declared build recipe. A selected alternative with corrupt
bytes fails resolution rather than silently falling back. Paths remain confined
to the manifest's source directory.

Prebuilt plans require neither build input files nor the declared build
executable to be available. Apply never reselects an alternative or rereads the
source directory. It verifies the captured artifacts and schemas through the
same installation path as built outputs. The installed knowledge records the
selected alternative; destination authorization and root activation rules still
apply. Exact fields and validation are defined in the manifest's `extension.rs`.

The common introspection API exposes the answering executable's bundled
metadata and documents independently of its package store and live actors.
Document queries support exact stable IDs and case-insensitive content/ID search.
Large documents are split at UTF-8 boundaries into ordered parts with content
and bundle digests. Normal response pagination reconstructs the complete document
without exceeding mailbox limits; revision checks protect multi-page reads.
Search returns every part of each matching document. The generated command
reference is available as `brood.commands.v1`. Bundled target, progress, friction,
reference, and generated-contract categories remain explicit. Offline queries
identify the querying binary; connected and guest queries identify the endpoint
whose executable supplies the bundle.

Installed package descriptions and documents are independent common API
collections. Requests name both the package ID and exact installation digest;
responses retain its knowledge digest and artifact identities. Documents include
authored entries and the installed manifest, interface schemas, configuration
schema, and diagnostic schemas. Contract document IDs `manifest` and `schema:`
are reserved by manifest validation. The same chunking, search, and pagination
rules apply as for executable documents.

Runtime endpoints serve selected knowledge already verified by the catalogue
worker, without filesystem reads during a presentation query. Guest access
requires that exact installation to be represented in the caller's realm and
scope. An unavailable or replaced revision is an explicit error. The local store
query supports historical immutable revisions, including after removal, without
running the package or consulting its original source directory.

Discovery's request/response schemas and the separately indexed activation,
policy, peer, service, and live-instance schemas are generated from their Rust
serde types. Stable schema document IDs appear in discovery and are retrievable
through the common document API or offline export. Generated entries contribute
to the exported bundle digest. These are structural contracts; decoder checks,
resource limits, destination authorization, and realm visibility still enforce
runtime semantics. Schema generation is an optional feature of the portable
contract crate, enabled by the host CLI without requiring it in guest builds.

Configuration references are generated from the types used by the parsers:
`brood.realm-init-config.v1` describes the shared init/client selection parser,
and `brood.runtime-config.v1` describes backend session runners, startup modules,
and startup applications. Their definitions live in `brood-cli/src/realm_init.rs`
and `brood-cli/src/configuration.rs`; backend loading and semantic validation
remain in `broodd/src/config.rs`. They are separate views of the same TOML
configuration, matching the existing parser boundaries. Generated schemas record
the owning Rust type as well as their stable document ID.

The executable bundle checks internal wiki references outside code spans and
fences. Referenced text assets are included under their stable paths; unresolved
references or duplicate IDs fail the build. One registry associates each generated
schema with its actual Rust type and bundled source document, and knowledge export
checks that all registered schema documents carry the expected associations.

Package documents may declare stable-ID references and associated schema document
IDs. Manifest validation rejects unresolved declarations. Installation and import
also require the knowledge's manifest, document set, categories, associations,
schema sets, and artifact/installation identities to agree with the installed
manifest and receipt. Recomputing bundle digests does not excuse missing required
knowledge. Common document queries preserve these associations for callers.
