# Experimental WSLg Work

This directory preserves the WSLg investigation history, scripts, patches, and notes.

It is not an officially supported Manifold backend. The supported project path is the Linux-native `manifoldd` compositor and presentation workflow documented from the repo root.

Agents should not use this directory unless the task explicitly asks about WSLg, Windows integration, the private WSLg system distro experiment, or old WSLg smoke/capture evidence.

## Contents

- `wslg.md`: the original WSLg implementation workspace notes.
- `decisions/`: ADRs that applied to the WSLg experiment.
- `tools/`: private WSLg build, deploy, capture, and smoke-test scripts.
- `patches/`: patch queue used by the private WSLg/Weston spike.

The scripts are kept so the experiment can be resumed or inspected, but they may lag behind the supported Manifold workflow.

## Screenshot Note

During the WSLg spike, native Wayland windows exported through WSLg were most reliably captured from Windows with `PrintWindow`; common in-WSL Wayland screenshot tools were unreliable in that environment. That capture path remains here as experimental evidence and tooling, not as the default project screenshot workflow.
