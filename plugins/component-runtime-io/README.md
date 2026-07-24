# Component runtime I/O proof

This Component plugin exercises the target bounded runtime-I/O surface from a
real worker. It writes `.manifold/runtime-io-transcript.md` with four directly
inspectable sections: transformed output from a host subprocess pipeline, a
prompt/input/reply exchange through a real PTY, a request and response from a
real loopback peer, and the observed ordering of two timer notifications.

The transcript includes each PATH-resolved executable selected by the host.
Stream, child, and timer readiness all use the ordinary notification,
pollable, and `worker-wait-any` path; the plugin has no category-specific wait
function.

The Component and Preview 1 variants record independent transcripts and the
local peer's request/response log, so the guest and peer observations can be
compared. These are server-runtime workloads and do not require a presentation
target.
