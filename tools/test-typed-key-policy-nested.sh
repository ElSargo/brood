#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Typed key-policy acceptance now belongs to the presentation client that owns
# local plugin panes and its managed-window inventory. Keep the historical test
# entry point, but run the complete client-realm workload instead of maintaining
# a second daemon-realm harness with different nesting semantics.
exec "$ROOT/tools/test-session-switcher-plugin-nested.sh" "$@"
