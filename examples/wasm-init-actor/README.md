# Actor Init Example

This Component Model actor is the default surface-session root. On its lifecycle
start event it requests a `foot` terminal in that session and completes startup.
The host supplies the session-targeted app-launch mechanism; startup selection
and policy remain guest code.

When selected for the client realm as well, the same artifact uses its bound
session ID to send an explicitly addressed message to that session's root. The
root reacts by launching the `client-to-session-actor-proof` workload. Actor
mailbox behavior is covered by the workspace tests; presentation behavior must
be proved through an external `manifold-client`.
