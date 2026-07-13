#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug/manifold"

ctl() {
  RUST_LOG=off timeout 8s "$BIN" ctl "$@"
}

wait_for_file() {
  local path="$1"
  local pattern="$2"
  local label="$3"
  for _ in $(seq 1 240); do
    if [[ -s "$path" ]] && grep -q "$pattern" "$path"; then
      return
    fi
    sleep 0.05
  done
  echo "timed out waiting for $label" >&2
  [[ -e "$path" ]] && cat "$path" >&2
  return 1
}

run_controller() {
  local out="$1"
  local realm_one="${MANIFOLD_SURFACE_SESSION_ID:?managed controller needs a surface session}"
  local create realm_two service_name ready other same token notification_id
  local dump="$out/inner-dump"
  local transcript="$out/session-bus-transcript.md"

  mkdir -p "$out/rube"
  printf '\033[2J\033[H'
  echo 'Typed session-bus routing proof'
  echo
  echo 'The real GTK session switcher is visible in Realm One.'
  echo 'Realm Two cannot see its name; Realm One observes its live owner.'
  echo 'Four managed DBus clients then exchange properties, methods, replies,'
  echo 'and signals before the realm notification proxy reaches the desktop.'

  create="$(ctl surface-session new 'DBus Realm Two')"
  realm_two="$(sed -n 's/.*active=\([0-9][0-9]*\).*/\1/p' <<<"$create")"
  [[ -n "$realm_two" && "$realm_two" != "$realm_one" ]]
  # Keep the second realm live when focus returns to Realm One. Its animated
  # EGL client is also a genuine graphical workload, not a synthetic bus stub.
  ctl spawn-in-session "$realm_two" weston-simple-egl -f >/dev/null
  for _ in $(seq 1 100); do
    if ctl --json '["surface-session","list-json"]' \
      | python3 -c 'import json,sys; p=json.load(sys.stdin); target=int(sys.argv[1]); raise SystemExit(0 if any(s["id"]==target and s["windows"] for s in p["sessions"]) else 1)' "$realm_two"; then
      break
    fi
    sleep 0.05
  done
  ctl surface-session switch "$realm_one" >/dev/null

  ctl spawn-in-session "$realm_one" env GSK_RENDERER=cairo GTK_A11Y=none \
    python3 "$ROOT/apps/manifold-session-switcher.py" \
    --ready-file "$out/gtk.ready" \
    --event-file "$out/gtk.events" \
    --refresh-ms 100 >/dev/null
  wait_for_file "$out/gtk.ready" '^ready=true$' 'GTK switcher registration'
  ready="$(cat "$out/gtk.ready")"

  service_name='org.manifold.Rube.A'
  ctl spawn-in-session "$realm_one" python3 "$ROOT/tools/dbus-rube-goldberg.py" \
    --role node --node A --run-dir "$out/rube" --timeout 60 >/dev/null
  wait_for_file "$out/rube/ready-A" '^ready=true$' 'Rube node A'

  ctl spawn-in-session "$realm_two" python3 "$ROOT/tools/dbus-session-client.py" \
    --service-name "$service_name" --owner-only \
    --out-file "$out/realm-two-client.txt" >/dev/null
  wait_for_file "$out/realm-two-client.txt" '^has_owner=false$' 'isolated realm miss'
  other="$(cat "$out/realm-two-client.txt")"

  ctl spawn-in-session "$realm_one" python3 "$ROOT/tools/dbus-session-client.py" \
    --service-name "$service_name" --owner-only \
    --out-file "$out/realm-one-client.txt" >/dev/null
  wait_for_file "$out/realm-one-client.txt" '^has_owner=true$' 'same-realm owner lookup'
  same="$(cat "$out/realm-one-client.txt")"

  for node in C D B; do
    ctl spawn-in-session "$realm_one" python3 "$ROOT/tools/dbus-rube-goldberg.py" \
      --role node --node "$node" --run-dir "$out/rube" --timeout 60 >/dev/null
    wait_for_file "$out/rube/ready-$node" '^ready=true$' "Rube node $node"
  done

  token="typed-routing-$$"
  ctl spawn-in-session "$realm_one" python3 "$ROOT/tools/dbus-rube-goldberg.py" \
    --role kick --run-dir "$out/rube" --token "$token" --value 41 >/dev/null
  wait_for_file "$out/rube/kick" '^introspected=A,B,C,D$' 'introspection and method kick'
  wait_for_file "$out/rube/done" '^notification_id=[0-9][0-9]*$' 'desktop notification reply'
  grep -q "A Kick token=$token value=41" "$out/rube/events.log"
  grep -q "B Started token=$token" "$out/rube/events.log"
  grep -q "C Transform token=$token input=42 output=49" "$out/rube/events.log"
  grep -q "D Transformed token=$token value=49" "$out/rube/events.log"
  notification_id="$(sed -n 's/^notification_id=//p' "$out/rube/done")"
  [[ "$notification_id" =~ ^[0-9]+$ ]]

  ctl debug dump "$dump" >/dev/null
  test -s "$dump/composed.png"
  test -s "$dump/state.json"

  {
    echo '# Typed session-bus nested proof'
    echo
    echo 'Rube node A owns a well-known name only in Realm One. The ordinary client in Realm Two sees no owner; the Realm One client sees the owner on a different in-process bus address. The rendered GTK switcher simultaneously shows both surface sessions to the user.'
    echo
    echo '```text'
    echo "realm_one=$realm_one"
    echo "realm_two=$realm_two"
    echo "$ready"
    echo '-- Realm Two lookup --'
    echo "$other"
    echo '-- Realm One lookup --'
    echo "$same"
    echo '```'
    echo
    echo "Four real managed DBus clients then introspect one another, round-trip a property, route a method return through B, and route two signals through the chain. Node D sends its notification over the ordinary managed session bus; the virtual notification owner proxies it to the host desktop service and returns that service's real notification id."
    echo
    echo '```text'
    cat "$out/rube/kick"
    cat "$out/rube/events.log"
    cat "$out/rube/done"
    echo '```'
    echo
    echo "Rendered GTK proof: \`$dump/composed.png\`"
  } >"$transcript"

  echo "typed_session_bus_controller=verified realm_one=$realm_one realm_two=$realm_two host_notification_id=$notification_id"
}

