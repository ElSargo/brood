#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel 2> /dev/null || pwd -P)"
repo_root="$(realpath "$repo_root")"
repo_name="$(basename "$repo_root")"
repo_parent="$(dirname "$repo_root")"
snapshot_root="${CODEX_BTRFS_SNAPSHOT_ROOT:-$repo_parent/.snapshots/$repo_name}"
created_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
snapshot_stamp="$(date -u +%Y%m%dT%H%M%SZ)"
snapshot_id="pre-prompt-$snapshot_stamp-$$"
snapshot_path="$snapshot_root/$snapshot_id"
manifest_path="$snapshot_root/manifest.jsonl"
hook_input_path="$(mktemp)"

cleanup() {
  rm -f "$hook_input_path"
}
trap cleanup EXIT

cat > "$hook_input_path" || true

skip_or_fail() {
  local message="$1"
  if [[ "${CODEX_BTRFS_SNAPSHOT_ALLOW_SKIP:-}" == "1" ]]; then
    echo "Codex Btrfs snapshot skipped: $message" >&2
    exit 0
  fi
  echo "Codex Btrfs snapshot failed: $message" >&2
  echo "Set CODEX_BTRFS_SNAPSHOT_ALLOW_SKIP=1 to make this hook non-blocking." >&2
  exit 1
}

command -v btrfs > /dev/null 2>&1 || skip_or_fail "btrfs command is not available"
btrfs subvolume show "$repo_root" > /dev/null 2>&1 ||
  skip_or_fail "$repo_root is not a Btrfs subvolume"

mkdir -p "$snapshot_root"
btrfs subvolume snapshot -r "$repo_root" "$snapshot_path" > /dev/null

SNAPSHOT_ID="$snapshot_id" \
  SNAPSHOT_PATH="$snapshot_path" \
  CREATED_AT="$created_at" \
  REPO_ROOT="$repo_root" \
  HOOK_INPUT_PATH="$hook_input_path" \
  python3 - << 'PY' >> "$manifest_path"
import json
import os
from pathlib import Path

hook_input_path = Path(os.environ["HOOK_INPUT_PATH"])
try:
    hook_input = json.loads(hook_input_path.read_text())
except Exception:
    hook_input = None

entry = {
    "snapshot": os.environ["SNAPSHOT_ID"],
    "path": os.environ["SNAPSHOT_PATH"],
    "createdAt": os.environ["CREATED_AT"],
    "cwd": os.environ["REPO_ROOT"],
    "source": "codex-user-prompt-submit",
}

if isinstance(hook_input, dict):
    prompt = hook_input.get("prompt") or hook_input.get("userPrompt")
    if isinstance(prompt, str):
        entry["prompt"] = prompt

print(json.dumps(entry, separators=(",", ":")))
PY

echo "Created Btrfs snapshot $snapshot_path" >&2
