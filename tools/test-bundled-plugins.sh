#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MATRIX="$ROOT/tools/bundled-plugin-e2e.tsv"

usage() {
  cat <<'EOF'
Usage: tools/test-bundled-plugins.sh --audit
       tools/test-bundled-plugins.sh --list
       tools/test-bundled-plugins.sh --lane NAME [--out-dir DIR]
       tools/test-bundled-plugins.sh --all [--out-dir DIR]
EOF
}

die() {
  echo "ERR $*" >&2
  exit 2
}

declare -A LANE_COMMANDS=(
  [divider-drags]='BROOD_SESSION_SWITCHER_PACKAGE="$SUITE_PACKAGE" BROOD_PLUGIN_DRAG_OUT="$LANE_OUT" bash tools/test-plugin-divider-drags.sh'
  [coding-agent]='BROOD_CODING_AGENT_OUT="$LANE_OUT" bash tools/test-coding-agent-nested.sh'
  [editor]='BROOD_EDITOR_PACKAGE="$SUITE_PACKAGE" cargo run --quiet -- test editor --out-dir "$LANE_OUT"'
  [keybindings-narrow]='BROOD_SESSION_SWITCHER_PACKAGE="$SUITE_PACKAGE" BROOD_SESSION_SWITCHER_OUT="$LANE_OUT" BROOD_KEYBINDINGS_ONLY=1 BROOD_KEYBINDINGS_TEST_OUTPUT_MODE=360x320 bash tools/test-session-switcher-plugin-nested.sh'
  [runtime-io]='BROOD_TERMINAL_PACKAGE="$SUITE_PACKAGE" BROOD_RUNTIME_PLUGIN_ONLY=1 BROOD_RUNTIME_PLUGIN_OUT="$LANE_OUT" cargo run --quiet -- test nested'
  [resize]='cargo run --quiet -- test resize --out-dir "$LANE_OUT"'
  [session-shell]='BROOD_SESSION_SWITCHER_PACKAGE="$SUITE_PACKAGE" BROOD_SESSION_SWITCHER_OUT="$LANE_OUT" bash tools/test-session-switcher-plugin-nested.sh'
  [startup-empty]='BROOD_EMPTY_STARTUP_ONLY=1 BROOD_SESSION_SWITCHER_PACKAGE="$SUITE_PACKAGE" BROOD_SESSION_SWITCHER_OUT="$LANE_OUT" bash tools/test-session-switcher-plugin-nested.sh'
  [terminal]='BROOD_TERMINAL_PACKAGE="$SUITE_PACKAGE" cargo run --quiet -- test nested --out-dir "$LANE_OUT"'
  [workspace-watch]='BROOD_TERMINAL_PACKAGE="$SUITE_PACKAGE" BROOD_RUNTIME_PLUGIN_ONLY=1 BROOD_RUNTIME_PLUGIN_WATCH_ONLY=1 BROOD_RUNTIME_PLUGIN_OUT="$LANE_OUT" cargo run --quiet -- test nested'
  [worktree]='BROOD_WORKTREE_MANAGER_OUT="$LANE_OUT" bash tools/test-worktree-manager.sh'
)

