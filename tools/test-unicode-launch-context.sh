#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug/manifold-runner-exec"
OUT="${MANIFOLD_UNICODE_LAUNCH_OUT:-$(mktemp -d /tmp/manifold-unicode-launch.XXXXXX)}"
RUNTIME_DIR="${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR is required}"
ID="p${BASHPID}t$(date +%s%N)"
RECORD="$RUNTIME_DIR/manifold/launches/$ID.json"
PROJECT="$OUT/プロジェクト-👩🏽‍💻"
WORKING="$PROJECT/étude-🇳🇿"
CACHE="$OUT/кэш-🧑🏽‍🚀"
ARGUMENT="参数-é-👍🏽-🇯🇵"
WORKLOAD="$OUT/record-unicode-context.sh"
ARTIFACT="$OUT/unicode-launch-context.md"

cleanup() {
  rm -f "$RECORD"
}
trap cleanup EXIT

rm -rf "$OUT"
mkdir -p "$WORKING" "$CACHE" "$(dirname "$RECORD")"

cat >"$WORKLOAD" <<'WORKLOAD'
#!/usr/bin/env bash
set -euo pipefail
{
  echo '# Unicode launch context proof'
  echo
  echo 'The actual runner sidecar restored these launch-record values without previewing or truncating them:'
  echo
  printf -- '- project: `%s`\n' "$MANIFOLD_SESSION_PROJECT_DIR"
  printf -- '- working: `%s`\n' "$MANIFOLD_SESSION_WORKING_DIR"
  printf -- '- cache: `%s`\n' "$MANIFOLD_SESSION_CACHE_DIR"
  printf -- '- argv: `%s`\n' "$1"
  printf -- '- process cwd: `%s`\n' "$PWD"
} >"$MANIFOLD_UNICODE_ARTIFACT"
WORKLOAD
chmod +x "$WORKLOAD"

ID="$ID" PROJECT="$PROJECT" WORKING="$WORKING" CACHE="$CACHE" \
  WORKLOAD="$WORKLOAD" ARGUMENT="$ARGUMENT" \
  nix develop --command python3 - "$RECORD" <<'PY'
import json
import os
import sys

record = {
    "id": os.environ["ID"],
    "project_dir": os.environ["PROJECT"],
    "working_dir": os.environ["WORKING"],
    "cache_dir": os.environ["CACHE"],
    "runner_cache_root": None,
    "runner_kind": "proof",
    "program_argv": [
        {"Unix": list(os.fsencode(os.environ["WORKLOAD"]))},
        {"Unix": list(os.fsencode(os.environ["ARGUMENT"]))},
    ],
    "session_args": None,
}
with open(sys.argv[1], "w", encoding="utf-8") as output:
    json.dump(record, output, ensure_ascii=False, indent=2)
PY

nix develop --command cargo build --quiet -p manifoldd --bin manifold-runner-exec

MANIFOLD_UNICODE_ARTIFACT="$ARTIFACT" \
  systemd-run --user --scope --quiet --collect --unit "manifold-launch-$ID.scope" "$BIN"

test -s "$ARTIFACT"
grep -Fq -- "- project: \`$PROJECT\`" "$ARTIFACT"
grep -Fq -- "- working: \`$WORKING\`" "$ARTIFACT"
grep -Fq -- "- cache: \`$CACHE\`" "$ARTIFACT"
grep -Fq -- "- argv: \`$ARGUMENT\`" "$ARTIFACT"
grep -Fq -- "- process cwd: \`$WORKING\`" "$ARTIFACT"

cat "$ARTIFACT"
echo "unicode_launch_context=verified artifact=$ARTIFACT"
