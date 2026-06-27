#!/usr/bin/env sh
set -eu

test "${MANIFOLD_DUMMY_NIX_ENV:-}" = "active"
test "${MANIFOLD_DUMMY_PROJECT_NAME:-}" = "session-runner-nix-project"
test -n "${MANIFOLD_SESSION_PROJECT_DIR:-}"
test -n "${MANIFOLD_SESSION_CACHE_DIR:-}"
printf 'dummy nix environment active\n'