audit_matrix() {
  [[ -r "$MATRIX" ]] || die "missing coverage matrix: $MATRIX"
  local inventory matrix_plugins missing_shell_sidecars unknown_lanes
  inventory="$({ find "$ROOT/plugins" -mindepth 2 -maxdepth 2 -name Cargo.toml -printf '%h\n'; } | sed 's#.*/##' | sort -u)"
  matrix_plugins="$(awk -F '\t' '!/^#/ && NF {print $1}' "$MATRIX" | sort -u)"
  if [[ "$inventory" != "$matrix_plugins" ]]; then
    diff -u \
      <(printf '%s\n' "$inventory") \
      <(printf '%s\n' "$matrix_plugins") >&2 || true
    die "coverage matrix and bundled plugin inventory differ"
  fi
  unknown_lanes="$(
    awk -F '\t' '!/^#/ && NF {print $2}' "$MATRIX" | sort -u |
      while IFS= read -r lane; do
        [[ -v "LANE_COMMANDS[$lane]" ]] || printf '%s\n' "$lane"
      done
  )"
  [[ -z "$unknown_lanes" ]] || die "matrix contains unknown lanes: ${unknown_lanes//$'\n'/, }"
  missing_shell_sidecars="$(
    rg -l 'brood_session_switcher_plugin\.component\.wasm' "$ROOT/tools" -g '*.sh' |
      while IFS= read -r script; do
        # The build assembles the manifest's complete artifact plan; it does
        # not spell out each output filename in shell code.
        if [[ "$script" == "$ROOT/tools/brood-build.sh" ]]; then
          for sidecar in picker keybindings inbox status_bar; do
            rg -q "^placement = \"brood_${sidecar}_plugin\\.component\\.wasm\"$" \
              "$ROOT/packages/default-client.brood-package.toml" \
              || printf '%s missing=%s\n' "packages/default-client.brood-package.toml" "$sidecar"
          done
          continue
        fi
        for sidecar in picker keybindings inbox status_bar; do
          # Manual assemblers also pass a stem to componentize_plugin,
          # which appends .component.wasm itself.
          rg -q "brood_${sidecar}_plugin\\.component\\.wasm|^[[:space:]]*componentize_plugin[[:space:]]+${sidecar//_/-}[[:space:]]+brood_${sidecar}_plugin[[:space:]]*$" "$script" \
            || printf '%s missing=%s\n' "${script#"$ROOT/"}" "$sidecar"
        done
      done
  )"
  [[ -z "$missing_shell_sidecars" ]] \
    || die "manual default-client assemblies missing shell sidecars: ${missing_shell_sidecars//$'\n'/, }"
  awk -F '\t' '
    BEGIN { failed = 0 }
    /^#/ || !NF { next }
    NF != 4 || $1 == "" || $2 == "" || $3 == "" || $4 == "" {
      printf "ERR malformed coverage row %d\n", NR > "/dev/stderr"
      failed = 1
    }
    END { exit failed }
  ' "$MATRIX"
  echo "bundled_plugin_matrix=verified plugins=$(printf '%s\n' "$inventory" | wc -l) lanes=${#LANE_COMMANDS[@]}"
}

list_matrix() {
  printf '%-28s %-20s %s\n' plugin lane 'external assertion'
  awk -F '\t' '!/^#/ && NF {printf "%-28s %-20s %s\n", $1, $2, $4}' "$MATRIX"
}

run_lane() {
  local lane="$1"
  [[ -v "LANE_COMMANDS[$lane]" ]] || die "unknown lane: $lane"
  LANE_OUT="$OUT/$lane"
  mkdir -p "$LANE_OUT"
  echo "plugin_e2e_lane_start=$lane out=$LANE_OUT"
  (
    cd "$ROOT"
    export LANE_OUT
    eval "${LANE_COMMANDS[$lane]}"
  ) 2>&1 | tee "$LANE_OUT/suite.log"
  echo "plugin_e2e_lane=verified lane=$lane out=$LANE_OUT"
}

MODE=""
SELECTED_LANE=""
OUT="${BROOD_PLUGIN_E2E_OUT:-$(mktemp -d /tmp/brood-plugin-e2e.XXXXXX)}"
SUITE_PACKAGE="${BROOD_PLUGIN_E2E_PACKAGE:-}"
while (($#)); do
  case "$1" in
    --audit|--list|--all)
      [[ -z "$MODE" ]] || die "choose exactly one mode"
      MODE="${1#--}"
      shift
      ;;
    --lane)
      [[ -z "$MODE" ]] || die "choose exactly one mode"
      (($# >= 2)) || die "--lane needs a value"
      MODE="lane"
      SELECTED_LANE="$2"
      shift 2
      ;;
    --out-dir)
      (($# >= 2)) || die "--out-dir needs a value"
      OUT="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done
[[ -n "$MODE" ]] || die "a mode is required"

audit_matrix
case "$MODE" in
  audit) ;;
  list) list_matrix ;;
  lane) run_lane "$SELECTED_LANE" ;;
  all)
    mkdir -p "$OUT"
    if [[ -z "$SUITE_PACKAGE" ]]; then
      nix build path:.#brood -o "$OUT/package-result"
      SUITE_PACKAGE="$(readlink -f "$OUT/package-result")"
    fi
    [[ -x "$SUITE_PACKAGE/bin/brood-client" ]] \
      || die "suite package has no Brood client: $SUITE_PACKAGE"
    while IFS= read -r lane; do
      run_lane "$lane"
    done < <(awk -F '\t' '!/^#/ && NF {print $2}' "$MATRIX" | sort -u)
    echo "bundled_plugin_e2e=verified out=$OUT"
    ;;
esac
