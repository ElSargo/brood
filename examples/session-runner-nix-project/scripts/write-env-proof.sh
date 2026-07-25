#!/usr/bin/env sh
set -eu

mkdir -p .brood
{
  printf 'BROOD_DUMMY_NIX_ENV=%s\n' "${BROOD_DUMMY_NIX_ENV:-}"
  printf 'BROOD_DUMMY_PROJECT_NAME=%s\n' "${BROOD_DUMMY_PROJECT_NAME:-}"
  printf 'BROOD_SESSION_PROJECT_DIR=%s\n' "${BROOD_SESSION_PROJECT_DIR:-}"
  printf 'BROOD_SESSION_CACHE_DIR=%s\n' "${BROOD_SESSION_CACHE_DIR:-}"
} > .brood/env-proof.txt
