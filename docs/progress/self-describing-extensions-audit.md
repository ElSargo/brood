---
status: verified
type: progress
authority: implementation
last-reviewed: 2026-09-10
---

# Self-describing extensions completion audit

This audits the unchanged target in
[[../targets/systems/self-describing-extensions]]. Implementation evidence is
preserved in [[self-describing-extensions-final-report.json]] and the milestone
history in [[self-describing-extensions]]. The report identifies the packaged
executables, source snapshot, proof directories, and validation results.

| Target requirement | Implementation and verification evidence |
| --- | --- |
| Existing manifest with stable package, source, artifact, installation and actor identities | `brood-package-manifest::extension` extends the existing manifest; `installation` binds captured sources, artifacts and knowledge to immutable selections. The live workload links exact installation/artifact/knowledge identities to each generation, including historical lookup after removal. |
| Build inputs, explicit toolchain/environment, outputs and compatible alternatives | `installation::plan` captures the declared inputs and pins the executable; apply uses the captured bytes after source deletion. Packaged prebuilt activation works with the source and toolchain absent. Installation tests cover unavailable/incompatible alternatives, corruption, stale plans, invalid output/schema, build deadlines, process cleanup and bounded logs. Missing inputs fail explicitly. |
| Dependencies and distinct installation/readiness order | Plans capture exact dependency graphs; all builds complete before recoverable graph selection. Integration tests cover dependency ordering, parent failure and convergence. The packaged workload installs an actor-free library and four dependent actors, then proves library disable retires all four and enable creates fresh generations. Typed service readiness remains separately negotiated. |
| Declared daemon/session/presentation placement, scope, conditions and later realms | Catalogue declarations and configured roots select exact scope IDs or `*`. The packaged workload starts all three realms, adds a second session and a later presentation, checks pending reasons and verifies repeat apply does not duplicate instances. |
| Configured roots own loading, child actors, restart and topology | SDK package dispatch consumes the common activation contract. Backend/client hosts provide resolution and lifecycle mechanisms; roots acknowledge policy and own starts/stops. Runtime evidence covers root identity/revision, pending reasons, generation retirement, replacement and service-loss stop/restart. Ordinary package installation never changes configured root selection. |
| Actual typed service contracts, versions, multiplicity and disconnect behavior | The host broker validates authenticated generations, scope, interface version and payload schema through offer/accept/commit. Tests cover version, scope, multiplicity, routing fairness and lease expiry. The packaged workload exchanges validated ping/pong, rejects an incompatible peer and proves root-directed disconnect-stop/rebind over the real network. |
| Inspectable immutable plan, destinations, expected grants, conflicts and convergent apply | Public plan output includes the resolved manifest's realm/scope/activation/capability declarations, executable/input digests, dependency order and previous selection. The packaged workload asserts this output and repeat convergence. Installation tests reject stale selection and altered plans. Build authority is separate from destination runtime grants. |
| Build/resolve failures, preserved logs and unchanged selections | Packaged public plan/apply rejects an invalid Cargo feature, preserves build-phase evidence and keeps the previous selected installation and actor generations. Installation tests also cover malformed inputs, invalid WASM/schema, bounded logs and failure without partial dependency selection. |
| Verified artifact/knowledge delivery and independent destination outcomes | Existing authenticated Bulk-lane offers transfer package bundles, each verified against destination-owned stable-ID and realm authorization. The packaged workload uses disjoint stores, transfers the exact dependency in both directions, observes actual binding after installation, and checks both denied-root partial success and authorized-root/denied-library pending activation. Installation acceptance does not claim activation; receipts and host/endpoint observations distinguish them. Disconnection is reported as unreachable. |
| Replacement, failure, mutable state compatibility and migration | Immutable historical objects survive replacement/removal. Durable state compatibility gates publication to actors; manual migration requires exact-revision operator attestation after quiescence. Packaged verification covers four-instance teardown, migration/restart, rollback refusal, failed replacement and independent broken-plugin inspection. No automatic data transformation is advertised. |
| Disable/remove explain dependents and retained state | Selection operations retain artifacts/state and report dependent packages/services. The live installation-dependency workload verifies actual teardown/restart; removal and historical inspection pass. Captured graph tests verify transitive gates. |
| Common discoverable, versioned, bounded and scoped introspection | The portable request/response contract serves CLI, connected control and guest inspection. Generated schemas derive from actual types. Unit/schema tests cover bounds, pagination/revision changes, structured errors and scope. Packaged guests retrieve exact package and executable documents, observe resource counts, and receive explicit denial for operator-only cross-endpoint queries. Diagnostics are namespaced/schema-validated and can fail while actors remain running. |
| Shipped offline knowledge, source/build provenance, generated references and validation | The Nix package embeds authored targets/reference/progress/friction as distinct categories, SDK/WIT/examples, parser-derived commands and schemas from actual configuration/protocol types. Build validation rejects missing references and schema associations. The network-isolated proof hides the checkout, discovers/searches/retrieves/exports 188 entries and reconstructs paginated contracts. Exported source NAR metadata matches the Nix derivation; compiler/features/profile and explicit unavailable Git metadata accompany it. Binary hash and source identity are recorded in the proof. |
| Public self-modification workflow and recovery inspection | The external package is planned, built/acquired, applied, queried, replaced and removed through public commands/contracts. Introspection is host-owned, so failed guests do not supply their own standard lifecycle facts. Offline knowledge/historical inspection require neither a functioning plugin nor a daemon. Host executable replacement remains a distinct operation, as required by the target. |
| Real external acceptance, including negative paths | The final packaged report passes source removal, existing/later realms, typed binding, repeated submission, failed build, incompatible peer, disconnected endpoint, failed replacement, rejected diagnostics, exact installed documentation, dependency disable/enable, migration, prebuilt acquisition and independently authorized remote dependency delivery. |

## Implementation choices and verification limits

The target deliberately leaves schema syntax and transports open. Exact scope
selectors, the public plan/apply workflow, and existing per-ID artifact offers
satisfy those contracts. Remote dependencies use that same verified artifact
exchange and require their own destination allowlist entries; a parent manifest
cannot grant authority to its dependencies. The referenced distribution target
explicitly uses session/client destinations and initially accepts no requested
capabilities. A new remote installer, richer selector language, nonzero remote
grants, or remote daemon transport would be additional design work.

Manual migration is an operator-owned data change with an exact-revision host
attestation. Resource expectations distinguish observations from unsupported
metrics. Full host source and build dependencies are not claimed to be embedded;
the target leaves those bundle choices open. Missing source/Git facts are
explicit, and a NAR content identity is not represented as a clean Git checkout.

The packaged proof identifies a concrete production source snapshot. Subsequent
test-only lint annotations, harness improvements and progress notes in the
working checkout are not retroactively claimed to be in that package. The
current harness tested those packaged executables; Rust tests and all-target
Clippy separately checked the working source. No target was rewritten to match
the implementation, and no required target item remains open in this audit.
