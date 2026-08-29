#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
. "$ROOT/tools/smoke-lib.sh"

if [[ -z "${IN_NIX_SHELL:-}" ]]; then
  exec nix develop --command bash "$0" "$@"
fi
if [[ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]]; then
  exec dbus-run-session -- "$0" "$@"
fi
for tool in awk base64 cargo cc grim jq magick nix pkg-config python3 rg sway swaymsg; do
  command -v "$tool" > /dev/null 2>&1 || {
    echo "ERR $tool is required for the render-scheduling proof" >&2
    exit 2
  }
done

OUT="${BROOD_RENDER_SCHEDULING_OUT:-$(mktemp -d /tmp/brood-render-scheduling.XXXXXX)}"
# Keep nested runtime paths well below the Linux AF_UNIX SUN_LEN limit after
# broodd appends its per-native-agent socket name.
RUNTIME_DIR="$(mktemp -d /tmp/brs.XXXXXX)"
SERVER_RUNTIME_DIR="$RUNTIME_DIR/server"
WORKSPACE="$OUT/workspace"
REPORT="$OUT/render-scheduling-report.json"
DETERMINISTIC_LOG="$OUT/deterministic.log"
CARGO_TARGET_DIR="$OUT/cargo-target"
export CARGO_TARGET_DIR
UNSATURATED_LOG="$OUT/client-unsaturated-60hz.log"
SATURATED_LOG="$OUT/client-saturated-60hz.log"
DIRECT_LOG="$OUT/client-direct-120hz.log"
SERVER_STATUS="$OUT/server-saturated-status.json"
mkdir -p "$OUT/captures" "$OUT/visual-fixtures" "$CARGO_TARGET_DIR" "$WORKSPACE/.brood"
chmod 700 "$RUNTIME_DIR"
mkdir -p "$SERVER_RUNTIME_DIR"
chmod 700 "$SERVER_RUNTIME_DIR"

CLIENT_PID=""
SERVER_PID=""
COMPOSITOR_PID=""
BULK_PID=""
IO_PID=""
WAYLAND_SOCKET=""
SWAY_SOCKET=""
DBUS_DESTINATION=""
ACTIVE_SERVER_LOG=""
ACTIVE_CLIENT_LOG=""
ACTIVE_COMPOSITOR_LOG=""

cleanup() {
  local result=$?
  set +e
  terminate_process_bounded "$BULK_PID"
  terminate_process_bounded "$IO_PID"
  terminate_process_bounded "$CLIENT_PID"
  terminate_process_bounded "$SERVER_PID"
  terminate_process_bounded "$COMPOSITOR_PID"
  rm -rf -- "$RUNTIME_DIR"
  if ((result != 0)); then
    echo "render_scheduling_nested=failed" >&2
    echo "render_scheduling_artifacts=$OUT" >&2
    [[ -f "$ACTIVE_CLIENT_LOG" ]] && tail -n 160 "$ACTIVE_CLIENT_LOG" >&2
    [[ -f "$ACTIVE_SERVER_LOG" ]] && tail -n 120 "$ACTIVE_SERVER_LOG" >&2
    [[ -f "$ACTIVE_COMPOSITOR_LOG" ]] && tail -n 80 "$ACTIVE_COMPOSITOR_LOG" >&2
  fi
  return "$result"
}
trap 'echo "ERR command=$BASH_COMMAND line=$LINENO" >&2' ERR
trap cleanup EXIT

tools/analyze-render-scheduling.py --self-test

if [[ -n "${BROOD_RENDER_SCHEDULING_PACKAGE:-}" ]]; then
  PACKAGE="$(readlink -f "$BROOD_RENDER_SCHEDULING_PACKAGE")"
else
  PACKAGE="$(nix build path:.#brood --no-link --print-out-paths)"
fi
CLIENT_BIN="$PACKAGE/bin/brood-client"
BROOD_BIN="$PACKAGE/bin/brood"

find_component() {
  local filename="$1"
  local path
  path="$(find -L "$PACKAGE" -type f -name "$filename" -print -quit)"
  [[ -n "$path" && -s "$path" ]] || {
    echo "ERR packaged component is missing: $filename" >&2
    exit 2
  }
  readlink -f "$path"
}

