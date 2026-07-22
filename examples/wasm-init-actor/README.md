# Actor Init Example

This Component Model actor is the default surface-session root. On its lifecycle
start event it requests a `foot` terminal in that session and completes startup.
The host supplies the session-targeted app-launch mechanism; startup selection
and policy remain guest code.

The nested session and client proof is `tools/test-actor-init-nested.sh`.
