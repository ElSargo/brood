#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TMPDIR="$(mktemp -d /tmp/manifold-audit-strict-smoke.XXXXXX)"
LOG="$TMPDIR/audit-strict.log"

set +e
MANIFOLD_AUDIT_REQUIRE_NO_SKIPS=1 ./tools/audit-v1-capabilities.sh > "$LOG" 2>&1
RC=$?
set -e

OVERALL="$(grep '^overall=' "$LOG" | tail -1 || true)"
SKIPS="$(sed -n 's/.* skips=\([0-9][0-9]*\).*/\1/p' <<< "$OVERALL")"
SKIPS="${SKIPS:-0}"

case "$OVERALL" in
  overall=pass\ failures=0\ skips=0*)
    if [[ "$RC" != "0" ]]; then
      echo "ERR strict audit reported pass but exited rc=$RC" >&2
      tail -80 "$LOG" >&2 || true
      exit 1
    fi
    echo "OK strict audit passed with no capability skips"
    ;;
  overall=fail_capability_skips_disallowed*)
    if [[ "$RC" == "0" ]]; then
      echo "ERR strict audit disallowed skips but exited rc=0" >&2
      tail -80 "$LOG" >&2 || true
      exit 1
    fi
    if ((SKIPS < 1)); then
      echo "ERR strict audit reported skips disallowed without skips: $OVERALL" >&2
      tail -80 "$LOG" >&2 || true
      exit 1
    fi
    echo "OK strict audit rejected capability skips"
    ;;
  overall=blocked_host_display_unavailable*)
    echo "SKIP strict audit host display unavailable"
    echo "log=$LOG"
    exit 77
    ;;
  overall=fail*)
    echo "ERR strict audit failed for reasons other than capability skips: $OVERALL" >&2
    tail -120 "$LOG" >&2 || true
    exit 1
    ;;
  *)
    echo "ERR unexpected strict audit result rc=$RC overall=${OVERALL:-none}" >&2
    tail -120 "$LOG" >&2 || true
    exit 1
    ;;
esac

echo "audit_result=$OVERALL"
echo "rc=$RC"
echo "log=$LOG"
