#!/usr/bin/env sh
set -eu

export MANIFOLD_DUMMY_RUNNER_ENV=active
export MANIFOLD_DUMMY_PROJECT_NAME=session-runner-custom-project

exec ../../target/debug/manifold-runner-exec
