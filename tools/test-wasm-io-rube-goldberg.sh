#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROFILE="${MANIFOLD_WASM_IO_RUBE_PROFILE:-quick}"
SEED="${MANIFOLD_WASM_IO_RUBE_SEED:-5566816691025824836}"
OUT_DIR="${MANIFOLD_WASM_IO_RUBE_OUT:-}"
TIMEOUT_SECONDS="${MANIFOLD_WASM_IO_RUBE_TIMEOUT_SECONDS:-}"
EXTRA_ARGS=()

usage() {
  cat << 'EOF'
usage: tools/test-wasm-io-rube-goldberg.sh [OPTIONS] [-- WORKLOAD_OPTIONS...]

  --profile quick|stress|soak  workload profile (default: quick)
  --seed SEED                  reproducible workload seed
  --out-dir DIR                artifact directory
  -h, --help                   show this help

Environment defaults:
  MANIFOLD_WASM_IO_RUBE_PROFILE
  MANIFOLD_WASM_IO_RUBE_SEED
  MANIFOLD_WASM_IO_RUBE_OUT
  MANIFOLD_WASM_IO_RUBE_TIMEOUT_SECONDS
EOF
}

while (($# > 0)); do
  case "$1" in
    --profile)
      [[ $# -ge 2 ]] || {
        echo "ERR --profile requires a value" >&2
        exit 2
      }
      PROFILE="$2"
      shift 2
      ;;
    --seed)
      [[ $# -ge 2 ]] || {
        echo "ERR --seed requires a value" >&2
        exit 2
      }
      SEED="$2"
      shift 2
      ;;
    --out-dir)
      [[ $# -ge 2 ]] || {
        echo "ERR --out-dir requires a directory" >&2
        exit 2
      }
      OUT_DIR="$2"
      shift 2
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    --)
      shift
      EXTRA_ARGS+=("$@")
      break
      ;;
    *)
      EXTRA_ARGS+=("$1")
      shift
      ;;
  esac
done

case "$PROFILE" in
  quick | stress | soak) ;;
  *)
    echo "ERR profile must be quick, stress, or soak" >&2
    exit 2
    ;;
esac

for arg in "${EXTRA_ARGS[@]}"; do
  case "$arg" in
    --profile | --profile=* | --seed | --seed=* | --out-dir | --out-dir=*)
      echo "ERR reserved workload option $arg must use the wrapper's separate option/value form before --" >&2
      exit 2
      ;;
  esac
done

if [[ -z "$TIMEOUT_SECONDS" ]]; then
  case "$PROFILE" in
    quick) TIMEOUT_SECONDS=180 ;;
    stress) TIMEOUT_SECONDS=1800 ;;
    soak) TIMEOUT_SECONDS=3900 ;;
  esac
fi
[[ "$TIMEOUT_SECONDS" =~ ^[1-9][0-9]*$ ]] || {
  echo "ERR MANIFOLD_WASM_IO_RUBE_TIMEOUT_SECONDS must be a positive integer" >&2
  exit 2
}

if [[ -z "$OUT_DIR" ]]; then
  OUT_DIR="$(mktemp -d /tmp/manifold-wasm-io-rube.XXXXXX)"
else
  mkdir -p "$OUT_DIR"
  OUT_DIR="$(realpath "$OUT_DIR")"
fi

report_out_dir() {
  local status="$?"
  echo "wasm_io_rube_out=$OUT_DIR" >&2
  return "$status"
}
trap report_out_dir EXIT

cd "$ROOT"

WASM_MODULES="$(nix build --no-link --print-out-paths .#wasm-examples | tail -n1)"
COMPONENT="$WASM_MODULES/examples/wasm-component-actor/target/wasm32-unknown-unknown/release/manifold_component_actor_example.component.wasm"
PREVIEW1="$WASM_MODULES/examples/wasm-preview1-worker/target/wasm32-wasip1-threads/release/manifold_preview1_worker_example.wasm"

[[ -s "$COMPONENT" ]] || {
  echo "ERR packaged Component guest missing at $COMPONENT" >&2
  exit 1
}
[[ -s "$PREVIEW1" ]] || {
  echo "ERR packaged Preview 1 guest missing at $PREVIEW1" >&2
  exit 1
}

echo "wasm_io_rube_profile=$PROFILE seed=$SEED out_dir=$OUT_DIR"
timeout --kill-after=10s "${TIMEOUT_SECONDS}s" \
  nix develop --command cargo run --quiet --locked -p manifold-wasm \
  --example io_rube_goldberg -- \
  "$COMPONENT" \
  "$PREVIEW1" \
  --profile "$PROFILE" \
  --seed "$SEED" \
  --out-dir "$OUT_DIR" \
  "${EXTRA_ARGS[@]}"

[[ -s "$OUT_DIR/trace.jsonl" ]] || {
  echo "ERR workload did not write trace.jsonl" >&2
  exit 1
}
[[ -s "$OUT_DIR/manifest.json" ]] || {
  echo "ERR workload did not write manifest.json" >&2
  exit 1
}
[[ -s "$OUT_DIR/resource-samples.jsonl" ]] || {
  echo "ERR workload did not write resource-samples.jsonl" >&2
  exit 1
}
[[ -s "$OUT_DIR/summary.json" ]] || {
  echo "ERR workload did not write summary.json" >&2
  exit 1
}
[[ ! -e "$OUT_DIR/failure.json" ]] || {
  echo "ERR successful workload left failure.json" >&2
  exit 1
}
