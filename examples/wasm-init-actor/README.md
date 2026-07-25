# Actor Init Example

This Component Model actor is the default surface-session root. On its lifecycle
start event it requests a `foot` terminal and starts the initial coding-agent
child in that session. It owns the agent registry and creates further agent
children when presentation actors ask for them. The host supplies the
session-targeted app-launch and child-lifecycle mechanisms; startup selection
and supervision policy remain guest code.

When selected for the client realm as well, the same artifact uses its bound
session ID to send an explicitly addressed message to that session's root. The
root reacts by launching the `client-to-session-actor-proof` workload. Actor
mailbox behavior is exercised by the workspace workload; presentation behavior must
be proved through an external `manifold-client`.
