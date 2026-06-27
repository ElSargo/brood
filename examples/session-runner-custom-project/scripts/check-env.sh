#!/usr/bin/env sh
set -eu

test "${MANIFOLD_DUMMY_RUNNER_ENV:-}" = "active"
test "${MANIFOLD_DUMMY_PROJECT_NAME:-}" = "session-runner-custom-project"
test -n "${MANIFOLD_SESSION_PROJECT_DIR:-}"
test -n "${MANIFOLD_SESSION_CACHE_DIR:-}"
printf 'dummy custom runner environment active\n'
