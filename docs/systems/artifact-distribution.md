---
status: target
type: system
security-boundary: process-and-network
runtime-boundary: process-and-network
wire-format: not-frozen
authority: product
last-reviewed: 2026-07-26
code-links:
  - crates/brood-protocol/src/lib.rs
  - crates/brood-transport/src/artifact.rs
  - clients/brood-client/src/remote.rs
  - clients/brood-client/src/actor_runtime.rs
  - crates/brood-remote/src/main.rs
---

# Artifact Distribution

## Purpose

Brood may begin with either endpoint hydrated. An SSH-started backend can begin
empty and receive its session actors from the workstation. A browser client can
begin empty and receive its client actors from the backend. Mixed deployments
may transfer artifacts in both directions over one authenticated presentation
connection.

Artifact source, execution placement, and runtime authority are independent:

- either peer may offer immutable content;
- the manifest names the destination `client` or `session` realm;
- the destination decides whether that stable artifact ID is allowed;
- the destination runtime grants capabilities when it instantiates the
  artifact.

Receiving bytes is never itself a capability grant.

## Exchange

Artifacts use the presentation protocol's `Bulk` lane. An offer declares a
stable ID, destination realm, artifact kind, ABI version, byte length,
capability requirements, and SHA-256 content digest. The destination either
rejects the offer, accepts already verified cached content, or requests chunks.
Chunks are ordered, bounded, and completed explicitly. The receiver checks
length and digest before publishing the cache entry or passing it to a runtime.
The receiver sends `Accepted` only after the destination runtime installs the
artifact; a load or policy failure produces `Rejected`.

The same state machine runs at both endpoints. Direction-specific commands are
operator policy around that state machine, not distinct client-push and
server-push protocols.

The cache is content addressed and immutable. Each digest has a separate
package directory, so the runtime's default read-only package view contains
only that artifact. Cache hits are rehashed before use. In-flight artifact
count, aggregate bytes, manifest fields, chunks, and diagnostics are bounded.

## Authorization And Capabilities

The mutually authenticated transport establishes peer identity. It does not
authorize arbitrary code execution. Each destination has an explicit stable-ID
allowlist and rejects offers for the wrong realm. The initial protocol accepts
only artifacts with no requested capability grants.

After verification, the destination selects the local runtime and policy:

- a client-realm actor is instantiated by the presentation client's actor
  runtime without actor-management or runtime-I/O capabilities;
- a session-realm actor is installed through the backend's normal plugin
  control path;
- a future package manifest may name a destination-owned capability profile,
  but the sender cannot mint authority by including capability strings.

Executable identity is the pair of stable installation ID and verified content
digest. Replacement policy belongs to the destination realm. Actor messages,
session authority, presentation leases, and resource handles remain
generation-scoped and are not serialized into artifact storage.

## Hydration Modes

SSH hydration is a bootstrap workflow built on this mechanism:

1. connect to the target and place or select a compatible Brood bootstrap;
2. start a minimal authenticated backend and relay;
3. connect the workstation client;
4. offer the selected session package and wait for verified installation;
5. run the normal presentation session.

The copied bootstrap is replaceable infrastructure, not the source of plugin
authority. Once connected, content-addressed transfer avoids resending cached
artifacts.

Browser hydration uses the opposite placement: the backend offers approved
client-realm WASM to a blank browser runtime. Native, browser, SSH, container,
and preinstalled deployments therefore share one artifact plane while choosing
different offer and allowlist policy.

## Failure And Compatibility

An unsupported ABI, malformed identity, oversized transfer, digest mismatch,
unauthorized ID, wrong destination realm, or capability request fails that
artifact. It does not fall back to executing unverified bytes. Transport
disconnect abandons in-flight data; only atomically published verified cache
entries survive.

Protocol-version negotiation guards the message shape. Package-level ABI and
host capability compatibility remain explicit manifest concerns. A peer may
offer several compatible variants under distinct identities in the future,
with destination selection occurring before content transfer.

## Verification

Acceptance requires:

- both offer directions over a mutually authenticated network connection;
- exact cache digest and length verification;
- rejection of unauthorized, wrong-realm, oversized, and malformed offers;
- a client-realm artifact loading in the presentation runtime;
- a session-realm artifact appearing as a live backend actor;
- artifact traffic remaining isolated from critical input and frame streams.

Tests should transfer and execute real WASM artifacts. Serialization-only tests
cover bounds and lane routing but are not sufficient behavior proof.

## Related Pages

- [[presentation-transport]]
- [[presentation-client]]
- [[plugin-runtime]]
- [[extension-host]]
