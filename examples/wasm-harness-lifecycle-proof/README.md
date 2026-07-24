# WASM harness lifecycle proof plugin

This core-WASM harness starts a real animated EGL client and interactive
terminal through the harness control import, then blocks on the named-signal
import. A lifecycle acceptance test either releases the harness to complete
normally or quits the supervisor while it is blocked to prove cancellation and
owned-child cleanup.

Presentation evidence must come from an external `manifold-client`; daemon
state is only lifecycle and transport evidence. The guest's integer return code
is only the harness protocol outcome and is not behavioral proof by itself.
