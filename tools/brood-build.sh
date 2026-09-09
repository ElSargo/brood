#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: tools/brood-build.sh [--host-only | --wasm-only] [--jobs N]

Build the Brood host programs and every WASM module. Independent Cargo graphs
run in parallel, Cargo rebuilds only stale crates, and Component Model encoding
runs only when its core module changed.

Options:
  --host-only  Build only the host programs.
  --wasm-only  Build only the WASM modules.
  --jobs N     Limit the shared parallel jobserver (default: available CPUs).
  --self-test  Check the build-script Preview 1 memory contract.
  -h, --help   Show this help.
EOF
}

ROOT="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ ! -f "$ROOT/Cargo.toml" || ! -f "$ROOT/tools/brood-build.mk" ]]; then
  echo "ERR brood-build.sh is not inside a Brood checkout" >&2
  exit 2
fi
cd "$ROOT"

componentize_if_stale() {
  local core="$1"
  local output="$2"
  local temporary

  if [[ ! -s "$core" ]]; then
    echo "ERR WASM core module was not built: $core" >&2
    return 2
  fi
  if [[ -s "$output" && ! "$core" -nt "$output" ]]; then
    echo "build=component module=$output status=fresh"
    return
  fi

  temporary="$output.tmp.$$"
  trap 'rm -f "$temporary"' RETURN
  echo "build=component module=$output status=encoding"
  wasm-tools component new "$core" --output "$temporary"
  wasm-tools validate "$temporary"
  wasm-tools component wit "$temporary" >/dev/null
  mv "$temporary" "$output"
  trap - RETURN
}

copy_if_stale() {
  local source="$1"
  local destination="$2"
  if [[ ! -s "$destination" || "$source" -nt "$destination" ]]; then
    install -D "$source" "$destination"
  fi
}

assemble_local_client() {
  local manifest=packages/default-client.brood-package.toml
  local package_dir=plugins/session-switcher/target/wasm32-unknown-unknown/release
  local record
  local _id
  local source
  local placement
  local plan
  local -a manifest_tool

  if command -v brood-package-manifest >/dev/null 2>&1; then
    manifest_tool=(brood-package-manifest)
  else
    manifest_tool=(cargo run --quiet --manifest-path crates/brood-package-manifest/Cargo.toml --)
  fi

  plan="$("${manifest_tool[@]}" plan "$manifest")"
  while IFS=$'\t' read -r record _id source placement; do
    [[ "$record" == artifact ]] || continue
    if [[ "$source" != "$package_dir/$placement" ]]; then
      copy_if_stale "$source" "$package_dir/$placement"
    fi
  done <<<"$plan"

  copy_if_stale "$manifest" "$package_dir/brood-package.toml"
  printf '%s\n' brood_session_switcher_plugin.component.wasm \
    >"$package_dir/brood-package-root"
  echo "build=package id=org.brood.default-client path=$package_dir status=finished"
}

preview1_memory_import_has_generation_ceiling() {
  local memory_import="$1"
  local expected='^[[:space:]]*\(import "env" "memory" \(memory \(;[0-9]+;\) [0-9]+ 8192 shared\)\)$'
  [[ "$memory_import" =~ $expected ]]
}

verify_preview1_memory_ceiling() {
  local module="$1"
  local memory_import

  memory_import="$(
    wasm-tools print "$module" \
      | awk '/^[[:space:]]*\(import "env" "memory" \(memory/ && !found { print; found = 1 }'
  )"
  if ! preview1_memory_import_has_generation_ceiling "$memory_import"; then
    echo "ERR Preview 1 module must import shared env::memory with a 512 MiB maximum: $module" >&2
    echo "ERR observed memory import: ${memory_import:-missing}" >&2
    return 2
  fi
}

build_script_self_test() {
  preview1_memory_import_has_generation_ceiling \
    '  (import "env" "memory" (memory (;0;) 17 8192 shared))'
  if preview1_memory_import_has_generation_ceiling \
    '  (import "env" "memory" (memory (;0;) 17 16384 shared))'; then
    echo "ERR Preview 1 memory contract accepted a 1 GiB maximum" >&2
    return 1
  fi
  echo "brood_build_self_test=passed"
}

