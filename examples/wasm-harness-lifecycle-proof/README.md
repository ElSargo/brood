# WASM harness lifecycle proof plugin

This core-WASM harness is the controller used by
`tools/test-wasm-harness-lifecycle-nested.sh`. It starts a real animated EGL
client and interactive terminal through the harness control import, waits until
the outer compositor visibly contains both clients beside the nested
presentation, then blocks on the named-signal import. The acceptance script
either releases the harness to complete normally or quits the supervisor while
the harness is blocked to prove cancellation and owned-child cleanup.

The screenshot and daemon transcript are the proof. The guest's integer return
code is only the harness protocol outcome; it is not treated as the behavioral
evidence.
