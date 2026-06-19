# Agent Instructions

## Reference Code

When implementing features, check the code in `reference/` for relevant examples before designing a new approach from scratch.
Refer patterns that fit Manifold's own architecture and terminology. Treat the reference repositories as examples, not source to copy wholesale.

## Documentation

Document the target design by default, not the current implementation snapshot.

Only describe the current state when it materially affects usability, operator workflow, or verification.

Do not duplicate CLI argument lists in prose. For exact flags, subcommands, and parser behavior, point readers to the code that defines them.

## WSLg Wayland Screenshots

Native Wayland apps work under WSLg. For screenshots, prefer capturing the exported Windows window with `PrintWindow` from PowerShell. This successfully captured WSLg native Wayland windows such as `Flower (Ubuntu-24.04)` and `simple-egl (Ubuntu-24.04)`.

Do not rely on the usual in-WSL Wayland screenshot tools in this environment:weston-screenshooter grim and plain GDI all fail for various reasons


## Testing

Whenever possible prefer to test the changes with a "real" workload. Open the real app and send real keystrokes, clicks and scrolls and record metrics from the nested client as well as manifoldd. Use screenshots to verify any graphical changes.
