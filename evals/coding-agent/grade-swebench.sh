#!/usr/bin/env bash
set -euo pipefail

if (( $# < 2 || $# > 3 )); then
  echo "usage: grade-swebench.sh <predictions.jsonl> <run-id> [workers]" >&2
  exit 2
fi

PREDICTIONS="$(realpath "$1")"
RUN_ID="$2"
WORKERS="${3:-1}"
if [[ ! "$RUN_ID" =~ ^[A-Za-z0-9._-]+$ ]]; then
  echo "run ID may contain only letters, digits, dots, underscores, and dashes" >&2
  exit 2
fi

EVAL_CACHE="${BROOD_AGENT_EVAL_CACHE:-$HOME/.cache/brood-agent-evals}"
SWE_BENCH_ROOT="$EVAL_CACHE/SWE-bench"
DATASET_ROWS="$EVAL_CACHE/data/swebench-verified.json"
if [[ ! -f "$DATASET_ROWS" ]]; then
  echo "dataset cache missing; run run-swebench.mjs once first" >&2
  exit 1
fi
if [[ ! -d "$SWE_BENCH_ROOT/.git" ]]; then
  git clone --depth 1 https://github.com/SWE-bench/SWE-bench.git "$SWE_BENCH_ROOT"
fi

uv sync --directory "$SWE_BENCH_ROOT"
mapfile -t EVAL_IMAGES < <(python3 - "$PREDICTIONS" "$DATASET_ROWS" <<'PY'
import json
import sys

prediction_ids = {
    json.loads(line)["instance_id"]
    for line in open(sys.argv[1], encoding="utf-8")
    if line.strip()
}
rows = json.load(open(sys.argv[2], encoding="utf-8"))
images = {row["image"] for row in rows if row["instance_id"] in prediction_ids}
if len(images) != len(prediction_ids):
    raise SystemExit("one or more prediction IDs are absent from the dataset cache")
for image in sorted(images):
    print(image)
PY
)

for image in "${EVAL_IMAGES[@]}"; do
  if ! sudo -n podman image exists "docker.io/$image"; then
    sudo -n podman pull "docker.io/$image"
  fi
done

SWE_LIBSTDCXX="$(nix build --no-link --print-out-paths nixpkgs#stdenv.cc.cc.lib)"
SWE_ZLIB="$(nix build --no-link --print-out-paths nixpkgs#zlib)"
TASK_UID="$(id -u)"
TASK_GID="$(id -g)"
restore_ownership() {
  sudo -n chown -R "$TASK_UID:$TASK_GID" \
    "$SWE_BENCH_ROOT/logs/run_evaluation/$RUN_ID" \
    "$EVAL_CACHE/huggingface" 2>/dev/null || true
  for report in "$SWE_BENCH_ROOT"/*."$RUN_ID".json; do
    [[ -e "$report" ]] && sudo -n chown "$TASK_UID:$TASK_GID" "$report"
  done
}
trap restore_ownership EXIT

cd "$SWE_BENCH_ROOT"
sudo -n env \
  DOCKER_HOST=unix:///var/run/docker.sock \
  HF_HOME="$EVAL_CACHE/huggingface" \
  LD_LIBRARY_PATH="$SWE_LIBSTDCXX/lib:$SWE_ZLIB/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
  "$SWE_BENCH_ROOT/.venv/bin/swebench" eval verified \
  -p "$PREDICTIONS" \
  --run-id "$RUN_ID" \
  -j "$WORKERS"
