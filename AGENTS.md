# Agent Instructions

## Reference Code

When implementing features, check the code in `reference/` for relevant examples before designing a new approach from scratch.

Use these local checkouts as implementation references:

- `reference/waypipe`
- `reference/niri`
- `reference/smithay`
- `reference/helix`
- `reference/zed`

Prefer patterns that fit Manifold's own architecture and terminology. Treat the reference repositories as examples, not source to copy wholesale.

## Dependencies

Use normal Cargo registry resolution for now. Do not commit `vendor/` or repo-local `.cargo/` configuration unless we explicitly decide to vendor dependencies for reproducible/offline builds.

## WSLg Wayland Screenshots

Native Wayland apps work under WSLg. For screenshots, prefer capturing the exported Windows window with `PrintWindow` from PowerShell. This successfully captured WSLg native Wayland windows such as `Flower (Ubuntu-24.04)` and `simple-egl (Ubuntu-24.04)`.

Do not rely on the usual in-WSL Wayland screenshot tools in this environment:weston-screenshooter grim and plain GDI all fail for various reasons


## Testing

Whenever possible prefer to test the changes with a "real" workload. Open the real app and send real keystrokes, clicks and scrolls and record metrics from the nested client as well as manifoldd. Use screenshots to verify any graphical changes.
