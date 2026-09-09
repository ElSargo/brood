---
status: partial
type: progress
authority: implementation
last-reviewed: 2026-09-09
---

# Self-describing extensions implementation

The target in `docs/targets/systems/self-describing-extensions.md` remains
unchanged. This is a partial implementation, not completion of its end-to-end
acceptance criteria.

## Implemented

The existing package manifest accepts an optional versioned extension section.
Its typed model describes source inputs, an explicit Nix-store build executable,
output artifacts, documentation, JSON interface/configuration schemas, realm
activation declarations, capability expectations, and state/migration semantics.
The definition and validation live in
`crates/brood-package-manifest/src/extension.rs`; exact CLI grammar lives in
`crates/brood-cli/src/package.rs`.

Planning captures exact local input bytes and a build-executable digest.
Applying validates that captured plan and runs the recipe in a fresh directory
with only its declared environment. Builds retain their stdout, stderr, plan,
and phase failure record. Output validation checks WASM structure and declared
container kind. Artifacts and their knowledge are published together, followed
by atomic local selection under a store mutation lock. Stale selection conflicts
fail before a build. Repeating the selected plan verifies installed content and
returns without rebuilding. Previous objects and attempt records remain.
The legacy assembler rejects extension manifests because it cannot assemble
the corresponding knowledge contract.

Knowledge is embedded into the CLI library used by the Brood executables.
This includes authored documentation, WIT, SDK source, configuration source,
a guest example, manifest definitions, dependency locks, build provenance,
and a command reference generated from the actual Clap parser. Targets,
implementation notes, friction, reference material, and contract source retain
separate classifications. A bounded search index and exact document lookup use
the same query implementation locally and through the endpoint control command.
Connected responses identify the serving process. Remote queries cannot export
files on that server. The bundle explicitly reports that the full host source
and build dependencies are not embedded.

## Verification

Nix-backed verification performed during implementation:

- Package manifest suite: seven existing checks and four new installation
  workload checks passed. The new checks build two valid empty WASM components
  in an external temporary package, remove source before applying a plan,
  repeat application, exercise build failure, conflict and corruption, and
  reject invalid output. They do **not** execute a paired guest workload.
- CLI suite: six existing tests and two knowledge tests passed, including
  paging, offline example retrieval, endpoint identification, and rejection
  of server-side export.
- Brood executable build passed.
- A Bubblewrap workload with the checkout absent and networking disabled
  discovered contracts, searched documents, retrieved the guest example,
  and exported the full bundle from the built executable. The repeatable
  proof is `tools/test-knowledge-offline.py`, dispatched by the test command
  parser in `crates/brood-cli/src/cli.rs`.

## Still required for the target

The local default package store is now observed on a background worker by the
session backend and presentation client. Verified declarations go to their
explicit realm init roots through `brood-package-contract`; the opt-in SDK
policy owns starts, stops, and replacement ordering. Exact selected references
resolve through host mechanisms without becoming new realm roots. All three
bundled roots consume the policy, including daemon child execution. Default-store selection now activates packages in existing and
future local session/presentation realms. An explicit non-default CLI store
remains an offline selection unless the destination is configured for it.

Session inspection exposes host-owned actor identity, generation, owner,
artifact/knowledge identity, lifecycle outcomes, and resource counts through
the normal control path and a paginated package CLI command. Presentation
records, including failed startup, now travel through the reserved host service
lane to connected inspection. Peer facts identify their endpoint-reported
authority, exact artifact generation, and connection status. Missing host
generations are explicitly unavailable.

Unified destination planning/outcome queries, richer diagnostics, full dependency-aware
replacement/deactivation policy remain. Immutable
installation records retain their pending declarations; only live endpoint
observations prove activation. Readiness is separate from running: a required service stays pending until
registered versions and installed schema digests match and the hosting endpoints
acknowledge the binding. Multiplicity ambiguity remains pending. Schema-validated
messaging rejects stale generations and disconnected bindings.

The resolver currently supports local snapshotted inputs and an explicitly
available Nix-store executable. Cross-package dependency resolution, compatible
prebuilt acquisition, remote source acquisition, richer instance selection,
and build timeout/resource policy remain. Configuration/interface schemas are compiled as JSON Schema and bundled;
invalid schemas or unavailable external references prevent selection. Service
payloads are checked against the installed schema on both hosts. Structural
schema generation remains. Required bundle entries are checked, but general document
reference validation remains. Source provenance can be unavailable in a
filtered source build and is not a claim of full reproducibility.

The real external paired-component workload now passes through the built-in
package test. It snapshots an external Rust package, deletes that source before
application, builds actual Brood components, verifies activation in an existing
session and a later session, repeats application without a new generation, and
verifies existing and subsequently created presentation realms under a headless
Sway compositor. The client is closed through the public frontend-close control
operation so host-owned records are captured before ordered shutdown. Evidence:
`/tmp/brood-package-activation-proof/packages-6outo63p/report.json` and adjacent
session/presentation observations. The extended proof at
`/tmp/brood-package-services-proof/packages-icqfcuim/report.json` additionally
verifies an acknowledged typed ping/pong exchange, an incompatible replacement
remaining pending, disconnected endpoint status, and a failed replacement
observed through the public control API. The failed guest does not supply its
standard lifecycle facts. Build failures and schema rejection are also covered
by the installation workload suite. The complete target remains outstanding.

