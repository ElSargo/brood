---
status: target
type: system
security-boundary: mixed
runtime-boundary: process-and-network
wire-format: not-frozen
authority: product
last-reviewed: 2026-08-22
---

# UI Concurrency

## Purpose

Brood's UI is distributed across the native presentation loop, local and
remote transports, backend command workers, presentation actors, plugin
workers, and platform-service threads. These components are intentionally
asynchronous. Correctness must therefore come from explicit causal authority,
not from the order in which independent queues happen to run.

This target defines the shared concurrency contract for presentation and
plugin UI systems. Subsystem-specific leases, revisions, and epochs refine
this contract; they do not replace it.

## Core Rule

An asynchronous result is an observation about the operation and scope that
created it. It is not authority to mutate whatever UI state is current when it
arrives.

Every state mutation has one owning reducer. The native presentation loop is
the reducer for native viewport state. Each presentation actor is the reducer
for its guest-owned UI state. Background tasks, transport readers, platform
services, and workers produce immutable inputs to those reducers and do not
publish current state directly.

Before applying an input, the reducer validates all causal identities relevant
to that input. Depending on the boundary, these include:

- presentation-client and connection generation;
- backend and session realm;
- actor generation or resource lease;
- user-intent and operation identity;
- source revision, event sequence, or resize epoch.

A missing identity is not treated as a wildcard when replacement is possible.
If the producer cannot identify the scope or operation to which a result
belongs, the result cannot change scoped UI state.

## State Classes

Every asynchronous channel declares which state class it carries. One channel
must not silently mix classes with different admission rules.

| Class | Meaning | Admission and backpressure rule |
| --- | --- | --- |
| Reliable transition | A lifecycle or edge that must be observed | Preserve FIFO order within one causal stream; reject or apply backpressure at a real bounded limit. |
| Latest state | A replaceable sample such as pointer position | Retain only the newest sample in one scope and reject samples older than the scope watermark. |
| Accumulator | Commutative state such as damage or scroll delta | Merge with a declared associative operation and a bounded value range. |
| Authoritative snapshot | A complete view of durable state | Apply only a newer revision from the current source generation. |
| Transaction result | Commit or failure of a requested mutation | Match the current operation identity and scope; a commit names the durable revision it produced. |

Capacity checks apply in release builds. A full reliable queue has an explicit
failure or retry policy. It never grows an unbounded hidden tail. A full
latest-state queue replaces an older value rather than replaying obsolete
samples later.

## Operations And User Intent

A user action that starts asynchronous work creates a unique intent identity.
Each concrete attempt under that intent has a unique operation identity and an
immutable scope. UI pending state is represented by that structured operation,
not by an unqualified boolean or one global request number.

Completion handling first looks up the operation. An absent, cancelled,
superseded, or differently scoped operation makes the completion a no-op apart
from diagnostics. No status text, selection, modal visibility, optimistic
rollback, or focus state may change before this check.

Cancellation removes authority immediately. Cooperative task cancellation is
an optimization for resource use; validation still rejects a late completion
when cancellation loses a race.

## Snapshots And Mutation Publication

Every replace-all snapshot carries a source generation and monotonic revision.
The reducer stores the last applied stamp per source and rejects duplicates and
regressions. When several producers describe overlapping state, one reducer
defines their precedence explicitly; absence or an empty list is never used as
an implicit freshness signal.

A successful durable mutation returns a commit receipt containing its
operation identity and committed revision. The authoritative publication for
that revision is observable before, with, or after the receipt according to a
documented protocol:

1. If publication arrives first, the reducer applies it and later settles the
   matching operation.
2. If the receipt arrives first, the UI may show committed-pending-publication
   state but does not manufacture an authoritative snapshot.
3. The operation settles only after the reducer has observed at least the
   committed revision, or after an explicit terminal failure.

This rule applies to session operations, backend discovery and deployment,
actor resources, persistence, and layout transactions.

## Modal And Picker Lifecycle

Modal creation is idempotent by caller intent identity. Creation replies echo
that identity. A caller can cancel an intent before a pane or picker identity
exists. The service binds every created modal to the authenticated owner actor
generation and destroys it when the intent is cancelled or the owner
generation ends.

An old create reply cannot reopen, replace, or update a newer modal. A modal is
visible only while its owner generation and creating intent are live.

## Input, Focus, And Cross-Lane Ordering

Splitting input into reliable and latest-state lanes is a flow-control detail,
not permission to lose causal order. One sequencing authority assigns an input
scope epoch and sequence before events enter separate lanes.

Critical transitions that invalidate sampled state advance a watermark. These
include focus changes, pointer leave, session or backend authority changes,
window destruction, and connection replacement. Consumers reject latest-state
samples older than that watermark.

A focus change is one logical transaction. Keyboard reset, old-pane blur,
new-pane focus, managed-window focus, and subsequent input carry the same focus
epoch. Each consumer ignores earlier epochs. The renderer may update
immediately, but input for the new target is not admitted under an old focus
epoch.

## Task And Event-Loop Lifetime

Every asynchronous task belongs to a supervised scope such as a connection,
backend deployment, actor generation, modal intent, clipboard selection, or
file-drop operation. Leaving the scope revokes its authority and requests task
cancellation. Teardown reaping happens outside the native event-loop callback.

The native presentation loop performs only bounded in-memory work. It does not
wait for child processes or worker threads, read or decode files, perform
backend I/O, or sleep for retry. Such work runs under a supervised task and
returns a causally stamped event. Detachment and authority revocation happen
synchronously; resource cleanup completes asynchronously.

## Observability

Logs and traces for asynchronous UI work include the relevant scope,
generation, intent, operation, source revision, and sequence. Each reducer
counts applied, coalesced, retried, cancelled, superseded, and stale-rejected
inputs. Queue depth and event-loop callback duration have high-water metrics.

Stale rejection is expected behavior and is not logged as an undifferentiated
error. Repeated stale results or sustained queue pressure remain observable as
health signals.

## Verification

Concurrency verification has four layers:

1. Pure reducer tests permute snapshots, results, cancellation, and scope
   replacement and prove that invalid permutations converge to the same state.
2. Property and state-machine tests generate overlapping user intents and
   enforce operation, modal, revision, and bounded-queue invariants.
3. Transport and actor harnesses inject delay, reordering, duplication,
   saturation, disconnect, and worker failure at semantic lane boundaries.
4. Nix-backed nested presentation tests exercise real input, rendering, actor,
   backend, and reconnect paths while failpoints force the adverse orderings.

Low-level concurrency model checking is useful for shared-memory primitives,
but it does not replace semantic permutation tests at UI reducers.

The system must demonstrate these invariants:

- a stale result cannot change visible or durable current state;
- a snapshot cannot move a source revision backward;
- every visible modal has a live owner and current creating intent;
- focus and input consumers never accept an older focus epoch;
- all queues remain within their declared release-build limits;
- teardown and platform-service work cannot block the presentation loop;
- a mutation receipt and its authoritative publication converge regardless of
  arrival order.

