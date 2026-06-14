#!/usr/bin/env bash

describe_host_presentation_environment() {
  local wayland_socket=""
  local display_number=""
  local path
  local listed_dirs=" "

  echo "host_display_diagnostic=begin" >&2
  echo "WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-unset}" >&2
  echo "XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-unset}" >&2
  echo "DISPLAY=${DISPLAY:-unset}" >&2
  echo "WSL2_GUI_APPS_ENABLED=${WSL2_GUI_APPS_ENABLED:-unset}" >&2

  if [[ -n "${XDG_RUNTIME_DIR:-}" && -n "${WAYLAND_DISPLAY:-}" ]]; then
    wayland_socket="$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY"
    if [[ -S "$wayland_socket" ]]; then
      echo "wayland_socket=$wayland_socket socket=yes" >&2
    elif [[ -e "$wayland_socket" ]]; then
      echo "wayland_socket=$wayland_socket socket=no exists=yes" >&2
    else
      echo "wayland_socket=$wayland_socket socket=no exists=no" >&2
    fi
  else
    echo "wayland_socket=unknown socket=no reason=missing_env" >&2
  fi

  if [[ "${DISPLAY:-}" =~ ^:([0-9]+) ]]; then
    display_number="${BASH_REMATCH[1]}"
    for path in "/tmp/.X11-unix/X$display_number" "/mnt/wslg/.X11-unix/X$display_number"; do
      if [[ -S "$path" ]]; then
        echo "x11_socket=$path socket=yes" >&2
      elif [[ -e "$path" ]]; then
        echo "x11_socket=$path socket=no exists=yes" >&2
      else
        echo "x11_socket=$path socket=no exists=no" >&2
      fi
    done
  else
    echo "x11_socket=unknown socket=no reason=display_unset_or_unsupported" >&2
  fi

  for path in "${XDG_RUNTIME_DIR:-}" /mnt/wslg/runtime-dir /tmp/.X11-unix /mnt/wslg/.X11-unix; do
    if [[ -z "$path" || "$listed_dirs" == *" $path "* ]]; then
      continue
    fi
    listed_dirs="${listed_dirs}${path} "
    if [[ -d "$path" ]]; then
      echo "dir_listing=$path" >&2
      ls -la "$path" >&2 || true
    else
      echo "dir_listing=$path missing" >&2
    fi
  done

  if [[ -r /mnt/wslg/weston.log ]]; then
    echo "wslg_weston_log_tail:" >&2
    tail -40 /mnt/wslg/weston.log >&2 || true
  else
    echo "wslg_weston_log_tail=unavailable" >&2
  fi
  echo "host_display_diagnostic=end" >&2
}

require_host_presentation_environment() {
  if [[ "${MANIFOLD_SKIP_HOST_PREFLIGHT:-}" == "1" ]]; then
    return 0
  fi

  local wayland_socket=""
  if [[ -n "${XDG_RUNTIME_DIR:-}" && -n "${WAYLAND_DISPLAY:-}" ]]; then
    wayland_socket="$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY"
    if [[ -S "$wayland_socket" ]]; then
      return 0
    fi
  fi

  local display_number=""
  local x_socket=""
  if [[ "${DISPLAY:-}" =~ ^:([0-9]+) ]]; then
    display_number="${BASH_REMATCH[1]}"
    for x_socket in "/tmp/.X11-unix/X$display_number" "/mnt/wslg/.X11-unix/X$display_number"; do
      if [[ -S "$x_socket" ]]; then
        return 0
      fi
    done
  fi

  echo "ERR no host display socket is available for the Winit presentation frontend" >&2
  describe_host_presentation_environment
  return 1
}

wait_for_manifoldd_control() {
  local sock="$1"
  local pid="$2"
  local log="$3"

  for _ in $(seq 1 120); do
    if ! kill -0 "$pid" 2>/dev/null; then
      echo "ERR manifoldd exited before control socket became responsive" >&2
      tail -120 "$log" >&2 || true
      return 1
    fi
    if [[ -S "$sock" ]] &&
      target/debug/manifoldd ctl --control-socket "$sock" status >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.1
  done

  echo "ERR control socket did not become responsive" >&2
  tail -120 "$log" >&2 || true
  return 1
}
