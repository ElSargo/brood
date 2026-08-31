# Container command

## 2026-08-31

- Added `brood container` as the Podman-compatible user entry point while
  retaining the packaged `podman` compatibility frontend.
- Container `run` now reports failure to start the injected backend as a
  command failure instead of silently continuing as an ordinary container run.
- Detached and attached runs retain private host-side connection material and
  publish a stderr-only `brood container connect CONTAINER` hint. Podman's
  stdout remains available for container IDs and scripted output.
- `connect` resolves local names through Podman and can use the full saved ID
  directly for remote Podman engines. It launches the normal client with both
  the container backend and local backend attached.
- Compact detach flags are normalized only for the internal create operation,
  preserving the remaining Podman flags.
- Verification passed with the Nix development toolchain: workspace checks for
  `brood-cli`, `broodd`, and `brood-remote`; unit tests for `brood-cli` and
  `brood-remote`; and executable-level delegation of Podman `version`, `ps`,
  and `--help`.
- A full image/backend launch was not run in this worktree because no local OCI
  image or built portable backend bundle was present; materializing the bundle
  required a 1.4 GiB Nix closure. The attempted build was cancelled before a
  container was created.

## Integration onto `54dc7b6`

- Rebased the complete tracked and untracked Terra worktree slice without
  changing its source worktree.
- Restricted compact detach normalization to alphabetic short-option groups so
  unrelated single-dash values containing `d` are preserved. Explicit false
  detach settings are removed from the internal `create` command without
  changing the attached result.
- The Nix development toolchain passed formatting, warnings-denied Clippy, and
  27 focused tests across `brood-cli`, `brood-remote`, and `broodd`.
- Executable-level checks delegated `version`, `ps`, and `--help` through
  `brood container` to the real local Podman binary and preserved a delegated
  failure exit status.