The package policy's replacement barrier and failed-instance behavior have two
focused tests. Manifest/install tests and Clippy for CLI, SDK, daemon, and client
pass. SDK consumer lockfiles include the new shared contract. Nix source layers
now preserve the knowledge inputs needed by the CLI build script; the full
packaged Nix derivation still needs a build verification.

## Typed services implementation evidence

The portable service protocol lives beside the activation contract. Guests
register the versions they actually implement through the existing host-message
command; the host attaches artifact identity and actor generation. The common
host broker routes bounded observations and a binding offer/accept/commit
exchange over the existing actor-message lane. Session/presentation bindings
follow the active session association. Local bindings remain within one realm
and scope. Broker tests cover version mismatch, scope isolation, acknowledgement
before messaging, schema rejection, stale generations, and disconnection.

The first extended workload attempt failed in the fixture build because its
host-message command name did not match WIT. The fixture now uses the actual
`send-host-message` command. Subsequent workload runs passed. No host-visible
Brood window was started by these tests.

Catalogue scans now reject a selection revision that changed during loading.
A transient catalogue read failure retains the last verified declarations and
exposes the failure rather than turning an I/O error into package removal.

Remaining service/introspection work includes root-directed disconnect-stop
policy, plugin diagnostics, standard paginated envelopes across all observation
collections, bounded stale-observation expiry, and destination installation
authority. A connected transport observation is not remote installation approval.

## Daemon and selection lifecycle

The configured daemon root now supervises ordinary package children using the
same declarations and host lifecycle mechanisms. Daemon children resolve exact
selected package references, receive explicitly requested capabilities, and
report startup, failure, and retirement to their owning root. Stop acknowledgement
follows the actor retirement task. They do not become new realm roots.

Enable, disable, and remove mutate selection under the installation lock.
Disable preserves selection and suppresses activation declarations; enable
restores them. Removal deselects the package and retains its immutable objects,
knowledge, logs, and mutable state. Reports distinguish the local selection
change from pending endpoint teardown and list potentially affected service
consumers. Exact digest-addressed inspection remains available after replacement
or removal, with historical selection status. Corrupt artifacts do not prevent
disabling or deselecting a package.

The three-realm proof passed at
`/tmp/brood-package-lifecycle-proof/packages-idy2sgia/report.json`. It verifies
daemon activation, disable teardown across daemon/session/presentation actors,
re-enable and typed rebinding, idempotent disable/remove, and retained artifacts.
The workload retrieves historical knowledge and inspection through the public CLI
after removal; the installation suite independently checks the same behavior.
Cross-package install dependency resolution remains pending;
potential service consumers are not claimed to be a complete dependency graph.

## Authenticated package distribution

Bundle export captures verified installed artifacts and their exact knowledge as
one bounded content-addressed artifact. The existing symmetric artifact exchange
recognizes this package kind without promoting it to an init root. Destination
workers verify the bundle file set, digests, executable containers, and schemas
before publication. The backend relay imports through its connected daemon's
store location; presentation import runs outside the UI turn. Both use their
existing authenticated artifact ID allowlists and acknowledge verified selection
separately from later root activation.

Destination-owned authorization records restrict imported activation to the
receiving session or presentation realm. Network imports grant no extra runtime
capabilities. Local application of an inspected plan has local-user authority;
merely enabling an imported selection retains its destination restrictions.
Broader capability grants, daemon remote destinations, and unified destination
plan/outcome queries remain outstanding.

The network workload passed at
`/tmp/brood-package-distribution-proof/packages-c32zwd8r/report.json`. Source,
backend, and presentation have disjoint stores. Both authorized destinations
receive the same exported bundle, start only their own realm, and exchange typed
ping/pong over the network connection. A second connection denies backend
installation while permitting presentation installation; the presentation actor
remains pending without a peer. This is verified partial success rather than
shared-filesystem activation.

The first network attempt exposed a duplex artifact deadlock: each endpoint
awaited an entire outgoing bundle before reading incoming bytes. A bounded
artifact sender now runs independently of receive processing. The subsequent
denial test exposed digest-direction confusion: rejecting an outgoing offer
removed an incoming transfer of identical content. Sender abortion is now a
distinct wire message and offer rejection preserves the opposite receive state.
A focused transport regression test covers that case. Installation tests cover
bundle identity, per-destination realm restrictions, repeated import, and
authorization revision observation.

## State compatibility and root acknowledgement

A conservative package state-format record now survives disable, removal, and
host restart. It records a format selected code may have written, without
reading plugin data. Stateless selections do not erase an existing state record.
A different state-dependent compatibility label blocks new activation. Roots
retire old generations and report the pending reason; host resolution also
rejects an attempt to bypass the blocked declaration.

Supported migration requirements are none or manual. Manual completion is an
operator attestation tied to the exact selected installation and previous
format, with bounded evidence. The operator or an external migrator owns the
data transformation and must quiesce affected instances first. The host does
not claim to verify private data or transform it implicitly. Selecting older
code leaves the newer state record intact and can require another migration.