build_wasm_one() {
  local crate_dir="$1"
  local wasm_target=wasm32-unknown-unknown
  local release_dir
  local core=
  local component=
  local -a cargo_config_args=()

  case "$crate_dir" in
    examples/wasm-preview1-worker|plugins/workspace-file-log)
      wasm_target=wasm32-wasip1-threads
      ;;
    runtimes/computer-use-js)
      wasm_target=wasm32-wasip1
      ;;
  esac

  if [[ -f "$crate_dir/.cargo/config.toml" ]]; then
    cargo_config_args=(--config "$ROOT/$crate_dir/.cargo/config.toml")
  fi

  echo "build=wasm module=$crate_dir target=$wasm_target status=checking"
  cargo build \
    --quiet \
    "${cargo_config_args[@]}" \
    --manifest-path "$crate_dir/Cargo.toml" \
    --target "$wasm_target" \
    --release \
    --locked

  release_dir="$crate_dir/target/$wasm_target/release"
  case "$crate_dir" in
    examples/wasm-preview1-worker)
      verify_preview1_memory_ceiling "$release_dir/brood_preview1_worker_example.wasm"
      ;;
    plugins/workspace-file-log)
      verify_preview1_memory_ceiling "$release_dir/brood_workspace_file_log_plugin.wasm"
      ;;
  esac
  case "$crate_dir" in
    examples/wasm-component-actor)
      core="$release_dir/brood_component_actor_example.wasm"
      ;;
    examples/wasm-facet-probe)
      core="$release_dir/brood_component_facet_probe.wasm"
      ;;
    examples/wasm-paired-facet-probe)
      core="$release_dir/brood_paired_facet_probe.wasm"
      ;;
    examples/wasm-plugin-graphics)
      core="$release_dir/brood_plugin_graphics_example.wasm"
      ;;
    examples/wasm-init-actor)
      core="$release_dir/brood_wasm_init_actor_example.wasm"
      ;;
    plugins/coding-agent)
      core="$release_dir/brood_coding_agent_plugin.wasm"
      ;;
    plugins/component-runtime-io)
      core="$release_dir/brood_component_runtime_io_plugin.wasm"
      ;;
    plugins/component-workspace-watch)
      core="$release_dir/brood_component_workspace_watch_plugin.wasm"
      ;;
    plugins/diff-viewer)
      core="$release_dir/brood_diff_viewer_plugin.wasm"
      ;;
    plugins/inbox)
      core="$release_dir/brood_inbox_plugin.wasm"
      ;;
    plugins/keybindings)
      core="$release_dir/brood_keybindings_plugin.wasm"
      ;;
    plugins/neovim-client)
      core="$release_dir/brood_neovim_client_plugin.wasm"
      ;;
    plugins/neovim-files)
      core="$release_dir/brood_neovim_files_plugin.wasm"
      ;;
    plugins/picker)
      core="$release_dir/brood_picker_plugin.wasm"
      ;;
    plugins/resize-proof)
      core="$release_dir/brood_resize_proof_plugin.wasm"
      ;;
    plugins/session-switcher)
      core="$release_dir/brood_session_switcher_plugin.wasm"
      ;;
    plugins/status-bar)
      core="$release_dir/brood_status_bar_plugin.wasm"
      ;;
    plugins/terminal-session)
      core="$release_dir/brood_terminal_session_plugin.wasm"
      ;;
    plugins/terminal-ui)
      core="$release_dir/brood_terminal_ui_plugin.wasm"
      ;;
    plugins/worktree-manager)
      core="$release_dir/brood_worktree_manager_plugin.wasm"
      ;;
  esac

  if [[ -n "$core" ]]; then
    component="${core%.wasm}.component.wasm"
    componentize_if_stale "$core" "$component"
  fi

  case "$crate_dir" in
    plugins/coding-agent|plugins/neovim-files|plugins/terminal-session)
      copy_if_stale \
        "$component" \
        "examples/wasm-init-actor/target/wasm32-unknown-unknown/release/$(basename "$component")"
      ;;
  esac
  echo "build=wasm module=$crate_dir target=$wasm_target status=finished"
}

if [[ "${1:-}" == "--build-host" ]]; then
  cargo build --quiet --workspace --bins --locked
  exit
fi
if [[ "${1:-}" == "--build-wasm-one" ]]; then
  if [[ $# != 2 ]]; then
    echo "ERR --build-wasm-one requires a crate directory" >&2
    exit 2
  fi
  build_wasm_one "$2"
  exit
fi
if [[ "${1:-}" == "--assemble-local-client" ]]; then
  assemble_local_client
  exit
fi
if [[ "${1:-}" == "--self-test" ]]; then
  if [[ $# != 1 ]]; then
    echo "ERR --self-test does not accept arguments" >&2
    exit 2
  fi
  build_script_self_test
  exit
fi

mode=all
jobs="${BROOD_BUILD_JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)}"
while (($# > 0)); do
  case "$1" in
    --host-only)
      mode=host
      ;;
    --wasm-only)
      mode=wasm
      ;;
    --jobs)
      if (($# < 2)); then
        echo "ERR --jobs requires a positive integer" >&2
        exit 2
      fi
      jobs="$2"
      shift
      ;;
    -h|--help)
      usage
      exit
      ;;
    *)
      echo "ERR unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

if [[ ! "$jobs" =~ ^[1-9][0-9]*$ ]]; then
  echo "ERR --jobs must be a positive integer, got: $jobs" >&2
  exit 2
fi

echo "build=brood mode=$mode jobs=$jobs status=started"
make \
  --file tools/brood-build.mk \
  --jobs "$jobs" \
  --output-sync=target \
  "BUILD_TOOL=$ROOT/tools/brood-build.sh" \
  "$mode"
echo "build=brood mode=$mode jobs=$jobs status=finished"
