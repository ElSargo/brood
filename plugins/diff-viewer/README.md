# Diff Viewer Plugin

The diff viewer is a presentation-local, read-only project diff. It opens as an
ordinary workspace pane and closes that pane when its action is toggled again.
The default workspace policy binds the action to `Ctrl+Alt+D`.

The pane follows Zed's project-diff shape: every changed file appears in one
continuous, scrollable surface and changed lines are aligned in a split view.
File headers and hunk headers remain in the same reading flow instead of
opening one pane per file. The scrollbar has a generous pointer target: its
thumb can be dragged, and pressing elsewhere on the track jumps to that
position before continuing the drag.

High-resolution pointer-scroll deltas are consumed as pixels so touchpads stay
smooth and proportional. Text layouts shared by overlapping frames are reused;
the renderer keeps only the resources referenced by the latest submitted
frame.

Diff collection runs in an actor worker through a read-only workspace grant and
the allowlisted `git` subprocess capability. It compares tracked changes with
`HEAD`, reads untracked files through the worker filesystem API, and reports
repository or process failures in the pane. `R` refreshes an open viewer;
closing and reopening also reloads the project.

The rendered snapshot is bounded to 900 KiB so its worker result stays inside
the actor generation's normal result budget.

The client init actor owns the toggle lifecycle. It starts the child with only
read-only workspace and `subprocess:git` capabilities, and stopping that child
destroys its pane. The diff actor does not claim layout authority or request
special placement.

Build and componentize through the checked-in Nix environment:

```sh
nix develop --command cargo build \
  --manifest-path plugins/diff-viewer/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release
nix develop --command cargo run -p brood-wasm --example componentize -- \
  plugins/diff-viewer/target/wasm32-unknown-unknown/release/brood_diff_viewer_plugin.wasm \
  plugins/diff-viewer/target/wasm32-unknown-unknown/release/brood_diff_viewer_plugin.component.wasm
```