GRAPHICS_COMPONENT="$(find_component brood_plugin_graphics_example.component.wasm)"
CPU_COMPONENT="$(find_component brood_component_actor_example.component.wasm)"
WATCH_COMPONENT="$(find_component brood_component_workspace_watch_plugin.component.wasm)"
BACKEND_INIT="$(find_component brood_worktree_manager_plugin.component.wasm)"
SESSION_INIT="$(find_component brood_wasm_init_actor_example.component.wasm)"
CLIENT_INIT="$(find_component brood_session_switcher_plugin.component.wasm)"

read -r -a WAYLAND_BUILD_FLAGS <<< "$(pkg-config --cflags --libs wayland-client xkbcommon)"
cc -std=c11 -Wall -Wextra -Werror \
  tools/virtual-keyboard-key.c -o "$OUT/virtual-keyboard-key" \
  "${WAYLAND_BUILD_FLAGS[@]}"
cc -std=c11 -Wall -Wextra -Werror \
  tools/virtual-pointer-input.c -o "$OUT/virtual-pointer-input" \
  "${WAYLAND_BUILD_FLAGS[@]}"

# This focused ignored test prints the virtual-clock observations themselves;
# a generic test exit is deliberately not translated into zero counters.
nix develop --command cargo test -p brood-client --release \
  render_scheduler::tests -- --nocapture --test-threads=1 > "$DETERMINISTIC_LOG" 2>&1
nix develop --command cargo test -p brood-client --release \
  plugin_graphics::tests -- --nocapture --test-threads=1 >> "$DETERMINISTIC_LOG" 2>&1
nix develop --command cargo test -p brood-client --release \
  actor_runtime::tests -- --nocapture --test-threads=1 >> "$DETERMINISTIC_LOG" 2>&1
[[ "$(rg -c 'render scheduling deterministic [{]' "$DETERMINISTIC_LOG" || true)" -eq 4 ]] || {
  echo "ERR deterministic scheduler test did not emit machine observations" >&2
  tail -n 120 "$DETERMINISTIC_LOG" >&2
  exit 1
}

write_config() {
  local config="$1"
  local saturated="$2"
  {
    printf '[init]\nbackend = "%s"\nsession = "%s"\nclient = "%s"\n' \
      "$BACKEND_INIT" "$SESSION_INIT" "$CLIENT_INIT"
    printf '\n[startup.terminal]\nenabled = true\nargv = ["foot"]\n'
    printf '\n[session.runner]\nenabled = false\nkind = "nix-develop"\n'
    if [[ "$saturated" == 1 ]]; then
      for actor in render-cpu-a render-cpu-b render-cpu-c render-bulk; do
        if [[ "$actor" == render-bulk ]]; then
          printf '\n[module.%s]\nenabled = true\nkind = "plugin"\npath = "%s"\n' \
            "$actor" "$CPU_COMPONENT"
        else
          printf '\n[module.%s]\nenabled = true\nkind = "plugin"\npath = "%s"\ncapabilities = ["filesystem:workspace-write"]\n' \
            "$actor" "$CPU_COMPONENT"
        fi
      done
      printf '\n[module.render-io]\nenabled = true\nkind = "plugin"\npath = "%s"\ncapabilities = ["filesystem:workspace-write"]\n' \
        "$WATCH_COMPONENT"
    fi
  } > "$config"
}

ctl() {
  env -u brood_session "$BROOD_BIN" ctl \
    --dbus-destination "$DBUS_DESTINATION" \
    --dbus-address "$DBUS_SESSION_BUS_ADDRESS" "$@"
}

