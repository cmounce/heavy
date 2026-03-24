#!/usr/bin/env bash
# Run Heavy in front of a Python server for quick smoke testing
set -euo pipefail

UPSTREAM_PORT=3011
PROXY_PORT=8011

# Make sure that on exit (Ctrl-C, etc) we kill the processes we started
cleanup() {
    echo ""
    echo "Shutting down..."
    kill "$PROXY_PID" "$UPSTREAM_PID" 2>/dev/null || true
    wait "$PROXY_PID" "$UPSTREAM_PID" 2>/dev/null || true
}
trap cleanup EXIT

# Start a Python HTTP server as the upstream
echo "Starting upstream on :$UPSTREAM_PORT ..."
python3 -m http.server "$UPSTREAM_PORT" --directory "$(pwd)" &
UPSTREAM_PID=$!

# Build and start the proxy
echo "Starting proxy on :$PROXY_PORT -> :$UPSTREAM_PORT ..."
BIND="0.0.0.0:$PROXY_PORT" TARGET="http://localhost:$UPSTREAM_PORT" cargo run --quiet &
PROXY_PID=$!

echo ""
echo "Proxy is running. Try: curl http://localhost:$PROXY_PORT/"
echo "Press Ctrl-C to stop."
wait