if [[ "${1:-}" == '--controller' ]]; then
  [[ $# -eq 2 ]]
  run_controller "$2"
  exit
fi

if [[ "${MANIFOLD_TYPED_SESSION_BUS_CONTROLLER:-0}" == '1' ]]; then
  run_controller "${MANIFOLD_TYPED_SESSION_BUS_OUT:?controller output directory required}"
  exit
fi

cd "$ROOT"
nix develop --command cargo build --quiet -p manifoldd --bins

OUT="${MANIFOLD_TYPED_SESSION_BUS_OUT:-$(mktemp -d /tmp/manifold-typed-session-bus.XXXXXX)}"
rm -rf "$OUT"
mkdir -p "$OUT"

export MANIFOLD_TYPED_SESSION_BUS_OUT="$OUT"
export MANIFOLD_TYPED_SESSION_BUS_CONTROLLER=1
timeout --kill-after=5s 150s nix develop --command "$BIN" test nested \
  --force-outer \
  --out-dir "$OUT/nested" \
  --wait-for-inner-command-exit \
  --inner-command "weston-terminal --shell=$ROOT/tools/test-typed-session-bus-nested.sh" \
  | tee "$OUT/nested-test.stdout"

grep -qx 'topology=outer-supervisor' "$OUT/nested-test.stdout"
grep -qx 'outer_presentation=offscreen' "$OUT/nested-test.stdout"
grep -q '^has_owner=false$' "$OUT/realm-two-client.txt"
grep -q '^has_owner=true$' "$OUT/realm-one-client.txt"
grep -q '^notification_id=[0-9][0-9]*$' "$OUT/rube/done"
grep -q '^# Typed session-bus nested proof$' "$OUT/session-bus-transcript.md"
test -s "$OUT/inner-dump/composed.png"
test -s "$OUT/nested/outer-dump/composed.png"

echo "typed_session_bus_nested=verified transcript=$OUT/session-bus-transcript.md screenshot=$OUT/inner-dump/composed.png"
