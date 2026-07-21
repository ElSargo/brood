# Plugin graphics canvas workload

This presentation actor submits one bounded canvas packet containing every base
command. Pointer motion moves the white diamond; left click, scrolling, or a key
press changes the palette or transformed shape and submits a replacement packet.
Text is shaped and rasterized by the client's native text service before the
packet references its immutable layout handle.

Build and componentize it from the repository root using the checked-in Nix
development shell, then pass the resulting component to `manifold-client` as a
presentation actor. The exact client flags are defined by `ClientArgs` in
`crates/manifold-cli/src/cli.rs`.
