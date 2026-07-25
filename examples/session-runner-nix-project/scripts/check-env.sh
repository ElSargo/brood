#!/usr/bin/env sh
set -eu

test "${BROOD_DUMMY_NIX_ENV:-}" = "active"
test "${BROOD_DUMMY_PROJECT_NAME:-}" = "session-runner-nix-project"
test -n "${BROOD_SESSION_PROJECT_DIR:-}"
test -n "${BROOD_SESSION_CACHE_DIR:-}"
printf 'dummy nix environment active\n'
