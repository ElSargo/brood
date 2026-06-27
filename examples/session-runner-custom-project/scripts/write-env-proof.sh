#!/usr/bin/env sh
set -eu

mkdir -p .manifold
{
  printf 'MANIFOLD_DUMMY_RUNNER_ENV=%s\n' "${MANIFOLD_DUMMY_RUNNER_ENV:-}"
  printf 'MANIFOLD_DUMMY_PROJECT_NAME=%s\n' "${MANIFOLD_DUMMY_PROJECT_NAME:-}"
  printf 'MANIFOLD_SESSION_PROJECT_DIR=%s\n' "${MANIFOLD_SESSION_PROJECT_DIR:-}"
  printf 'MANIFOLD_SESSION_CACHE_DIR=%s\n' "${MANIFOLD_SESSION_CACHE_DIR:-}"
} > .manifold/env-proof.txt
