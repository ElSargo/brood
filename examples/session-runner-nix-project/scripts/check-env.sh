#!/usr/bin/env sh
set -eu

test "${MANIFOLD_DUMMY_NIX_ENV:-}" = "active"
test "${MANIFOLD_DUMMY_PROJECT_NAME:-}" = "session-runner-nix-project"
printf 'dummy nix environment active\n'