start_server() {
  local lane="$1"
  local saturated="$2"
  local -a init_args=()
  if [[ "$saturated" == 1 ]]; then
    init_args+=(--no-init)
  fi
  local config="$OUT/$lane-brood.toml"
  local server_runtime="$SERVER_RUNTIME_DIR/$lane"
  ACTIVE_SERVER_LOG="$OUT/server-$lane.log"
  DBUS_DESTINATION="org.brood.RenderScheduling.${lane//-/_}.p$$"
  write_config "$config" "$saturated"
  mkdir -p "$server_runtime"
  chmod 700 "$server_runtime"
  (
    cd "$WORKSPACE"
    env -u brood_session XDG_RUNTIME_DIR="$server_runtime" \
      RUST_LOG="${RUST_LOG:-broodd=info,brood_wasm=info}" \
      "$BROOD_BIN" run --no-session-runner "${init_args[@]}" --headless \
      --config "$config" \
      --dbus-destination "$DBUS_DESTINATION" \
      --dbus-address "$DBUS_SESSION_BUS_ADDRESS"
  ) > "$ACTIVE_SERVER_LOG" 2>&1 &
  SERVER_PID=$!
  for _ in $(seq 1 300); do
    ctl status > /dev/null 2>&1 && return
    kill -0 "$SERVER_PID" 2> /dev/null || break
    sleep 0.05
  done
  echo "ERR $lane server did not become ready" >&2
  return 1
}

start_load() {
  local actor
  local accepted
  : > "$OUT/saturated-load-provenance.log"
  for actor in render-cpu-a render-cpu-b render-cpu-c; do
    accepted="$(ctl actors send "$actor" spawn-spin-worker)"
    printf '%s\n' "$accepted" >> "$OUT/saturated-load-provenance.log"
    rg -q "^actor id=$actor queued=true$" <<< "$accepted" || {
      echo "ERR saturated CPU spin worker spawn was not accepted actor=$actor" >&2
      return 1
    }
  done
  (
    local sequence=0
    local actor
    while kill -0 "$SERVER_PID" 2> /dev/null; do
      for actor in render-cpu-a render-cpu-b render-cpu-c render-bulk; do
        ctl actors send "$actor" "bulk-$sequence-$(printf '%0800d' 0)" > /dev/null 2>&1 || exit
      done
      sequence=$((sequence + 1))
    done
  ) &
  BULK_PID=$!
  (
    local sequence=0
    while kill -0 "$SERVER_PID" 2> /dev/null; do
      printf 'render scheduling io %s\n' "$sequence" > "$WORKSPACE/io-load.tmp"
      sequence=$((sequence + 1))
      sleep 0.01
    done
  ) &
  IO_PID=$!
}

wait_for_load_actors() {
  local actors=""
  for _ in $(seq 1 600); do
    actors="$(ctl actors list 2> /dev/null || true)"
    if rg -q 'id=render-cpu-a ' <<< "$actors" \
      && rg -q 'id=render-cpu-b ' <<< "$actors" \
      && rg -q 'id=render-cpu-c ' <<< "$actors" \
      && rg -q 'id=render-bulk ' <<< "$actors"; then
      return
    fi
    kill -0 "$SERVER_PID" 2> /dev/null || break
    sleep 0.025
  done
  echo "ERR saturated backend actors did not become ready" >&2
  printf '%s\n' "$actors" >&2
  return 1
}

