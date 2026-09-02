#!/bin/sh
# Inspect one HTTPS endpoint for outbound reachability: a quick proxy/stress
# diagnostic for provider connectivity before an eval run. Exits non-zero
# when the endpoint cannot be reached within the timeout. No repository
# behavior depends on this script; curl must be installed.
set -eu

endpoint="${1:-https://api.github.com}"
timeout="${2:-10}"

if ! command -v curl >/dev/null 2>&1; then
    echo "inspect_outbound: curl is required" >&2
    exit 2
fi

code="$(curl -sS -o /dev/null -w '%{http_code}' --max-time "$timeout" -I "$endpoint" 2>/dev/null || true)"
if [ -z "$code" ] || [ "$code" = "000" ]; then
    echo "inspect_outbound: cannot reach $endpoint within ${timeout}s" >&2
    exit 1
fi

echo "inspect_outbound: $endpoint -> HTTP $code"