Configured roots acknowledge the declaration revision and report policy intent
through the existing host-message command. Host wrappers authenticate root
identity/generation and distinguish root-reported policy from observed lifecycle.
Missing and stale acknowledgements remain visible. These reports do not equate
a running child with a ready typed service.

The live proof passed at
`/tmp/brood-package-migration-proof/packages-p2f3qjzb/report.json`. A daemon actor,
two session actors, and a presentation actor all stop for an incompatible format,
resume after explicit completion, and stop again when older code is selected
against newer retained state. The fixture's data change is deliberately external
to the guests and host: this verifies the manual migration contract and guard,
not an automated guest migrator. Public migration inspection confirms the state
record remains newer during rollback. Focused tests also reject stale completion
and verify that removal does not discard the state format. The first workload
attempt hit a fixture plan-filename collision; unique output names fixed it.

### Captured installation dependency graphs

The extension manifest now accepts package dependencies with source-relative
manifest paths. Planning recursively captures their inputs and exact plan
identities, rejects cycles/conflicting identities, and bounds the graph and
aggregate captured bytes. Apply preflights every prior selection, builds in
dependency order, and publishes selections only after every build succeeds.
The catalogue refuses observations during the store mutation lock. Publication
is not yet a crash-atomic multi-file transaction; after a crash, exact dependency
activation gates prevent a mismatched graph from starting, but recovery still
needs explicit treatment.

Installed knowledge records exact dependency revisions. Catalogue activation
follows the dependency graph, including missing revisions, disabled selections,
and state compatibility gates. Disable/remove reports include direct installed
package dependents in addition to potential service consumers. Package
installation dependencies do not implicitly grant service bindings or make
build outputs available as compiler inputs.

Local build receipts distinguish objects built from a captured local plan from
objects imported under destination authorization. Reapplying a locally built
plan reuses verified objects; an imported object alone is insufficient evidence
to expand its destination authority by claiming the same plan identity.

Verification: the Nix-backed standalone installation suite passed all 11 tests,
including source deletion for parent and dependency before apply, repeat build
reuse, disable/re-enable activation gates, and successful dependency build
followed by parent failure without either selection being published. Dependency
bundle acquisition/authorization, crash recovery, and real running dependent
actors remain unverified.

### Root-owned service disconnect policy and observation expiry

Realm updates now include host facts about lost, previously bound required
services. The SDK init policy applies declared `disconnect = "stop"` to those
facts, retires the child through its existing stop/acknowledgement mechanism,
and reports the pending reason. Loss survives consumer teardown. A compatible
reachable provider or a different selected installation clears the old loss;
the root then owns starting the next generation. The `pending` policy continues
to leave the consumer alive with an unbound service. Remote routes expire after
15 seconds without service traffic, removing their peer bindings and marking
retained endpoint observations unreachable.

Verification: 9 CLI and 4 portable-contract unit tests passed, including an
expired provider, retained loss after consumer retirement, provider return, and
root stop/wait/start ordering. Nix-backed Clippy passed for CLI, contract, SDK,
backend, and client across all targets. The full built-in offscreen package
workload passed at
`/tmp/brood-package-disconnect-proof/packages-nh526y4c/report.json`.
Its authenticated separate-store round now disables only the provider package,
observes the remote consumer stopped, re-enables the provider, and observes a
new consumer generation with a ready binding. The first attempt caught a stale
fixture assumption that a consumer must still run after closing its connection;
the proof now checks binding before closure and accepts normal shutdown phases.
Lease expiry itself is tested with a controlled monotonic clock offset, while
real provider loss and recovery are exercised over the network.

### Common operator introspection envelope

The portable contract now defines versioned requests, responses, structured
errors, collection identifiers, and request/response bounds. The discovery
collection publishes envelope schemas. A single operator query path handles
local verified installations or connected backend observations. Installations,
local instances, root policy reports, bindings, peers, endpoints, remote instance
reports, and failures can be requested and paginated independently. Pages are
limited by both item count and 48 KiB encoded response size. Continuations can
supply the observation revision; changed data returns `revision-changed` rather
than silently merging pages. Observation-age counters do not invalidate a
revision, while actual record changes do. Unsupported observations return an
explicit structured error. Existing aggregate queries remain for their current
callers; the new bounded API does not claim those aggregates became paginated.

Verification: 11 CLI unit tests passed, including byte-bounded pages, revision
changes, malformed requests, and unavailable observations. Nix-backed Clippy
passed CLI, portable contract, and backend across all targets. The full built-in
offscreen proof passed at
`/tmp/brood-package-introspection-proof/packages-7ehaw3w8/report.json`, including
connected two-page instance retrieval and an offline installation query using
the same envelope after source removal. The first run failed its external build
because the fixture omitted the newly added portable source module; its captured
input list was corrected, and the rerun passed.

Remaining introspection integration includes bundled descriptions in this same
contract, guest delivery with realm/session authority, record-specific schema
discovery and generated structural schemas, plugin-reported diagnostics, and
common presentation query delivery. The envelope schemas are explicit contract
schemas, not yet generated from Rust types. These checks do not prove those
remaining target requirements.