wait_for_load_progress() {
  local status=""
  for _ in $(seq 1 600); do
    status="$(ctl status-json 2> /dev/null || true)"
    if [[ -s "$WORKSPACE/spin-entered" ]] \
      && jq -e '
        (.backend_actor_progress_by_actor["render-cpu-a"] // 0) > 1 and
        (.backend_actor_progress_by_actor["render-cpu-b"] // 0) > 1 and
        (.backend_actor_progress_by_actor["render-cpu-c"] // 0) > 1
      ' <<< "$status" > /dev/null 2>&1; then
      if rg -q 'component actor worker (failed|finished|cancelled) actor=render-cpu-(a|b|c) worker=900' \
        "$ACTIVE_SERVER_LOG"; then
        echo "ERR saturated CPU spin worker stopped after entering the load phase" >&2
        return 1
      fi
      printf '%s\n' "$status" >> "$OUT/saturated-load-provenance.log"
      return
    fi
    kill -0 "$SERVER_PID" 2> /dev/null || break
    sleep 0.025
  done
  echo "ERR saturated CPU actors did not prove spin entry and per-actor progress" >&2
  printf '%s\n' "$status" >&2
  return 1
}

verify_load_workers_live() {
  local actor
  local status
  status="$(ctl status-json)"
  for actor in render-cpu-a render-cpu-b render-cpu-c; do
    jq -e --arg actor "$actor" \
      '(.backend_actor_progress_by_actor[$actor] // 0) > 1' \
      <<< "$status" > /dev/null || {
      echo "ERR saturated CPU actor stopped progressing actor=$actor" >&2
      return 1
    }
    if rg -q "component actor worker (failed|finished|cancelled) actor=$actor worker=900" \
      "$ACTIVE_SERVER_LOG"; then
      echo "ERR saturated CPU spin worker was not live before measurement actor=$actor" >&2
      return 1
    fi
  done
  printf '%s\n' "$status" >> "$OUT/saturated-load-provenance.log"
}

stop_server() {
  terminate_process_bounded "$BULK_PID"
  terminate_process_bounded "$IO_PID"
  BULK_PID=""
  IO_PID=""
  terminate_process_bounded "$SERVER_PID"
  SERVER_PID=""
}

start_compositor() {
  local lane="$1"
  local refresh="$2"
  ACTIVE_COMPOSITOR_LOG="$OUT/compositor-$lane.log"
  WAYLAND_SOCKET=""
  SWAY_SOCKET=""
  local sway_config="$OUT/sway-$lane.conf"
  printf '%s\n' \
    'xwayland disable' \
    "output * mode --custom 1280x720@${refresh}Hz" \
    'seat seat0 fallback true' \
    'default_border none' \
    'default_floating_border none' > "$sway_config"
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" WLR_BACKENDS=headless \
    WLR_HEADLESS_OUTPUTS=1 WLR_LIBINPUT_NO_DEVICES=1 \
    sway --unsupported-gpu --config "$sway_config" \
    > "$ACTIVE_COMPOSITOR_LOG" 2>&1 &
  COMPOSITOR_PID=$!
  for _ in $(seq 1 300); do
    WAYLAND_SOCKET="$(find "$RUNTIME_DIR" -maxdepth 1 -type s -name 'wayland-*' -printf '%f\n' | head -1)"
    SWAY_SOCKET="$(find "$RUNTIME_DIR" -maxdepth 1 -type s -name 'sway-ipc.*.sock' -print | head -1)"
    [[ -n "$WAYLAND_SOCKET" && -n "$SWAY_SOCKET" ]] && return
    kill -0 "$COMPOSITOR_PID" 2> /dev/null || break
    sleep 0.05
  done
  echo "ERR $lane compositor did not become ready" >&2
  return 1
}

wayland_env() {
  env XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    SWAYSOCK="$SWAY_SOCKET" "$@"
}

capture_screen() {
  local capture="$1"
  local profile="$2"
  if [[ -z "$CLIENT_PID" ]] || ! kill -0 "$CLIENT_PID" 2> /dev/null; then
    echo "ERR client exited before capture: $capture" >&2
    return 1
  fi
  wayland_env grim "$capture"
  if ! kill -0 "$CLIENT_PID" 2> /dev/null; then
    echo "ERR client exited during capture: $capture" >&2
    return 1
  fi
  assert_capture_profile "$capture" "$profile"
}

capture_half_evidence() {
  local capture="$1"
  local x="$2"
  local coverage
  local colors
  coverage="$(
    magick "$capture" -crop "640x720+${x}+0" +repage -alpha off \
      -colorspace gray -threshold '8%' -format '%[fx:mean]' info:
  )"
  colors="$(
    magick "$capture" -crop "640x720+${x}+0" +repage \
      -depth 4 -format '%k' info:
  )"
  printf '%s %s\n' "$coverage" "$colors"
}

assert_capture_profile() {
  local capture="$1"
  local profile="$2"
  local dimensions
  local left_coverage
  local left_colors
  local right_coverage
  local right_colors
  dimensions="$(magick identify -format '%wx%h' "$capture")"
  [[ "$dimensions" == 1280x720 ]] || {
    echo "ERR capture dimensions changed: $capture dimensions=$dimensions" >&2
    return 1
  }
  read -r left_coverage left_colors < <(capture_half_evidence "$capture" 0)
  read -r right_coverage right_colors < <(capture_half_evidence "$capture" 640)
  case "$profile" in
    bilateral-full)
      awk -v lc="$left_coverage" -v rc="$right_coverage" \
        -v lk="$left_colors" -v rk="$right_colors" \
        'BEGIN { exit !(lc >= 0.10 && rc >= 0.10 && lk >= 32 && rk >= 32) }' || {
        echo "ERR capture lost bilateral full-scene content: $capture left_coverage=$left_coverage left_colors=$left_colors right_coverage=$right_coverage right_colors=$right_colors" >&2
        return 1
      }
      ;;
    left-full-right-hidden)
      awk -v lc="$left_coverage" -v rc="$right_coverage" \
        -v lk="$left_colors" -v rk="$right_colors" \
        'BEGIN { exit !(lc >= 0.10 && lk >= 32 && rc > 0.0001 && rc <= 0.02 && rk >= 2 && rk <= 16) }' || {
        echo "ERR hidden-pane capture content changed: $capture left_coverage=$left_coverage left_colors=$left_colors right_coverage=$right_coverage right_colors=$right_colors" >&2
        return 1
      }
      ;;
    *)
      echo "ERR unknown capture profile: $profile" >&2
      return 1
      ;;
  esac
  echo "render_scheduling_visual_capture=verified path=$capture profile=$profile left_coverage=$left_coverage left_colors=$left_colors right_coverage=$right_coverage right_colors=$right_colors"
}

