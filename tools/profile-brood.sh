#!/usr/bin/env bash
set -euo pipefail

usage() {
  printf 'usage: %s OUT_DIR -- COMMAND [ARG ...]\n' "$0"
}

if (($# < 3)) || [[ "$2" != "--" ]]; then
  usage >&2
  exit 2
fi

out_dir=$1
shift 2
mkdir -p "$out_dir"

export BROOD_PROFILE_DIR="$out_dir"
export BROOD_PROFILE_RUN="${BROOD_PROFILE_RUN:-$(date -u +%Y%m%dT%H%M%SZ)}"

"$@"

# Analyze every process stream after instrumented applications have performed
# their bounded shutdown flush.
nix develop --command cargo run --profile profiling -p brood-profiler -- \
  report "$out_dir" --json "$out_dir/report.json"
nix develop --command cargo run --profile profiling -p brood-profiler -- \
  chrome "$out_dir" --output "$out_dir/trace.json"