### Scoped guest introspection

Package guests can submit the common introspection request through the service
mailbox and receive a correlated, bounded response. The host resolves the real
actor generation and supplies its realm and scope; a query cannot name a
broader authority. Filtering occurs before pagination, totals, and revision
hashing. Local instance, peer, root-policy, failure, and installation records
are restricted to that scope. Binding observations include actual bound peers.
Endpoint-wide and remote-instance inventories require operator authority and
return an explicit error to a package guest. This path serves both backend and
presentation package guests without a new WIT world or transport.

Verification: 12 CLI and 5 portable-contract tests passed. They include scope
isolation, no revision leakage from other scopes, installation filtering, and
a complete guest-event wire round trip. Clippy passed CLI, contract, backend,
and client across all targets. The full offscreen workload passed at
`/tmp/brood-package-guest-query-proof/packages-tgfqco6s/report.json`.
The real session and presentation guests now delay service registration until
their scoped instance query succeeds and an endpoint inventory request is
denied, so the subsequent typed binding proves mailbox query completion. This
also runs for later sessions and separate authenticated network destinations.

The first real run exposed serde's inability to deserialize a `u128` timestamp
inside the tagged guest event. A focused regression test reproduced the exact
failure. The response timestamp is now `u64`; the round trip and full workload
pass. Guest query resource maps currently reflect the package host's retained
records; fresh runtime resource sampling through this mailbox still needs
integration. Bundled-description queries, generated record schemas, and
plugin-reported diagnostic handling remain separate unfinished requirements.

### Runtime resource observations in guest queries

Backend and presentation hosts now sample runtime I/O counters immediately when
a package guest queries introspection, and refresh them during periodic service
publication. Records include the resource observation timestamp independently
of the query timestamp. Updates require a matching running actor generation;
stopped instances clear the live measurements. Operator queries also timestamp
the runtime samples they collect. Timestamp-only changes do not invalidate a
pagination revision, while changed counts still do.

Verification: CLI unit tests and Nix-backed Clippy for CLI, backend, and client
passed. The full offscreen proof passed at
`/tmp/brood-package-resources-proof/packages-oq3k781q/report.json`.
Real session and presentation guests allocate a timer, query their scoped
instance, and require a timestamp plus the observed timer count before service
registration. The test covers later realms and authenticated remote endpoints,
and checks that disabled instances no longer report live resources. The repeat
apply assertion now compares actor/generation/artifact identities rather than
volatile observation timestamps. These are runtime I/O counters; this work does
not add plugin diagnostics or arbitrary memory inspection.

### Schema-described plugin diagnostics

Optional package-namespaced diagnostic declarations now capture schema files
in the immutable plan and knowledge bundle. Builds, catalogue loading, and
remote bundle import validate those schemas. The service mailbox accepts bounded
value or failure reports only for the real actor generation's selected package
and declared diagnostic ID/version. Records preserve plugin authority, schema
identity, validation result, report time, and an independent diagnostic status.
Declared but silent diagnostics are explicitly not reported. The common API
has an independently paginated diagnostics collection, with the existing guest
scope filtering applied before extraction. Invalid values mark the diagnostic
failed without changing actor health or service readiness. Value, error, and
aggregate diagnostic sizes are bounded.

Verification: the 12-test installation suite passes, including captured schema
knowledge, invalid schema replacement preserving selection, and rejection of
another package's diagnostic namespace. CLI/contract tests and Clippy passed;
standalone manifest Clippy also passed. The full built-in offscreen proof passed
at `/tmp/brood-package-diagnostics-proof/packages-8q999m7i/report.json`.
Real session and presentation guests report a valid diagnostic, read its
accepted value back through the scoped diagnostics query, then submit a
schema-invalid value before registering their service. The operator observes
failed diagnostics on both still-running actors while typed ping/pong succeeds.
The same guest sequence runs after authenticated separate-store distribution.
Diagnostics are explicitly supplied by the plugin; querying the host's retained
report does not run arbitrary guest inspection code or expose private memory.

### Explicit host contract compatibility

Manifests can now name acceptable versions of host interfaces separately from
artifact ABI. The portable contract owns the implemented host interface/version
set and common introspection discovery exposes it. Plans report the querying
build's compatibility result. Catalogue activation checks the destination
build's interfaces and reports a specific unknown-interface or version mismatch.
The check participates in dependency blocking and exact artifact resolution,
so an init root cannot bypass it with a direct package reference. Incompatible
immutable installations remain inspectable while activation is pending.

Verification: all 13 installation tests passed, including inspectable plan
mismatch, refused direct resolution, and acceptable-version alternatives.
Nix-backed Clippy passed CLI, contract, backend, and client. The full offscreen
workload passed at
`/tmp/brood-package-host-contract-proof/packages-64unsuko/report.json`.
A replacement requiring unsupported introspection version 99 retired all four
existing daemon/session/presentation instances and produced explicit pending
root reasons. A subsequent compatible installation resumed activation, and the
remaining migration, diagnostics, distribution, and partial-success checks
passed. Final packaged-build compatibility verification remains outstanding.

