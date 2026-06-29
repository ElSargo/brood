#!/usr/bin/env bash
set -euo pipefail

timeout_ms="${MANIFOLD_NOTIFY_SEND_SMOKE_TIMEOUT_MS:-300000}"
notify-send -t "$timeout_ms" ManifoldNotifySmoke

echo "OK notify-send forwarding smoke passed"
