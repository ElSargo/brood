# Release terminal diagnostics

Routine presentation lifecycle, input, layout, frame upload, and shutdown
metric records now use the client tracing filter. Release builds default to
warnings; debug builds also enable `brood_client=info`. `RUST_LOG=brood_client=info`
restores routine records in release builds. Existing explicit `BROOD_TRACE_*`
switches retain their diagnostic output. Errors and command/proof results remain
available. The client formatter preserves unadorned record lines for the behavior
harnesses that consume them.

Server tracing writes to stderr so enabling logs does not contaminate CLI stdout.
Session environment output and test-harness completion records retain their
existing stdout contract.

Verification: Nix-backed Cargo checks passed for `brood-client` and `broodd` in
both debug and release profiles. Cargo formatting and diff whitespace checks
passed. A temporary smoke program compiled the actual client logging initializer
with debug assertions enabled and disabled, verifying default filtering,
`RUST_LOG` overrides, exact diagnostic line formatting, warning retention, and
separate stdout/stderr. No graphical behavior workload was run for this logging
change.