### Build deadline and process cleanup

Build recipes now declare a validated timeout, with a schema-default deadline.
Apply launches Unix builds in a separate process group and retires that group
on timeout, failure, or successful completion before output verification. The
RAII guard also handles errors while waiting. Artifact outputs must be regular
files with an aggregate size no greater than 64 MiB before reading and validating
them. Existing graph application semantics preserve all selections on these
build/verification failures and retain attempt logs.

Verification: all 15 Nix-backed installation tests pass. A real Bash workload
starts a busy descendant, reaches the one-second deadline, preserves the prior
selection and timeout failure record, and verifies that the descendant is no
longer running. Another build emits an oversized artifact and never becomes
selected. Standalone manifest Clippy and the root CLI compilation passed.
The process-group policy does not revoke trusted build code's user authority or
contain descendants that intentionally leave that group. Log/spool quotas,
source acquisition, prebuilt alternatives, and final packaged workload checks
remain separate outstanding work.

### Bounded concurrent build log capture

Build stdout and stderr now use concurrently drained pipes with a validated
per-stream log budget. Only the bounded prefix reaches each preserved log.
Overflow or capture failure terminates the declared build and prevents selection
publication. Unix nonblocking readers finish after process-group retirement
even if an escaped helper retains a pipe. The process guard and log workers are
joined on the success, timeout, and failure paths.

Verification: all 16 Nix-backed installation tests passed, including a real
noisy Bash build that retained exactly 1024 stdout bytes and 900 stderr bytes,
recorded the budget failure, and did not publish a selection. Timeout/descendant
cleanup, successful captured builds, graph failures, and output-size tests still
pass. Standalone manifest Clippy passed, and root CLI compilation passed during
integration. This bounds captured logs, not arbitrary files trusted build code
may write to its workspace or elsewhere under the user's authority.

### Recoverable dependency publication and OS-owned locks

Store mutations now use OS-managed file locks; a killed owner cannot strand a
new-format lock. The file remains in place to avoid concurrent locking of
different inodes. Readers hold shared locks during catalogue verification.
Apply writes and syncs a publication journal after all graph builds succeed,
then publishes selections. Recovery verifies every object, local build receipt,
and dependency-first ordering before making further changes. A later mutation
or explicit recovery completes the previously authorized publication. Catalogue
readers reject an unfinished journal, allowing live hosts to retain their last
verified declarations. Legacy directory lock markers remain explicit errors
requiring inspection because they carry no trustworthy owner identity.

Verification: the 18-test installation suite passes, including replay of a
simulated partial two-package publication, refusal of corrupted recovery inputs,
repeat recovery, and a separate process killed while holding the actual store
lock. Standalone manifest and root CLI/backend/client Clippy passed. The full
offscreen workload passed at
`/tmp/brood-package-publication-proof/packages-vntpbj3t/report.json`, including
all realm lifecycle, state, diagnostics, and network distribution checks under
the new locking/publication path. This evidence covers process-lock death and
journal replay; it is not a hardware power-loss fault-injection proof. Forced
termination also cannot execute Rust build-process cleanup guards, so the lock
test alone does not prove cleanup of a build running when its installer is killed.

### Resource expectations and independent service delivery

Activation declarations now carry resource-use expectations with portable IDs
and units. Plans, knowledge, and realm updates retain them. Live records compare
observed usage to the expectation and distinguish above/within expectation,
not-observed, and unsupported-observation states. Expectations support sizing
and diagnosis; runtime grants and enforced limits remain separate facts.

Installation tests verify capture after source removal and exact realm delivery.
The real guests require an observed timer within expectation and explicit
unsupported memory sampling before service registration. During this work, two
runs stalled at later-session binding and remote-provider teardown. Tracing and
code inspection exposed a shared-queue problem: a failed send retried at the
front and ended flushing, blocking unrelated destinations. Both hosts now skip
blocked delivery lanes for the current flush and defer those lanes behind other
work, preserving each lane's FIFO order. A regression test puts a healthy reply
behind 100 blocked messages and verifies independent progress and ordering.
Temporary trace logging was removed.

The first full offscreen run after the delivery fix passed at
`/tmp/brood-package-expectations-fair-proof/packages-pw_zlu0t/report.json`.
The installation suite (19 tests), CLI tests (13), and root Clippy passed during
integration. A second uninstrumented workload also passed at
`/tmp/brood-package-expectations-confirmation/packages-9cgq0_o0/report.json`,
including both previously stalled stages.

## Captured prebuilt alternatives and retirement races

Manifest alternatives now declare a complete artifact set, per-file SHA-256,
identity, and host-interface requirements. Planning selects the first compatible
available local set, captures its bytes and knowledge inputs, and includes the
choice in the plan digest. Missing/incompatible alternatives fall back to the
captured build recipe; corrupt selected bytes fail resolution. Apply uses the
captured set without requiring source inputs or the declared Nix executable,
then runs normal artifact/schema verification and transactional publication.
The installed knowledge retains the selected alternative.