visual_capture_gate_self_test() {
  local fixture="$OUT/visual-fixtures/final17-purple-square-only.png"
  base64 --decode \
    "$ROOT/tools/fixtures/render-scheduling-final17-purple-square-only.png.base64" \
    > "$fixture"
  if assert_capture_profile "$fixture" bilateral-full > /dev/null 2>&1; then
    echo "ERR visual capture gate accepted final17 purple-square-only fixture" >&2
    return 1
  fi
  echo "render_scheduling_visual_capture_self_test=passed"
}

visual_capture_gate_self_test

stop_compositor() {
  terminate_process_bounded "$COMPOSITOR_PID"
  COMPOSITOR_PID=""
  find "$RUNTIME_DIR" -mindepth 1 -maxdepth 1 -type s -delete
}

wait_for_actor() {
  local log="$1"
  local expected_actors="$2"
  for _ in $(seq 1 600); do
    if rg -q 'started presentation WASM actor id=render-proof .* pane=[0-9]+ ' "$log" \
      && rg -q "render scheduling actor-frame promoted actor=render-proof .* pane_count=2 panes=\[[^]]+\]" "$log" \
      && [[ "$(rg -c 'started presentation WASM actor id=render-' "$log" 2> /dev/null || true)" -ge "$expected_actors" ]]; then
      return
    fi
    kill -0 "$CLIENT_PID" 2> /dev/null || break
    sleep 0.025
  done
  echo "ERR render-proof actor did not expose two rendered panes" >&2
  return 1
}

wait_for_saturated_layout() {
  local log="$1"
  for _ in $(seq 1 600); do
    if rg -q 'accepted synchronized layout .* wait=6 affected=6 .* target_rects=\[1:960:0:320:720,18446744073709551611:0:0:320:360,18446744073709551612:320:0:320:360,18446744073709551613:640:0:320:360,18446744073709551614:0:360:480:360,18446744073709551615:480:360:480:360\]' "$log" \
      && rg -q 'finalized native window resize .* epoch=1 size=1280x720 ' "$log" \
      && rg -q '^presentation layout source changed source=actor$' "$log"; then
      return
    fi
    kill -0 "$CLIENT_PID" 2> /dev/null || break
    sleep 0.025
  done
  echo "ERR saturated client did not settle the exact six-pane layout" >&2
  return 1
}

