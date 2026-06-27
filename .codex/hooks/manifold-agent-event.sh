#!/usr/bin/env bash
set -euo pipefail

event="${1:-user_prompt_submit}"
repo_root="$(git rev-parse --show-toplevel 2>/dev/null || pwd -P)"
repo_root="$(realpath "$repo_root")"
hook_input_path="$(mktemp)"
log_path="${MANIFOLD_AGENT_HOOK_LOG:-/tmp/manifold-agent-hook.log}"

log_hook() {
  printf '%s event=%s %s\n' "$(date +%H:%M:%S)" "$event" "$*" >>"$log_path" 2>/dev/null || true
}

cleanup() {
  rm -f "$hook_input_path"
}
trap cleanup EXIT
trap 'status=$?; log_hook "failed status=$status line=$LINENO pwd=$PWD path=$PATH destination=${MANIFOLDD_DBUS_DESTINATION:-} session=${MANIFOLD_SESSION:-${manifold_session:-}}"; exit "$status"' ERR

cat >"$hook_input_path" || true

if [[ -z "${MANIFOLD_SESSION:-${manifold_session:-}}" && -z "${MANIFOLDD_DBUS_DESTINATION:-}" ]]; then
  log_hook "skipped no-manifold-env pwd=$PWD path=$PATH"
  exit 0
fi

if [[ -x "$repo_root/target/debug/manifoldd" ]]; then
  mctl_cmd=("$repo_root/target/debug/manifoldd" "ctl")
elif command -v mctl >/dev/null 2>&1; then
  mctl_cmd=("$(command -v mctl)")
else
  log_hook "skipped no-control-command repo_root=$repo_root path=$PATH"
  exit 0
fi

agent_pid="${MANIFOLD_AGENT_PID:-$PPID}"
agent_identity="${MANIFOLD_AGENT_IDENTITY:-$agent_pid}"
agent_app_id="${MANIFOLD_AGENT_APP_ID:-}"
surface_session="${MANIFOLD_SURFACE_SESSION_ID:-1}"

event_payload="$(
  python3 - "$event" "$hook_input_path" <<'PY'
import json
import sys

event = sys.argv[1]
path = sys.argv[2]

try:
    raw = open(path, encoding="utf-8").read()
    data = json.loads(raw or "{}")
except Exception:
    raw = ""
    data = {}

titles = {
    "user_prompt_submit": ("Codex prompt", "running"),
    "pre_tool_use": ("Codex tool starting", "running"),
    "post_tool_use": ("Codex tool finished", "running"),
    "stop": ("Codex turn finished", "done"),
    "subagent_stop": ("Codex subagent finished", "done"),
}
title, status = titles.get(event, (f"Codex {event}", "running"))

def clean(value):
    return " ".join(str(value).split())

message = ""
if isinstance(data, dict):
    for key in ("prompt", "userPrompt", "tool_name", "toolName", "name", "command", "summary", "message"):
        value = data.get(key)
        if isinstance(value, str) and value.strip():
            message = clean(value)
            break

if not message and isinstance(data, dict):
    interesting = {
        key: data[key]
        for key in sorted(data)
        if key in {"event", "hook_event_name", "tool", "status", "cwd", "exit_code"}
    }
    if interesting:
        message = clean(json.dumps(interesting, sort_keys=True))

if not message:
    message = {
        "user_prompt_submit": "Prompt submitted",
        "pre_tool_use": "Tool call started",
        "post_tool_use": "Tool call finished",
        "stop": "Turn finished",
        "subagent_stop": "Subagent finished",
    }.get(event, event)

print(title, status, message[:180], sep="\t")
PY
)"
IFS=$'\t' read -r title status message <<<"$event_payload"

agent_id="$(
  "${mctl_cmd[@]}" agents register codex \
    cwd="$repo_root" \
    source=codex-hook \
    pid="$agent_identity" \
    app_id="$agent_app_id" \
    terminal="${TERM_PROGRAM:-${TERM:-unknown}}" \
    surface_session="$surface_session" 2>/dev/null |
    sed -n 's/^agent id=//p' |
    tail -n 1
)" || exit 0

[[ -n "$agent_id" ]] || exit 0

status_metadata=(
  cwd="$repo_root"
  source=codex-hook
  pid="$agent_identity"
  app_id="$agent_app_id"
  last_event="$event"
)
if [[ "$event" == "user_prompt_submit" && -n "$message" ]]; then
  status_metadata+=(first_user_message="$message")
fi

"${mctl_cmd[@]}" agents status "$agent_id" "$status" \
  "${status_metadata[@]}" >/dev/null 2>&1 || true

printf '{}\n'

"${mctl_cmd[@]}" agents notify "$agent_id" "$title" "$message" >/dev/null 2>&1 || true

"${mctl_cmd[@]}" agents status "$agent_id" "$status" \
  "${status_metadata[@]}" >/dev/null 2>&1 || true