The Nix-backed installer suite passed 22 tests, including prebuilt acquisition
with a nonexistent toolchain, source deletion before apply, fallback, corruption,
selection preservation, and convergence. Standalone Clippy and formatting passed.
A transient existing selection test hit lock contention. A lock guard now
explicitly unlocks before closing, so a descriptor briefly inherited by another
thread's fork does not extend the critical section until exec. A deterministic
shared-open-description regression passed; the complete installer suite passed
with the guard. The observed test failure is consistent with this race, although
that individual failing process was not traced at the syscall level.

The first extended real workload preserved at
`/tmp/brood-package-prebuilt-proof/packages-hhw0raws` passed the existing/later
realm and failure checks, then failed during migration before the new prebuilt
step. The presentation exited with `facet actor generation unavailable`: a
scheduler hint addressed an actor already removed for asynchronous teardown.
Dispatch now drops hints whose exact generation is no longer live, and package
inspection records `stopping` immediately when the root requests retirement.
This preserves the stop-completion acknowledgement barrier rather than treating
removal from the live map as completed teardown.

The corrected full offscreen workload passed:
`/tmp/brood-package-prebuilt-retirement-proof/packages-439ecri5/report.json`.
It verifies the formerly failing migration/quiescence stage, rollback guards,
then captures original compatible artifacts as a new prebuilt installation with
no build inputs and a nonexistent toolchain. After deleting its source directory,
apply starts the daemon, both sessions, and presentation; their typed peer
binding succeeds. Authorized/denied network distribution and disconnect-stop
recovery also pass. `prebuilt-instances.json` records all four exact selected
instances. Root Clippy passed for CLI, backend, presentation client, and remote;
workspace formatting passed. This is development-binary workload evidence,
not the outstanding final packaged Nix verification.

## Executable knowledge through common introspection

The portable introspection contract now exposes independent bundled-description
and documents collections. Operators, offline callers, and scoped guest queries
use the same request/response types. Documents support exact ID and
case-insensitive search, UTF-8-safe bounded chunks, per-document content digests,
ordered parts, and normal revision-checked pagination. Search retains every part
of a matching document. The actual Clap-generated command reference is included
as `brood.commands.v1`. These collections read the answering executable's bundle,
not the store or guest code; record authority and offline endpoint identity make
that distinction explicit. Categories remain authored target/progress/friction/
reference/contract-source or generated-contract.

Nix-backed verification:

- 14 CLI tests passed, including exact reconstruction of the embedded WIT file
  through guest-facing common pages and reconstruction of the generated parser
  reference. UTF-8 chunking and filter misuse are covered.
- The checkout-hidden, network-isolated binary proof passed at
  `/tmp/brood-common-knowledge-proof/knowledge-iy0fqqk5/proof.json`, including
  description, search, export, and exact multi-page WIT reconstruction with an
  unavailable package-store path. An initial search assertion incorrectly assumed
  a document ID could not also match bundled source content; the corrected check
  combines the exact document filter with content search.
- The full real workload passed at
  `/tmp/brood-common-documents-proof/packages-achmd4xb/report.json`. Session and
  presentation guests retrieve the generated command document before service
  registration, so successful typed bindings require actual common document
  delivery on both hosts. Existing/later realms, migration, prebuilt activation,
  and authorized/denied network distribution also pass.
- Clippy passed for CLI, backend, client, and remote; the small access changes
  it requested subsequently passed the CLI tests.

These are development-binary proofs. Installed-package knowledge still needs
common-API document retrieval tied to exact installation identity; generated
record/configuration schemas, reference/association validation, and final
packaged verification remain separate outstanding requirements.

## Exact installed knowledge through common introspection

The portable API now has package-description and package-documents collections,
requiring both package identity and exact installation digest. Responses retain
knowledge and artifact identities. Authored documents and the installed manifest,
interface schemas, diagnostic schemas, and configuration schema use the bounded
search/chunk/pagination path. Manifest validation reserves contract document IDs
to prevent authored entries from colliding with `manifest` or `schema:` entries.

Catalogue workers retain the verified knowledge in shared immutable memory.
Backend operator queries and presentation/backend guest queries read that cache,
not the filesystem on a runtime turn. A guest must have that exact installation
represented in its existing realm/scope. Unsupported or replaced cached revisions
are explicit errors. Offline store queries also support exact historical objects
after replacement/removal. No guest code or original checkout is required.

Nix-backed evidence:

- 15 CLI tests passed, including exact visible installation checks, denial before
  document totals/revision exposure, and mismatched cached knowledge rejection.
- All 22 standalone installation tests passed with catalogue knowledge retention
  and reserved document IDs.
- CLI/backend/client/remote Clippy passed after a conditional-style cleanup.
- The full workload passed at
  `/tmp/brood-installed-knowledge-proof/packages-ypzhr_j3/report.json`. Session
  and presentation actors request their exact installed overview using the
  identity returned by host instance inspection, then retrieve the executable
  command reference, before service registration. Connected description lookup
  checks the same knowledge digest. Historical common document lookup after
  removal checks both digest and exact authored content; evidence is in
  `historical-common-knowledge.json`. All previous lifecycle, migration, prebuilt,
  and authorized/denied network distribution checks also pass.

