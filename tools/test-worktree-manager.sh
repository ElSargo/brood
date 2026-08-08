#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]]; then
  exec dbus-run-session -- "$0" "$@"
fi

for tool in busctl cargo git python3; do
  command -v "$tool" > /dev/null 2>&1 || {
    echo "ERR $tool is required for the worktree-manager proof" >&2
    exit 2
  }
done

OUT="${BROOD_WORKTREE_MANAGER_OUT:-$(mktemp -d /tmp/brood-worktree-manager.XXXXXX)}"
PACKAGE="$OUT/package"
PROJECT="$OUT/project"
DATA_HOME="$OUT/data"
CONFIG="$OUT/brood.toml"
SERVER_LOG="$OUT/server.log"
DBUS_DESTINATION="org.brood.WorktreeManagerTest.p$$"
mkdir -p "$PACKAGE" "$PROJECT" "$DATA_HOME"

SERVER_PID=""
cleanup() {
  local result=$?
  if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2> /dev/null; then
    kill "$SERVER_PID" > /dev/null 2>&1 || true
    wait "$SERVER_PID" > /dev/null 2>&1 || true
  fi
  if ((result != 0)); then
    echo "server_log=$SERVER_LOG" >&2
    tail -n 160 "$SERVER_LOG" >&2 || true
  fi
  return "$result"
}
trap cleanup EXIT

git -C "$PROJECT" init --quiet
git -C "$PROJECT" -c user.name=Brood -c user.email=brood@example.invalid \
  commit --quiet --allow-empty -m initial
git -C "$PROJECT" branch managed-retry

cargo build -p broodd --bins --locked
cargo build \
  --manifest-path plugins/worktree-manager/Cargo.toml \
  --target wasm32-unknown-unknown \
  --release \
  --locked
cargo run --quiet -p brood-wasm --example componentize -- \
  plugins/worktree-manager/target/wasm32-unknown-unknown/release/brood_worktree_manager_plugin.wasm \
  "$PACKAGE/brood_worktree_manager_plugin.component.wasm"
printf '[init]\nbackend = "%s"\n' \
  "$PACKAGE/brood_worktree_manager_plugin.component.wasm" \
  > "$CONFIG"

(
  cd "$PROJECT"
  env -u brood_session \
    XDG_DATA_HOME="$DATA_HOME" \
    RUST_LOG=broodd=info \
    "$ROOT/target/debug/brood-sessiond" \
    --headless \
    --no-default-modules \
    --config "$CONFIG" \
    --dbus-destination "$DBUS_DESTINATION" \
    --dbus-address "$DBUS_SESSION_BUS_ADDRESS"
) > "$SERVER_LOG" 2>&1 &
SERVER_PID=$!

ctl() {
  env -u brood_session target/debug/brood ctl \
    --dbus-destination "$DBUS_DESTINATION" \
    --dbus-address "$DBUS_SESSION_BUS_ADDRESS" \
    "$@"
}

for _ in $(seq 1 100); do
  ctl status > /dev/null 2>&1 && break
  kill -0 "$SERVER_PID" 2> /dev/null || {
    echo "ERR session server exited during startup" >&2
    exit 1
  }
  sleep 0.05
done
ctl status > /dev/null

create_worktree() {
  local source_session="$1"
  busctl \
    --address="$DBUS_SESSION_BUS_ADDRESS" \
    call \
    "$DBUS_DESTINATION" \
    /org/brood/Session \
    org.brood.Session \
    CreateManagedWorktreeSession \
    ts \
    "$source_session" \
    managed-retry \
    | awk '{print $2}'
}

SESSION_ID="$(create_worktree 1)"
[[ "$SESSION_ID" =~ ^[0-9]+$ ]]
REUSED_SESSION_ID="$(create_worktree "$SESSION_ID")"
[[ "$REUSED_SESSION_ID" == "$SESSION_ID" ]]

SESSION_JSON="$(ctl surface-session list-json)"
python3 - "$SESSION_JSON" "$SESSION_ID" "$DATA_HOME" <<'PY'
import json
import pathlib
import sys

catalog = json.loads(sys.argv[1])
session_id = int(sys.argv[2])
data_home = pathlib.Path(sys.argv[3]).resolve()
matches = [session for session in catalog["sessions"] if session["name"] == "managed-retry"]
assert len(matches) == 1
assert matches[0]["id"] == session_id
worktree = pathlib.Path(matches[0]["working_directory"]).resolve()
worktree.relative_to(data_home / "brood" / "worktrees")
assert matches[0]["branch"] == "managed-retry"
assert matches[0]["worktree"] is True
PY

WORKTREE_DIRECTORY="$(
  python3 -c 'import json,sys; print(next(s["working_directory"] for s in json.load(sys.stdin)["sessions"] if s["name"] == "managed-retry"))' \
    <<< "$SESSION_JSON"
)"
WORKTREE_PARENT="${WORKTREE_DIRECTORY%/*}"
WORKTREE_NAME="${WORKTREE_DIRECTORY##*/}"
WORKTREE_METADATA="$WORKTREE_PARENT/$WORKTREE_NAME.brood-worktree.json"
[[ "$(git -C "$WORKTREE_DIRECTORY" branch --show-current)" == "managed-retry" ]]
[[ -z "$(git -C "$WORKTREE_DIRECTORY" status --short)" ]]

ctl surface-session new primary > /dev/null
python3 - "$WORKTREE_METADATA" "$SESSION_ID" <<'PY'
import json
import pathlib
import sys

metadata = json.loads(pathlib.Path(sys.argv[1]).read_text())
assert metadata["session"] == int(sys.argv[2])
PY
ctl close-session "$SESSION_ID" > /dev/null
for _ in $(seq 1 100); do
  if python3 - "$WORKTREE_METADATA" <<'PY'
import json
import pathlib
import sys

metadata = json.loads(pathlib.Path(sys.argv[1]).read_text())
raise SystemExit(0 if metadata["session"] is None else 1)
PY
  then
    break
  fi
  sleep 0.05
done
python3 - "$WORKTREE_METADATA" "$WORKTREE_DIRECTORY" <<'PY'
import json
import pathlib
import sys

metadata = json.loads(pathlib.Path(sys.argv[1]).read_text())
assert metadata["session"] is None
assert pathlib.Path(metadata["path"]).resolve() == pathlib.Path(sys.argv[2]).resolve()
PY

rg -q "actor=brood.backend-init" "$SERVER_LOG"
[[ "$(rg -c "attached managed worktree session" "$SERVER_LOG")" == "2" ]]
cat > "$OUT/proof.md" <<EOF
# Worktree manager proof

- daemon actor: brood.backend-init
- first acquisition: session $SESSION_ID
- retry acquisition: reused session $REUSED_SESSION_ID
- metadata: $WORKTREE_METADATA
- checkout status: clean
- persistence: creating another session retained the durable worktree session
- released occupation: closing the managed session wrote session null
EOF

echo "worktree_manager=verified"
echo "proof=$OUT/proof.md"
