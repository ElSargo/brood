#!/usr/bin/env sh
set -eu

test "${BROOD_DUMMY_RUNNER_ENV:-}" = "active"
test "${BROOD_DUMMY_PROJECT_NAME:-}" = "session-runner-custom-project"
test -n "${BROOD_SESSION_PROJECT_DIR:-}"
test -n "${BROOD_SESSION_CACHE_DIR:-}"
printf 'dummy custom runner environment active\n'
