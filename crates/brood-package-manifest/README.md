# Package resolution and offline descriptions

The manifest model in `src/lib.rs` remains the package format. Its optional
extension contract is defined and validated in `src/extension.rs`.
`src/installation.rs` provides the shared planning, application, inspection,
and verified knowledge APIs; the Brood CLI is an adapter to these functions.

Plans contain immutable input bytes, so their application does not require
access to the original checkout. The selected Nix build executable must still
exist locally. Build inputs and artifact output paths are distinct. Build
recipes execute as the local user with an explicitly declared environment;
that build authority does not authorize activation at another endpoint.

The local store preserves attempts and immutable objects separately from the
selected plan for each package. Verification precedes atomic selection.
Repeated application converges without executing the build again. Failed builds
retain their logs and leave the previous selection intact. A local mutation
lock prevents concurrent selection changes; after an abrupt process termination,
its `apply.lock` directory may need operator removal after confirming no writer
remains. This is not a distributed installation transaction.

Declared realm activation is pending until a realm init actor acknowledges it.
The configured default store is observed by daemon, session, and presentation hosts,
whose package-aware init actors start and supervise their declared children. See
`../../docs/progress/self-describing-extensions.md` for acceptance coverage and
remaining runtime work. The installation tests exercise an actual build process
and filesystem publication using empty component artifacts; they do not prove
guest lifecycle behavior.

Selection lifecycle operations retain immutable artifacts, knowledge, build logs,
and mutable state. Their responses request root convergence and do not claim
remote teardown acknowledgement. Historical descriptions can be queried by the
exact installation digest; see the CLI parser for operation arguments.

Verified package bundles reuse the authenticated artifact exchange. The receiving
endpoint's allowlist authorizes the package identity; a separate destination
record restricts activation to its receiving realm. Import verifies executable
bytes and knowledge together and acknowledges selection while root activation
remains observable separately. Network imports currently grant no extra runtime
capabilities. An ordinary bundle never replaces an init root.

State compatibility names a package-owned format. Equal non-stateless labels
promise compatible state; `stateless` claims no package state dependency.
The host retains a conservative format record across disable, removal, and
restart. It does not inspect mutable state. A different state-dependent format
blocks new activation and roots retire the previous generation. Selecting old
code does not change the retained format record.

Migration requirements are `none` or `manual`. A manual migration is performed
by the operator or an external migrator after quiescing affected instances.
The completion operation records an attestation tied to the exact selected
installation and previous format; it requires evidence and cannot be supplied
by a guest. The host does not execute an implicit migration, move state between
realms, or restore state during code rollback. A state mismatch with no declared
migration remains blocked. Operation syntax is defined by the public CLI parser.

Configured roots report the declaration revision they consumed and their
per-actor policy status. These are identified as root-reported intent, separate
from host lifecycle facts and peer readiness. Missing or stale acknowledgements
remain visible rather than implying that a root implements the package policy.