toggle_actor_subscription() {
  local log="$1"
  local actor="$2"
  local pane="$3"
  local x="$4"
  local y="$5"
  local require_promotion="$6"
  local first_new_line
  first_new_line=$(($(wc -l < "$log") + 1))
  wayland_env "$OUT/virtual-pointer-input" click "$x" "$y" 1280 720
  wayland_env "$OUT/virtual-keyboard-key" --plain 66
  for _ in $(seq 1 600); do
    if awk -v first="$first_new_line" -v actor="$actor" -v pane="$pane" \
      -v require_promotion="$require_promotion" '
        NR < first { next }
        index($0, "focused local plugin pane=" pane) { focused = 1 }
        focused && /plugin keyboard input logical="F8" .* state=Pressed/ { keyed = 1 }
        keyed && index($0, "render scheduling actor-frame promoted actor=" actor " ") {
          promoted = 1
        }
        END { exit !(focused && keyed && (!require_promotion || promoted)) }
      ' "$log"; then
      return
    fi
    kill -0 "$CLIENT_PID" 2> /dev/null || break
    sleep 0.025
  done
  echo "ERR subscription toggle lacked actor-specific evidence actor=$actor pane=$pane" >&2
  return 1
}

exercise_smithay_driver_after_reset() {
  local log="$1"
  local first_new_line
  local status=""
  first_new_line=$(($(wc -l < "$log") + 1))
  # The exact settled six-pane layout assigns the full-height right column to
  # the native foot window. Real terminal text forces a fresh managed commit,
  # DMA-BUF import, compositor frame, and separated Smithay driver call after
  # the metrics reset.
  wayland_env "$OUT/virtual-pointer-input" click 1120 360 1280 720
  wayland_env "$OUT/virtual-keyboard-key" --text 'smithay-driver-proof'
  for _ in $(seq 1 600); do
    status="$(ctl status-json 2> /dev/null || true)"
    if awk -v first="$first_new_line" '
      NR >= first && /^focused backend-managed window=1$/ { found = 1 }
      END { exit !found }
    ' "$log" \
      && jq -e '.smithay_driver_call_count > 0' <<< "$status" > /dev/null 2>&1; then
      break
    fi
    kill -0 "$CLIENT_PID" 2> /dev/null || break
    sleep 0.025
  done
  jq -e '.smithay_driver_call_count > 0' <<< "$status" > /dev/null 2>&1 || {
    echo "ERR saturated native workload did not exercise a post-reset Smithay driver call" >&2
    return 1
  }

  first_new_line=$(($(wc -l < "$log") + 1))
  wayland_env "$OUT/virtual-pointer-input" click 240 540 1280 720
  for _ in $(seq 1 600); do
    if awk -v first="$first_new_line" '
      NR >= first && /^focused local plugin pane=18446744073709551614$/ { found = 1 }
      END { exit !found }
    ' "$log"; then
      return
    fi
    kill -0 "$CLIENT_PID" 2> /dev/null || break
    sleep 0.025
  done
  echo "ERR saturated workload did not restore the intended plugin focus" >&2
  return 1
}

wait_for_renderer_recovery_and_actor_frame() {
  local log="$1"
  local first_new_line="$2"
  local recovery_line=""
  for _ in $(seq 1 600); do
    recovery_line="$(awk -v first="$first_new_line" '
      NR >= first && /^recovering renderer generation=[0-9]+ replacement=[0-9]+: render-scheduling F9 proof recovery$/ {
        print NR
        exit
      }
    ' "$log")"
    if [[ -n "$recovery_line" ]] && awk -v recovery="$recovery_line" '
      NR > recovery && /render scheduling actor-frame promoted actor=render-proof .* pane_count=2 panes=\[[^]]+\]/ {
        found = 1
        exit
      }
      END { exit !found }
    ' "$log"; then
      return
    fi
    kill -0 "$CLIENT_PID" 2> /dev/null || break
    sleep 0.025
  done
  echo "ERR renderer recovery did not produce a subsequent two-pane actor frame" >&2
  return 1
}

