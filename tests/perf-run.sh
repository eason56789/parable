#!/usr/bin/env bash
# Run the performance probes against an ISOLATED harness home.
#
# The probes create sessions (a chat POST always starts a new session). Running
# them against the real ~/.parable would litter the user's conversation list —
# and the SSE-lag probe creates thousands of them. This wrapper points
# PARABLE_HOME at a scratch directory so every run is disposable.
#
# Usage:
#   tests/perf-run.sh <probe.mjs> [args...]
#   tests/perf-run.sh --all
#
# Examples:
#   tests/perf-run.sh tests/perf-load.mjs http://127.0.0.1:65305 perftest123
#   tests/perf-run.sh --all

set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SCRATCH="${TMPDIR:-/tmp}/parable-perf-home.$$"
mkdir -p "$SCRATCH"
PORT="${PERF_PORT:-65400}"
TOKEN="perftest123"

cleanup() { rm -rf "$SCRATCH"; }
trap cleanup EXIT

export PARABLE_HOME="$SCRATCH"

start_server() {
  "$ROOT/target/release/parable.exe" --port "$PORT" --token "$TOKEN" \
    > "$SCRATCH/server.log" 2>&1 &
  SERVER_PID=$!
  for _ in $(seq 1 40); do
    if curl -s -o /dev/null -m 2 "http://127.0.0.1:$PORT/api/status"; then return 0; fi
    sleep 0.25
  done
  echo "server failed to start; log:" >&2
  cat "$SCRATCH/server.log" >&2
  return 1
}

stop_server() {
  [ -n "${SERVER_PID:-}" ] && kill "$SERVER_PID" 2>/dev/null
  wait "$SERVER_PID" 2>/dev/null
  SERVER_PID=""
}

run_probe() {
  local probe="$1"; shift
  echo "--- $probe $*"
  node "$ROOT/$probe" "$@"
}

if [ "${1:-}" = "--all" ]; then
  start_server || exit 1
  run_probe tests/perf-load.mjs "http://127.0.0.1:$PORT" "$TOKEN"
  run_probe tests/perf-memory.mjs "$PORT" "$TOKEN" "$(pgrep -f "parable.exe --port $PORT" | head -1)"
  stop_server
  exit 0
fi

PROBE="$1"; shift
start_server || exit 1
run_probe "$PROBE" "$@" "http://127.0.0.1:$PORT" "$TOKEN"
stop_server
