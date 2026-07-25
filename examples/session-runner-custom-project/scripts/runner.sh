#!/usr/bin/env sh
set -eu

if [ "$#" -lt 5 ]; then
  echo "usage: runner.sh <project-dir> <working-dir> <session-cache-dir> <runner-cache-root> <inner-binary>" >&2
  exit 64
fi

project_dir=$1
working_dir=$2
session_cache_dir=$3
inner_binary=$5

export BROOD_DUMMY_RUNNER_ENV=active
export BROOD_DUMMY_PROJECT_NAME=session-runner-custom-project
export BROOD_SESSION_PROJECT_DIR="$project_dir"
export BROOD_SESSION_WORKING_DIR="$working_dir"
export BROOD_SESSION_CACHE_DIR="$session_cache_dir"

set +e
"$inner_binary"
status=$?
set -e

exit "$status"