run_scenario() {
  local lane="$1"
  local refresh="$2"
  local saturated="$3"
  local log="$4"
  local settle_seconds="$5"
  local recovery_log_start
  local -a actor_args=(--wasm-actor "render-proof*2=$GRAPHICS_COMPONENT")
  if [[ "$saturated" == 1 ]]; then
    actor_args+=(
      --wasm-actor "render-cpu-a=$GRAPHICS_COMPONENT"
      --wasm-actor "render-cpu-b=$GRAPHICS_COMPONENT"
      --wasm-actor "render-cpu-c=$GRAPHICS_COMPONENT"
    )
  fi
  start_server "$lane" "$saturated"
  if [[ "$saturated" == 1 ]]; then
    wait_for_load_actors
    start_load
    wait_for_load_progress
  fi
  start_compositor "$lane" "$refresh"
  ACTIVE_CLIENT_LOG="$log"
  : > "$log"
  env -u brood_session -u BROOD_SURFACE_SESSION_ID -u BROOD_PANE_ID \
    XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$WAYLAND_SOCKET" \
    BROOD_RENDER_SCHEDULING_PROOF=1 \
    BROOD_RENDER_SCHEDULING_SCENARIO="$lane" \
    BROOD_TRACE_PLUGIN_GRAPHICS=1 BROOD_TRACE_PLUGIN_INPUT=1 \
    WGPU_BACKEND=vulkan BROOD_DMABUF_HAPPY_PATH=1 \
    RUST_LOG="${RUST_LOG:-brood_client=info,smithay=warn,wgpu_core=warn,wgpu_hal=warn}" \
    "$CLIENT_BIN" --no-init "${actor_args[@]}" \
    --dbus-destination "$DBUS_DESTINATION" \
    --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
    > "$log" 2>&1 &
  CLIENT_PID=$!
  if [[ "$saturated" == 1 ]]; then
    wait_for_actor "$log" 4
    wait_for_saturated_layout "$log"
  else
    wait_for_actor "$log" 1
  fi

  # Focus an actor-owned pane, start the subscription with a real F8 key,
  # then send 260 real press/release pairs while pacing is active.
  if [[ "$saturated" == 1 ]]; then
    # The settled layout has the managed pane plus five plugin panes. Toggle
    # exactly one pane for each distinct actor and prove the routed key caused
    # that actor to produce a later subscribed frame.
    toggle_actor_subscription "$log" render-cpu-c 18446744073709551611 160 180 1
    toggle_actor_subscription "$log" render-cpu-b 18446744073709551612 480 180 1
    toggle_actor_subscription "$log" render-cpu-a 18446744073709551613 800 180 1
    toggle_actor_subscription "$log" render-proof 18446744073709551614 240 540 1
  else
    wayland_env "$OUT/virtual-pointer-input" click 320 360 1280 720
    wayland_env "$OUT/virtual-keyboard-key" --plain 66
  fi
  # Key correlations and Smithay callback distributions begin only after the
  # client's exact 60-presentation warm-up boundary.
  for _ in $(seq 1 400); do
    rg -q '^render scheduling proof warmup complete$' "$log" && break
    kill -0 "$CLIENT_PID" 2> /dev/null || break
    sleep 0.025
  done
  rg -q '^render scheduling proof warmup complete$' "$log" || {
    echo "ERR $lane client did not report the warm-up boundary" >&2
    return 1
  }
  if [[ "$saturated" == 1 ]]; then
    verify_load_workers_live
  fi
  ctl render-scheduling-metrics-reset > /dev/null
  if [[ "$saturated" == 1 ]]; then
    exercise_smithay_driver_after_reset "$log"
  fi
  # One uninterrupted 1 ms stream exercises deadline cutoff while input is
  # continuously readable. All 260 presses begin after the scheduler-owned
  # presentation warm-up marker and therefore belong to the measured set.
  local key_stream
  key_stream='abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz'
  [[ "${#key_stream}" -eq 260 ]]
  wayland_env "$OUT/virtual-keyboard-key" --text "$key_stream"

  if [[ "$lane" == unsaturated_60hz ]]; then
    sleep 2
    capture_screen "$OUT/captures/$lane-visible-before.png" bilateral-full
    [[ -s "$OUT/captures/$lane-visible-before.png" ]]
    # Recovery is deliberately triggered only after the live pre-recovery
    # capture. Linux input code 67 is F9; the proof hook replaces the renderer.
    # Require both that new recovery and a later atomic two-pane actor frame so
    # the recovery capture cannot be satisfied by startup or retained logs.
    recovery_log_start=$(($(wc -l < "$log") + 1))
    wayland_env "$OUT/virtual-keyboard-key" --plain 67
    wait_for_renderer_recovery_and_actor_frame "$log" "$recovery_log_start"
    sleep 0.5
    capture_screen "$OUT/captures/$lane-recovered.png" bilateral-full
    [[ -s "$OUT/captures/$lane-recovered.png" ]]
    # Proof-only F10 routes through the client's ordinary layout eligibility
    # path, hiding one owned pane without hiding the OS window.
    wayland_env "$OUT/virtual-keyboard-key" --plain 68
    sleep 0.5
    capture_screen "$OUT/captures/$lane-one-pane-hidden.png" left-full-right-hidden
    [[ -s "$OUT/captures/$lane-one-pane-hidden.png" ]]
    wayland_env "$OUT/virtual-keyboard-key" --plain 68
    sleep 0.5
    capture_screen "$OUT/captures/$lane-visible-after.png" bilateral-full
    [[ -s "$OUT/captures/$lane-visible-after.png" ]]
  fi
  sleep "$settle_seconds"
  capture_screen "$OUT/captures/$lane.png" bilateral-full
  [[ -s "$OUT/captures/$lane.png" ]]

  # Disable subscription, allow all requests to settle, then measure a full
  # five-second idle window with no synthetic input or polling.
  if [[ "$saturated" == 1 ]]; then
    toggle_actor_subscription "$log" render-cpu-c 18446744073709551611 160 180 0
    toggle_actor_subscription "$log" render-cpu-b 18446744073709551612 480 180 0
    toggle_actor_subscription "$log" render-cpu-a 18446744073709551613 800 180 0
    toggle_actor_subscription "$log" render-proof 18446744073709551614 240 540 0
  else
    wayland_env "$OUT/virtual-keyboard-key" --plain 66
  fi
  sleep 0.5
  # Linux input code 88 is F12. The proof hook snapshots counters here.
  wayland_env "$OUT/virtual-keyboard-key" --plain 88
  sleep 5.5
  # F11 closes the measurement interval before frontend shutdown can add
  # lifecycle work to the idle deltas.
  wayland_env "$OUT/virtual-keyboard-key" --plain 87
  sleep 0.1
  ctl close-frontend > /dev/null
  for _ in $(seq 1 300); do
    ! kill -0 "$CLIENT_PID" 2> /dev/null && break
    sleep 0.05
  done
  wait "$CLIENT_PID"
  CLIENT_PID=""
  rg -q '^render scheduling diagnostics [{]' "$log" || {
    echo "ERR $lane client omitted required 'render scheduling diagnostics' JSON" >&2
    return 1
  }
  if [[ "$saturated" == 1 ]]; then
    ctl status-json > "$SERVER_STATUS"
  fi
  stop_compositor
  stop_server
}

run_scenario unsaturated_60hz 60 0 "$UNSATURATED_LOG" 12
run_scenario saturated_60hz 60 1 "$SATURATED_LOG" 12
run_scenario direct_120hz 120 0 "$DIRECT_LOG" 6.5

tools/assemble-render-scheduling-report.py \
  --deterministic-log "$DETERMINISTIC_LOG" \
  --unsaturated-log "$UNSATURATED_LOG" \
  --saturated-log "$SATURATED_LOG" \
  --direct-log "$DIRECT_LOG" \
  --server-status "$SERVER_STATUS" \
  --client-binary "$CLIENT_BIN" \
  --component "$GRAPHICS_COMPONENT" \
  --artifact-directory "$OUT" \
  --output "$REPORT"
tools/analyze-render-scheduling.py "$REPORT"

echo "render_scheduling_report=$REPORT"
echo "render_scheduling_captures=$OUT/captures"
echo "render_scheduling_nested=verified"
