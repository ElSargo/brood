---
status: target
type: system
authority: product
last-reviewed: 2026-09-09
---

# Self-Describing Extensions

## Outcome

A user, plugin, script, or agent can point Brood at a package manifest and have
Brood resolve its inputs, build or acquire compatible artifacts, install them,
and activate them in the declared realms. The caller does not need to edit host
source, copy build outputs by hand, or discover a separate installation path
for each plugin.

Brood and its plugins carry the information needed to understand and extend
the exact software on hand. A standard, versioned introspection API exposes
both bundled knowledge and observable runtime state. The command-line surface
and plugin-facing API use the same contracts.

This target establishes the workflow and information contract. Concrete schema
syntax, transport bindings, and implementation choices remain to be designed.

## Manifest Contract

Extend the existing package manifest rather than introduce a parallel package
format. A manifest describes:

- Stable package identity, release, immutable source revision or content
  identity, supported host interfaces, dependencies, and configuration schema.
- Build inputs, toolchain/environment, build recipe, outputs, and compatible
  prebuilt alternatives. Source inputs and produced artifact paths are distinct.
- Artifacts and their execution realms: daemon, surface session, or
  presentation client; instance scope and applicable session/client selection.
- Activation conditions, required services, lifecycle expectations, and
  deactivation behavior. Conditions cover existing matching realms and realms
  created after installation, with an explanation when activation is pending.
- Provided and required services, message/interface schemas and versions,
  cross-realm bindings, peer multiplicity, and disconnect behavior.
- Expected capabilities, resource needs, documentation, examples, provenance,
  source information, and optional plugin-specific diagnostic schemas.

Package identity, artifact content identity, installation selection, and live
actor identity are separate. One package may span multiple realms and produce
multiple instances. Installation dependency order is distinct from runtime
readiness and activation ordering.

Loading policy belongs to the realm init actors described in [[plugin-runtime]].
They consume package activation declarations through a common contract and
retain ownership of child actors, restart policy, and topology. Installing an
ordinary package does not bless its entry point as a new realm root. The host
provides validated package resolution, artifact transfer, lifecycle mechanisms,
and observable outcomes. Replacing an init actor is an explicit operation.

Interface declarations describe actual contracts; they do not make arbitrary
messages compatible or transparently move state between realms. Binding peers
must check supported interface versions before becoming ready.

## Installation And Replacement

Manifest submission produces an inspectable plan: resolved inputs, selected
build environment and outputs, destination realms, expected grants, activation
conditions, and conflicts with installed packages. Applying that plan uses the
same resolved inputs. Repeating it converges on the same selected installation
without starting duplicate instances.

Brood preserves build logs, content digests, provenance, and phase-specific
failure information. Resolve and build failures leave the selected installation
unchanged. Builds run in an explicit environment; build authority is distinct
from runtime grants. This follows the trusted-user-code model in
[[../concepts/plugin]], without treating a manifest as an automatic grant of
authority at another endpoint.

Artifacts and their knowledge bundles are verified and installed together.
Distribution reuses [[artifact-distribution]]. Each destination acknowledges
its own installation and activation outcome; a disconnected endpoint cannot
be reported as successfully activated. Multi-realm partial success is explicit.

Replacement follows generation teardown and publication rules in
[[plugin-runtime]]. Previous immutable artifacts remain identifiable, but
rollback of code does not imply rollback of mutable state. Packages declare
state compatibility and migration requirements. A failed replacement may leave
an instance unavailable and must report that fact. Disable and removal explain
dependent instances and retained state.

## Standard Introspection

The API has a common discoverable envelope, stable identifiers, versioned
schemas, bounded/paginated queries, and structured errors. Consumers can query
three related kinds of information independently:

1. **Bundled description:** purpose, interfaces, configuration, examples,
   lifecycle semantics, expected capabilities, source, and build provenance.
   This is readable without starting plugin code or connecting to a daemon.
2. **Resolved installation:** selected manifests, dependencies, digests,
   activation rules, destination realms, build results, and pending work.
3. **Live instances:** realm and scope, actor identity and generation, owning
   root, lifecycle/readiness, effective grants, registered services and actual
   peer bindings, resource counts, diagnostics, and recent failures.

Every live record links to the exact installed artifact and knowledge bundle.
Responses identify the endpoint, observation time, and generation or revision;
unreachable peers and unsupported observations are explicit, not empty lists.
Cross-endpoint queries do not claim a globally atomic snapshot.

The host supplies standard instance facts even if a plugin is broken. Plugins
may add namespaced, schema-described diagnostics through bounded requests;
those are identified as plugin-reported data and may fail independently.
Introspection exposes declared and observable contracts, not unrestricted
memory, credentials, or private session content. Visibility follows the
caller's existing realm and session authority.

## Knowledge Inside The Build

Documentation lives beside the code and interface definitions it explains.
The build assembles a searchable, indexed knowledge bundle and embeds Brood's
bundle in the shipped binary. Each plugin package carries its corresponding
bundle. Human-readable documentation and machine-readable contracts refer to
the same stable symbols and document IDs.

Generate structural reference material from actual interface definitions,
configuration types, and command parsers. Keep explanations, invariants,
extension guidance, and worked examples authored near their owning code.
Do not maintain duplicate prose lists of command arguments.

The bundle records exact build identity, enabled features, interface versions,
source revision and local modifications, source paths, and build instructions.
Required extension documentation and schemas are available offline from the
binary itself. Export and search work without the original checkout, a docs
website, or a functioning runtime. Runtime queries additionally describe the
connected executable and endpoint, which may differ from the querying binary.

Design targets, implemented contracts, progress notes, and unresolved friction
remain explicitly distinguished in bundled content. Bundling a target does not
advertise it as a supported runtime feature. Build validation checks required
entries, references, and schema associations; it cannot prove authored prose
correct, so behavior verification remains necessary.

"All information from the binary" includes the knowledge needed to write and
integrate an extension offline. Exact source provenance alone does not provide
offline source access or a reproducible build. Whether to embed an exportable
first-party source snapshot, and whether to bundle build dependencies, remains
an explicit design decision. An unavailable source or dependency must be
reported rather than silently substituted with the latest upstream version.

## Self-Modification

Agents can discover existing behavior and extension points, obtain the relevant
contracts and examples, construct a manifest, inspect its installation plan,
apply it, and verify the resulting instances through the same public APIs as
other callers. A private agent-only installer is unnecessary.

Plugin replacement and rebuilding/replacing Brood itself are distinct lifecycle
operations. Host replacement needs its own restart, compatibility, and recovery
contract; it is not implied by hot replacement of a plugin. Recovery inspection
must remain available when init actors or user plugins fail.

## Acceptance

Use a real external package with paired session and presentation actors. From
its manifest alone, build and install it, observe its typed peer binding, and
verify activation in both an existing realm and a subsequently created realm.
Repeat submission to prove it does not duplicate instances.

With the checkout absent and network unavailable, use a shipped binary to
discover supported extension contracts, retrieve an example, and export its
bundled documentation. Verify that each running instance resolves to its exact
artifact description. Exercise a failed build, incompatible peer, disconnected
destination, failed replacement, and broken plugin inspection through the same
API, checking useful outcomes rather than only serialized schema shapes.

## Related

- [[../concepts/plugin]]
- [[plugin-runtime]]
- [[artifact-distribution]]
- [[../../reference/plugin-package-manifest]]
- [[../../frictions/package-resolution]]