The goal remains incomplete: generated structural record/configuration schemas,
build-time reference/association validation, remaining distribution/dependency
integration, and final packaged verification are still tracked by the audit.

## Structural schemas generated from runtime contracts

Handwritten introspection request/response field lists have been replaced by
schemas generated from the portable Rust serde types. Generated contracts also
cover live InstanceRecord facts, RealmUpdate activation, PolicyReport, service
requests/events, and peer records. Their stable document IDs are advertised in
discovery and indexed alongside other offline knowledge. Generated entries are
included in the exported bundle's content digest and use normal bounded document
chunking rather than making discovery carry every full schema. Decoder/authority
checks remain semantic validation beyond structural type schemas.

The portable contract exposes generation as an optional `schema` feature; the
host CLI enables it with schemars 1.2.2. Default guest builds do not enable it.
The locked minimal-root WASM build and external offline guest builds still pass.

Nix-backed verification:

- 16 CLI tests passed. Each generated schema compiles with the JSON Schema
  validator; actual request/response and correlated service-event envelopes
  validate. Invalid version, collection, and page-limit examples fail both the
  generated request schema and runtime decoder. Discovery fits the mailbox bound.
- Isolated no-checkout/no-network proof passed at
  `/tmp/brood-generated-schemas-offline/knowledge-ju1x2yse/proof.json`: exported
  request/response, instance, and service-event schemas retain stable IDs and are
  retrievable through common document queries.
- Full real workload passed at
  `/tmp/brood-generated-schemas-live/packages-7csdrzu8/report.json`. Session and
  presentation guests retrieve the generated request-schema document before
  registration, so typed binding depends on its actual delivery. Earlier
  lifecycle, installed/historical knowledge, prebuilt, migration, and network
  authorization checks also pass.
- Root Clippy and workspace formatting passed.

Configuration-type structural schemas, required-reference/schema-association
validation, remaining distribution/dependency integration, and final packaged
verification remain open in the completion audit.

## Configuration schemas from the actual parser types

Backend configuration data types now live in `brood-cli/src/configuration.rs`;
`broodd/src/config.rs` imports those same types and retains loading, resource
ceilings, runner discovery, and semantic validation. Shared init/client parser
types remain in `realm_init.rs`. Schemas `brood.runtime-config.v1` and
`brood.realm-init-config.v1` describe those existing parser boundaries and are
available from every executable's shared knowledge bundle. Generated schemas
also record their owning compiled Rust type with `x-rust-type`.

The added parser/schema test uses a real TOML file containing init selection,
client key policy, session runner, startup module/resource/lifecycle settings,
and a startup application. It verifies defaults and renamed fields, rejects an
invalid path type through both parser and schema, and exercises actual relative
path resolution. An initial test helper used TOML's value FromStr rather than
its document parser; correcting that helper made the test pass. The complete
17-test CLI suite passed, and the configuration test passed again after test
formatting cleanup. Runtime crates compile and pass Clippy.

The isolated binary proof passed at
`/tmp/brood-configuration-schemas-offline/knowledge-lbxtfwtb/proof.json`, including
both configuration schema documents and owning type identities with no checkout
or network. The first live run at
`/tmp/brood-configuration-schemas-live/packages-ztpza8_f` failed an early
pagination assertion. That assertion assumed startup observations could not
change between pages and did not preserve the failing second response; the
specific returned error from that run is therefore not retained. The workload
now follows the already-tested API contract: restart on explicit
`revision-changed`, record restart revisions, and fail any other error. It does
not weaken identity, pagination, or endpoint checks.

The full corrected workload passed at
`/tmp/brood-configuration-schemas-pagination-proof/packages-rnogmccp/report.json`:
configured roots boot through the moved types, and all installed knowledge,
existing/later scopes, typed binding, migration, rollback, prebuilt, removal,
and authorized/denied distribution checks pass. This remains development-binary
evidence; reference/association validation, outstanding dependency/distribution
integration, and final packaged verification remain open.

## Knowledge reference and schema-association validation

The executable bundle now validates internal wiki references outside fenced and
inline code. Its build includes referenced text assets, including five JSON
progress-evidence files and the root context document. The incorrect context
reference in `docs/index.md` now points to `../CONTEXT`. Nix source filtering and
layered workspace construction retain those referenced assets. Unresolved
references, escapes, duplicate IDs, and missing schema source documents fail
validation. One shared registry drives typed schema generation and build-time
source checks; generated schemas carry `x-source-document`, checked when the
knowledge bundle is assembled for use/export.

Package Document declarations now support bounded stable-ID `references` and
`schemas` associations. Unknown declarations fail manifest resolution. Immutable
object verification also checks the knowledge's semantic manifest, document set,
metadata/associations, schema sets, and installation/artifact identities against
the manifest and receipt. Imported bundles cannot omit required knowledge merely
by recomputing their hashes. Common package-document chunks preserve the declared
associations. These declarations cover required internal knowledge relationships;
ordinary external URLs are not treated as bundled internal documents.

Nix-backed evidence:

- 18 CLI tests passed, including unresolved-reference rejection, code-span
  exclusion, schema-source association rejection, and all existing schema/query
  checks. Clippy passed for CLI/backend/client/remote.
- 23 standalone installation tests passed. New coverage preserves selection when
  references/associations are invalid and rejects a coherently rehashed bundle
  whose required documents were removed. Standalone all-target Clippy passed.
- An actual Cargo build rejected a temporary missing-reference probe. The probe
  was removed and the normal build passed; rejection output is preserved at
  `/tmp/brood-knowledge-build-rejection.txt`.
- The no-checkout/no-network proof under
  `/tmp/brood-knowledge-references-offline` passed with the linked text assets and
  generated schema source documents present in export.
- Full workload passed at
  `/tmp/brood-knowledge-references-live/packages-9niu270r/report.json`. External
  documents declare a manifest reference and interface/diagnostic associations;
  session and presentation guests check those exact fields before registration.
  Earlier lifecycle, prebuilt, historical knowledge, migration, and authorized/
  denied distribution checks also pass.

Final packaged verification and the remaining dependency/distribution integration
are still tracked in the completion audit. This does not mark the goal complete.

## Packaged build and provenance verification (2026-09-10)

A full Nix build from an isolated snapshot of tracked and nonignored untracked
source exposed three packaging issues: the knowledge overlay overwrote a
read-only backend configuration file when that backend was already a workspace
member; standalone/guest vendoring lacked optional contract schema metadata;
and relocating the standalone contract dependency inside the manifest workspace
changed Cargo's locked member graph. The overlay now skips an already-present
backend source, vendoring includes the root lock, and the relocated dependency
is explicitly excluded from standalone workspace membership.

The standalone manifest package and the first full default package both built.
The latter passed the no-checkout/no-network knowledge proof with 188 entries
under `/tmp/brood-packaged-offline-proof`. Its first runtime proof failed before
client startup because `current_exe` resolves the backend's separate Nix output.
The test CLI now finds the assembled client's executable on PATH when no sibling
exists; the harness applies the same rule to the remote relay executable.

Packaged knowledge inspection also showed null source revision and modification
metadata in the Git-free sandbox. The flake now passes its immutable source NAR
hash plus available revision/modification status into the build bundle. Missing
Git facts remain explicitly unavailable; a content hash is not presented as a
clean Git checkout. The second packaged build is verifying that provenance path.
The original snapshot and repository status are recorded in
`/tmp/brood-packaged-source-proof.json`; build logs are in
`/tmp/brood-packaged-default-provenance.log`.

The real workload now includes an invalid Cargo feature submitted through the
public plan/apply commands, requiring build-phase failure evidence and unchanged
selection and running generations. Runtime reports record executable paths and
hashes; offline reports also record source metadata and binary hash. This new
case still requires its packaged run.

Current checks pass: 18 CLI unit tests, two offline knowledge integration tests,
and CLI/backend all-target Clippy. That broader Clippy run found test-only panic
and indexing lint violations in the new contract fixtures; their test modules
now explicitly permit assertion/setup failure with documented reasons. Nix and
Rust formatting and `git diff --check` pass. Completion remains tracked in the
audit; no target document was changed during this verification.


The second full Nix package and both packaged proofs passed:

- `/tmp/brood-packaged-provenance-offline-proof/knowledge-rph1pukv/proof.json`
  records 188 entries, isolated network/checkout, and source NAR hash
  `sha256-iTZUxT4RUUldAXkFMnpwKKLz+kZcDgTuoWCmZyshXOM=`. The exported
  metadata exactly matches the derivation's source metadata.
- `/tmp/brood-packaged-provenance-runtime-proof/packages-2v0h80f5/report.json`
  passes the complete current workload, including the new public failed-build
  preservation case, scoped diagnostics/resources, state migration and rollback,
  prebuilt activation, and separate-store authorized/denied distribution.
- Both proofs used backend SHA-256
  `326dd64b7e52cfb3731a162e02e4775fdaf2d369cd7481f45f3da64f85a14437`.
  Runtime evidence records client/relay launch paths and their wrapper hashes.

The package was built from the recorded snapshot. Subsequent test-only lint
annotations, harness additions, formatting and these progress edits are not
claimed to have been embedded in that snapshot. The acceptance harness ran
from the working checkout against the packaged executables. Remaining concrete
work is the running installation-dependency workload and the final review of
remote dependency acquisition and destination planning/outcomes.

## Completion verification (2026-09-10)

The final packaged workload passes at
`/tmp/brood-package-final-dependency-proof/packages-giciygpy/report.json`.
It adds a captured actor-free library dependency, verifies that disabling it
retires all four real dependent instances, and verifies fresh generations after
re-enable. It also transfers the exact library revision in both network
directions through existing independently authorized package offers. Denying
the library while authorizing its parent leaves the parent installed but
explicitly pending, without backend actors or a ready service binding.
Public plan output is checked for declared realm/capability information,
resolved executable identity, previous selection and pending root acknowledgement.
All 23 installation integration tests pass again.

The unchanged target's completion audit is now verified in
[[self-describing-extensions-audit]], with consolidated evidence in
[[self-describing-extensions-final-report.json]]. The report retains the exact
packaged source identity and distinguishes later harness/progress edits. The
remaining notes in earlier milestones describe historical work, not open items.
